use crate::governor::PerformanceMode;
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy)]
pub struct LoadBands {
    pub upper: f32,
    pub medium: f32,
    pub slow: f32,
    pub crawl: f32,
    pub lower: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct RampStep {
    pub burst: f32,
    pub up: f32,
    pub up_medium: f32,
    pub up_slow: f32,
    pub up_crawl: f32,
    pub down: f32,
}

/// Share of recent samples that saw the GPU busy.
/// An empty history or a zero window is idle, so a bad config cannot produce NaN.
pub fn busy_ratio(history: &VecDeque<bool>, window: usize) -> f32 {
    if history.is_empty() || window == 0 {
        return 0.0;
    }
    if history.len() >= window {
        let count = history
            .iter()
            .rev()
            .take(window)
            .filter(|&&busy| busy)
            .count();
        count as f32 / window as f32
    } else {
        let count = history.iter().filter(|&&busy| busy).count();
        count as f32 / history.len() as f32
    }
}

/// True when the newest `burst_samples` are all busy.
pub fn is_burst(history: &VecDeque<bool>, burst_samples: usize) -> bool {
    burst_samples > 0
        && history.len() >= burst_samples
        && history.iter().rev().take(burst_samples).all(|&busy| busy)
}

/// One sample of the frequency target. Rates are MHz per millisecond.
pub fn step_target(
    target: f32,
    max_performance: bool,
    burst: bool,
    busy_up: f32,
    busy_down: f32,
    rates: RampStep,
    bands: LoadBands,
    delta_ms: f32,
    min_freq: u16,
    max_freq: u16,
) -> f32 {
    let mut next = if max_performance {
        f32::from(max_freq)
    } else if burst {
        target + rates.burst * delta_ms
    } else if busy_up > bands.upper {
        target + rates.up * delta_ms
    } else if busy_up > bands.medium {
        target + rates.up_medium * delta_ms
    } else if busy_up > bands.slow {
        target + rates.up_slow * delta_ms
    } else if busy_up > bands.crawl {
        target + rates.up_crawl * delta_ms
    } else if busy_down < bands.lower {
        target - rates.down * delta_ms
    } else {
        target
    };
    next = next.clamp(f32::from(min_freq), f32::from(max_freq));
    next
}

/// Whether this sample should send a clock write.
pub fn should_request_clock(
    pending: bool,
    skipping: bool,
    burst: bool,
    adjust_due: bool,
    finetune_due: bool,
    diff: u16,
    adjust_threshold: u16,
    finetune_threshold: u16,
) -> bool {
    !skipping
        && !pending
        && (burst
            || (adjust_due && diff >= adjust_threshold)
            || (finetune_due && diff >= finetune_threshold))
}

/// Voltage for `freq` from the safe-point table. `None` when the table is empty.
pub fn interpolate_voltage(freq: u16, safe_points: &BTreeMap<u16, u16>) -> Option<u16> {
    let (Some((&first_freq, &first_vol)), Some((&last_freq, &last_vol))) =
        (safe_points.first_key_value(), safe_points.last_key_value())
    else {
        return None;
    };
    if freq <= first_freq {
        return Some(first_vol);
    }
    if freq >= last_freq {
        return Some(last_vol);
    }

    let (&f1, &v1) = safe_points
        .range(..=freq)
        .next_back()
        .expect("a safe-point at or below this frequency exists");
    let (&f2, &v2) = safe_points
        .range(freq..)
        .next()
        .expect("a safe-point at or above this frequency exists");
    if f1 == f2 {
        return Some(v1);
    }
    let ratio = (freq - f1) as f32 / (f2 - f1) as f32;
    let interpolated = v1 as f32 + ratio * (v2 as f32 - v1 as f32);
    Some(interpolated.round() as u16)
}

/// Parses the content of the performance control file.
/// - If empty, whitespace, or "max": locks to `max_freq` (`PerformanceMode::Fixed(max_freq)`).
/// - If one integer: locks to that frequency clamped between `min_freq` and `max_freq`.
/// - If two integers: sets a dynamic range `[min, max]` clamped to `[min_freq, max_freq]`.
/// - Any other input defaults to `PerformanceMode::Fixed(max_freq)`.
pub fn parse_performance_control(content: &str, min_freq: u16, max_freq: u16) -> PerformanceMode {
    let trimmed = content.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("max") {
        return PerformanceMode::Fixed(max_freq);
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() == 1 {
        if let Ok(freq) = tokens[0].parse::<u16>() {
            return PerformanceMode::Fixed(freq.clamp(min_freq, max_freq));
        }
    } else if tokens.len() == 2 {
        if let (Ok(f1), Ok(f2)) = (tokens[0].parse::<u16>(), tokens[1].parse::<u16>()) {
            let lower = f1.min(f2).clamp(min_freq, max_freq);
            let upper = f1.max(f2).clamp(min_freq, max_freq);
            if lower == upper {
                return PerformanceMode::Fixed(lower);
            }
            return PerformanceMode::Range {
                min: lower,
                max: upper,
            };
        }
    }

    PerformanceMode::Fixed(max_freq)
}

/// Extracts SCLK and VDDC hardware limits from `pp_od_clk_voltage` content.
pub fn parse_hardware_od_limits(content: &str) -> (Option<(u16, u16)>, Option<(u16, u16)>) {
    let mut sclk = None;
    let mut vddc = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("SCLK:") {
            let nums: Vec<u16> = rest
                .split_whitespace()
                .filter_map(|token| {
                    let cleaned = token
                        .trim_end_matches(|c: char| c == 'M' || c == 'h' || c == 'z' || c == 'm');
                    cleaned.parse::<u16>().ok()
                })
                .collect();
            if nums.len() >= 2 {
                sclk = Some((nums[0].min(nums[1]), nums[0].max(nums[1])));
            }
        } else if let Some(rest) = trimmed.strip_prefix("VDDC:") {
            let nums: Vec<u16> = rest
                .split_whitespace()
                .filter_map(|token| {
                    let cleaned = token
                        .trim_end_matches(|c: char| c == 'm' || c == 'V' || c == 'v');
                    cleaned.parse::<u16>().ok()
                })
                .collect();
            if nums.len() >= 2 {
                vddc = Some((nums[0].min(nums[1]), nums[0].max(nums[1])));
            }
        }
    }

    (sclk, vddc)
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClampedSafePoints {
    pub points: Vec<(u16, u16)>,
    pub adjustments: Vec<String>,
}

/// Clamps safe points to the hardware's minimum and maximum SCLK and VDDC limits.
pub fn clamp_safe_points(
    points: &[(u16, u16)],
    sclk_limits: Option<(u16, u16)>,
    vddc_limits: Option<(u16, u16)>,
) -> ClampedSafePoints {
    let mut clamped_map: BTreeMap<u16, u16> = BTreeMap::new();
    let mut adjustments = Vec::new();

    for &(orig_freq, orig_vol) in points {
        let freq = if let Some((min_sclk, max_sclk)) = sclk_limits {
            let c = orig_freq.clamp(min_sclk, max_sclk);
            if c != orig_freq {
                adjustments.push(format!(
                    "Clamping safe point frequency {orig_freq}MHz -> {c}MHz (hardware range {min_sclk}-{max_sclk}MHz)"
                ));
            }
            c
        } else {
            orig_freq
        };

        let vol = if let Some((min_vddc, max_vddc)) = vddc_limits {
            let c = orig_vol.clamp(min_vddc, max_vddc);
            if c != orig_vol {
                adjustments.push(format!(
                    "Clamping safe point voltage {orig_vol}mV -> {c}mV (hardware range {min_vddc}-{max_vddc}mV)"
                ));
            }
            c
        } else {
            orig_vol
        };

        match clamped_map.get_mut(&freq) {
            Some(existing_vol) => {
                if vol > *existing_vol {
                    *existing_vol = vol;
                }
            }
            None => {
                clamped_map.insert(freq, vol);
            }
        }
    }

    ClampedSafePoints {
        points: clamped_map.into_iter().collect(),
        adjustments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bands() -> LoadBands {
        LoadBands {
            upper: 0.90,
            medium: 0.80,
            slow: 0.70,
            crawl: 0.60,
            lower: 0.40,
        }
    }

    fn rates() -> RampStep {
        RampStep {
            burst: 1000.0,
            up: 50.0,
            up_medium: 25.0,
            up_slow: 10.0,
            up_crawl: 2.0,
            down: 0.2,
        }
    }

    fn history(samples: &[bool]) -> VecDeque<bool> {
        samples.iter().copied().collect()
    }

    #[test]
    fn empty_or_zero_window_is_idle() {
        assert_eq!(busy_ratio(&history(&[]), 4), 0.0);
        assert_eq!(busy_ratio(&history(&[true, true]), 0), 0.0);
    }

    #[test]
    fn short_history_uses_every_sample() {
        assert_eq!(busy_ratio(&history(&[true, false]), 64), 0.5);
    }

    #[test]
    fn busy_ratio_uses_only_the_newest_window() {
        assert_eq!(busy_ratio(&history(&[false, false, true, true]), 2), 1.0);
    }

    #[test]
    fn burst_needs_a_full_busy_window() {
        assert!(!is_burst(&history(&[true, true]), 0));
        assert!(!is_burst(&history(&[true]), 2));
        assert!(!is_burst(&history(&[true, false, true]), 2));
        assert!(is_burst(&history(&[false, true, true]), 2));
    }

    #[test]
    fn high_load_ramps_up_and_stops_at_the_ceiling() {
        let next = step_target(
            1500.0,
            false,
            false,
            0.95,
            0.95,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(next, 1600.0);
        let capped = step_target(
            2200.0,
            false,
            false,
            0.95,
            0.95,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(capped, 2230.0);
    }

    #[test]
    fn load_exactly_on_a_threshold_falls_into_the_next_band() {
        let next = step_target(
            1500.0,
            false,
            false,
            0.90,
            0.90,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(next, 1550.0);
        let held = step_target(
            1500.0,
            false,
            false,
            0.40,
            0.40,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(held, 1500.0);
    }

    #[test]
    fn between_crawl_and_lower_the_clock_holds() {
        let next = step_target(
            1500.0,
            false,
            false,
            0.50,
            0.50,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(next, 1500.0);
    }

    #[test]
    fn idle_ramps_down_and_stops_at_the_floor() {
        let next = step_target(
            1500.0,
            false,
            false,
            0.10,
            0.10,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(next, 1499.6);
        let floored = step_target(
            350.2,
            false,
            false,
            0.10,
            0.10,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(floored, 350.0);
    }

    #[test]
    fn burst_outranks_idle_and_max_performance_locks_the_ceiling() {
        let burst = step_target(
            1500.0,
            false,
            true,
            0.0,
            0.0,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(burst, 2230.0);
        let locked = step_target(
            1500.0,
            true,
            false,
            0.0,
            0.0,
            rates(),
            bands(),
            2.0,
            350,
            2230,
        );
        assert_eq!(locked, 2230.0);
    }

    #[test]
    fn a_pending_or_skipped_clock_is_not_sent_again() {
        assert!(!should_request_clock(
            true, false, true, true, true, 500, 100, 25
        ));
        assert!(!should_request_clock(
            false, true, true, true, true, 500, 100, 25
        ));
    }

    #[test]
    fn adjust_and_finetune_respect_their_thresholds() {
        assert!(!should_request_clock(
            false, false, false, true, false, 99, 100, 25
        ));
        assert!(should_request_clock(
            false, false, false, true, false, 100, 100, 25
        ));
        assert!(!should_request_clock(
            false, false, false, false, true, 24, 100, 25
        ));
        assert!(should_request_clock(
            false, false, false, false, true, 25, 100, 25
        ));
        assert!(!should_request_clock(
            false, false, false, false, false, 500, 100, 25
        ));
    }

    #[test]
    fn burst_requests_a_write_even_when_the_clock_already_matches() {
        assert!(should_request_clock(
            false, false, true, false, false, 0, 100, 25
        ));
    }

    fn curve() -> BTreeMap<u16, u16> {
        BTreeMap::from([(1760, 850), (1890, 900)])
    }

    #[test]
    fn voltage_follows_the_table_edges_and_the_gap() {
        assert_eq!(interpolate_voltage(1800, &BTreeMap::new()), None);
        let only = BTreeMap::from([(1500, 775)]);
        assert_eq!(interpolate_voltage(400, &only), Some(775));
        assert_eq!(interpolate_voltage(2000, &only), Some(775));
        assert_eq!(interpolate_voltage(1000, &curve()), Some(850));
        assert_eq!(interpolate_voltage(1760, &curve()), Some(850));
        assert_eq!(interpolate_voltage(2000, &curve()), Some(900));
        assert_eq!(interpolate_voltage(1800, &curve()), Some(865));
    }

    #[test]
    fn parse_performance_control_empty_or_max_locks_to_max() {
        assert_eq!(
            parse_performance_control("", 350, 2230),
            PerformanceMode::Fixed(2230)
        );
        assert_eq!(
            parse_performance_control("   \n\t", 350, 2230),
            PerformanceMode::Fixed(2230)
        );
        assert_eq!(
            parse_performance_control("max", 350, 2230),
            PerformanceMode::Fixed(2230)
        );
        assert_eq!(
            parse_performance_control("MAX", 350, 2230),
            PerformanceMode::Fixed(2230)
        );
    }

    #[test]
    fn parse_performance_control_single_frequency_is_clamped() {
        assert_eq!(
            parse_performance_control("1600", 350, 2230),
            PerformanceMode::Fixed(1600)
        );
        assert_eq!(
            parse_performance_control("200", 350, 2230),
            PerformanceMode::Fixed(350)
        );
        assert_eq!(
            parse_performance_control("3000", 350, 2230),
            PerformanceMode::Fixed(2230)
        );
    }

    #[test]
    fn parse_performance_control_range_orders_and_clamps() {
        assert_eq!(
            parse_performance_control("800 1600", 350, 2230),
            PerformanceMode::Range {
                min: 800,
                max: 1600
            }
        );
        // Inverted arguments should be correctly normalized:
        assert_eq!(
            parse_performance_control("1600 800", 350, 2230),
            PerformanceMode::Range {
                min: 800,
                max: 1600
            }
        );
        // Equal bounds turn into Fixed:
        assert_eq!(
            parse_performance_control("1500 1500", 350, 2230),
            PerformanceMode::Fixed(1500)
        );
        // Clamping out-of-range bounds:
        assert_eq!(
            parse_performance_control("100 2500", 350, 2230),
            PerformanceMode::Range {
                min: 350,
                max: 2230
            }
        );
    }

    #[test]
    fn parse_performance_control_invalid_input_defaults_to_max() {
        assert_eq!(
            parse_performance_control("invalid garbage", 350, 2230),
            PerformanceMode::Fixed(2230)
        );
        assert_eq!(
            parse_performance_control("100 200 300", 350, 2230),
            PerformanceMode::Fixed(2230)
        );
    }

    #[test]
    fn parse_hardware_od_limits_handles_standard_output() {
        let sample = "\
OD_SCLK:
0: 350Mhz
1: 2230Mhz
OD_RANGE:
SCLK:     350Mhz       2230Mhz
VDDC:     700mV        1150mV
";
        let (sclk, vddc) = parse_hardware_od_limits(sample);
        assert_eq!(sclk, Some((350, 2230)));
        assert_eq!(vddc, Some((700, 1150)));
    }

    #[test]
    fn clamp_safe_points_adjusts_out_of_bound_frequencies_and_voltages() {
        let input = vec![
            (300, 650),   // frequency below 350, voltage below 700
            (1500, 775),  // within limits
            (2300, 1200), // frequency above 2230, voltage above 1150
        ];
        let sclk_limits = Some((350, 2230));
        let vddc_limits = Some((700, 1150));

        let clamped = clamp_safe_points(&input, sclk_limits, vddc_limits);
        assert_eq!(
            clamped.points,
            vec![(350, 700), (1500, 775), (2230, 1150)]
        );
        assert_eq!(clamped.adjustments.len(), 4);
    }
}

