use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Error as IoError, ErrorKind, Write},
    os::fd::AsRawFd,
    sync::mpsc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use libdrm_amdgpu_sys::{AMDGPU::DeviceHandle, PCI::BUS_INFO};

mod thermal;
use thermal::ThermalManager;

mod governor;
use governor::{GovCommand, GovernorState, GovernorStats, SetterAck, PerformanceMode};

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct Config {
    timing: Timing,
    #[serde(rename = "frequency-thresholds")]
    frequency_thresholds: FrequencyThresholds,
    #[serde(rename = "load-target")]
    load_target: LoadTarget,
    #[serde(rename = "safe-points")]
    safe_points: Vec<SafePoint>,
    thermal: Thermal,
    #[serde(rename = "performance-mode")]
    performance_mode: PerformanceModeConfig,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct Timing {
    intervals: Intervals,
    #[serde(rename = "burst-samples")]
    burst_samples: u8,
    #[serde(rename = "ramp-up-samples")]
    ramp_up_samples: u16,
    #[serde(rename = "ramp-down-samples")]
    ramp_down_samples: u16,
    #[serde(rename = "ramp-rates")]
    ramp_rates: RampRates,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct Intervals {
    sample: u64,
    adjust: u64,
    finetune: u64,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct RampRates {
    up: f32,
    down: f32,
    burst: f32,
    #[serde(rename = "up-medium")]
    up_medium: f32,
    #[serde(rename = "up-slow")]
    up_slow: f32,
    #[serde(rename = "up-crawl")]
    up_crawl: f32,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct FrequencyThresholds {
    adjust: u16,
    finetune: u16,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct LoadTarget {
    upper: f32,         
    medium: f32,
    slow: f32,
    crawl: f32,
    lower: f32,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default)]
struct Thermal {
    max_safe_temp: f32,
    emergency_temp: f32,
    monitor_interval: u64,
    fan_control_index: usize,
    #[serde(rename = "fan-control")]
    fan_control: FanControl,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct PerformanceModeConfig {
    enabled: bool,
    control_file: String,
    check_interval: u64,
}

impl Default for PerformanceModeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            control_file: "/tmp/bc250-max-performance".to_string(),
            check_interval: 500,
        }
    }
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default)]
struct FanControl {
    enabled: bool,
    curve: Vec<(f32, u8)>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct SafePoint {
    frequency: u16,
    voltage: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timing: Default::default(),
            frequency_thresholds: Default::default(),
            load_target: Default::default(),
            safe_points: vec![
                SafePoint { frequency: 350, voltage: 700 },
                SafePoint { frequency: 2000, voltage: 1000 },
            ],
            thermal: Default::default(),
            performance_mode: Default::default(),
        }
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            intervals: Default::default(),
            burst_samples: 6,
            ramp_up_samples: 64,
            ramp_down_samples: 256,
            ramp_rates: Default::default(),
        }
    }
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            sample: 2000,
            adjust: 8_000,
            finetune: 50_000,
        }
    }
}

impl Default for RampRates {
    fn default() -> Self {
        Self {
            up: 50.0,
            down: 0.24,
            burst: 800.0,
            up_medium: 25.0,
            up_slow: 10.0,
            up_crawl: 2.0,
        }
    }
}

impl Default for FrequencyThresholds {
    fn default() -> Self {
        Self {
            adjust: 100,
            finetune: 10,
        }
    }
}

impl Default for LoadTarget {
    fn default() -> Self {
        Self {
            upper: 0.90,
            medium: 0.75,
            slow: 0.60,
            crawl: 0.50,
            lower: 0.50,
        }
    }
}


const GRBM_STATUS_REG: u32 = 0x2004;
const GPU_ACTIVE_BIT: u8 = 31;

fn calculate_fan_speed(temp: f32, curve: &[(f32, u8)]) -> u8 {
    if curve.is_empty() {
        return 0;
    }

    if temp <= curve[0].0 {
        return curve[0].1;
    }

    if let Some(last_point) = curve.last() {
        if temp >= last_point.0 {
            return last_point.1;
        }
    }

    for i in 0..curve.len() - 1 {
        let p1 = curve[i];
        let p2 = curve[i + 1];
        if temp >= p1.0 && temp <= p2.0 {
            let (temp1, speed1) = (p1.0, p1.1 as f32);
            let (temp2, speed2) = (p2.0, p2.1 as f32);
            let ratio = (temp - temp1) / (temp2 - temp1);
            return (speed1 + ratio * (speed2 - speed1)) as u8;
        }
    }

    curve.last().map_or(0, |p| p.1)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list") {
        if let Ok(tm) = ThermalManager::new() {
            println!("Sensors found: {}", tm.sensors.len());
            for sensor in &tm.sensors {
                println!("  - {} -> {}", sensor.name, sensor.temp_input);
            }
            println!("Fans found: {}", tm.fans.len());
            for (i, fan) in tm.fans.iter().enumerate() {
                println!("  - {} (index {})", fan.name, i);
                println!("      pwm: {:?}", fan.pwm_path);
                println!("      enable: {:?}", fan.enable_path);
            }
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--current-fan") {
        if let Ok(tm) = ThermalManager::new() {
            tm.print_current_fan_speeds();
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--probe-fans") {
        if let Ok(tm) = ThermalManager::new() {
            println!("Probing {} fan PWM outputs...", tm.fans.len());
            tm.probe_fans();
        }
        return Ok(());
    }

    if let Some(pos) = args.iter().position(|a| a == "--pulse-fan") {
        if let Some(idx_str) = args.get(pos + 1) {
            if let Ok(idx) = idx_str.parse::<usize>() {
                if let Ok(tm) = ThermalManager::new() {
                    tm.pulse_fan(idx)?;
                }
            }
        }
        return Ok(());
    }

    let config_str = args.get(1)
        .filter(|s| !s.starts_with("--"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();

    let config: Config = toml::from_str(&config_str).map_err(|e| {
        eprintln!("⚠️  Invalid config file: {}. Using default values.", e);
        e
    }).unwrap_or_default();

    let safe_points: BTreeMap<u16, u16> = config.safe_points.iter().map(|p| (p.frequency, p.voltage)).collect();
    if safe_points.is_empty() {
        return Err(Box::new(IoError::new(
            ErrorKind::InvalidInput,
            "safe-points must not be empty",
        )));
    }

    let location = BUS_INFO { domain: 0, bus: 1, dev: 0, func: 0 };
    let card = File::open(location.get_drm_render_path()?)?;
    let (dev_handle, _, _) = DeviceHandle::init(card.as_raw_fd()).map_err(IoError::from_raw_os_error)?;
    
    // Use safe-points as the primary source for min/max frequencies
    // device_info() can trigger "Unsupported clock type" kernel warnings on some systems
    let min_freq = safe_points.first_key_value().map(|(&k, _)| k).unwrap();
    let max_freq = safe_points.last_key_value().map(|(&k, _)| k).unwrap();

    let current_freq = std::fs::read_to_string(
        dev_handle.get_sysfs_path().map_err(IoError::from_raw_os_error)?.join("pp_od_clk_voltage")
    )
    .ok()
    .and_then(|content| {
        content.lines()
            .skip_while(|line| !line.contains("OD_SCLK:"))
            .skip(1)
            .next()
            .and_then(|line| {
                line.split_whitespace()
                    .nth(1)
                    .and_then(|s| s.trim_end_matches("Mhz").parse::<u16>().ok())
            })
    })
    .unwrap_or(min_freq);
    
    println!("🚀 Initial frequency: {}MHz (min: {}MHz, max: {}MHz)", current_freq, min_freq, max_freq);

    let pp_file = std::fs::OpenOptions::new().write(true).open(
        dev_handle.get_sysfs_path().map_err(IoError::from_raw_os_error)?.join("pp_od_clk_voltage"),
    )?;

    let (gov_send, gov_recv) = mpsc::channel::<GovCommand>();
    let (ack_send, ack_recv) = mpsc::channel::<SetterAck>();

    let thermal_manager = ThermalManager::new().ok();

    let thermal_jh = if let Some(tm) = thermal_manager {
        let thermal_config = config.thermal;
        Some(std::thread::spawn(move || {
            let mut last_thermal_check = Instant::now();
            loop {
                if last_thermal_check.elapsed() >= Duration::from_millis(thermal_config.monitor_interval) {
                    let thermal_status = tm.get_thermal_status();
                    let (pwm_opt, fan_idx_opt) = tm.get_primary_fan_info(thermal_config.fan_control_index);
                    let pwm_raw = pwm_opt;
                    let pwm_str = pwm_raw.map(|p| p.to_string()).unwrap_or_else(|| "N/A".to_string());
                    let pwm_pct = pwm_raw.map(|raw| ((raw as f32) * 100.0 / 255.0).round() as u8);
                    let pwm_pct_str = pwm_pct.map(|p| format!("{}%", p)).unwrap_or_else(|| "N/A".to_string());
                    println!("🌡️  Temps: AMD:{:.1}°C CPU:{:.1}°C Max:{:.1}°C - PWM:{} ({})",
                        thermal_status.amdgpu_temperature, thermal_status.cpu_temperature, thermal_status.max_temperature,
                        pwm_str, pwm_pct_str);

                    if thermal_status.max_temperature > thermal_config.emergency_temp {
                        eprintln!("🚨 EMERGENCY: Temp {:.1}°C > {:.1}°C. Shutting down!",
                            thermal_status.max_temperature, thermal_config.emergency_temp);
                        std::process::exit(1);
                    } else if thermal_status.max_temperature > thermal_config.max_safe_temp {
                        eprintln!("🔥 THERMAL WARNING: {:.1}°C > {:.1}°C",
                            thermal_status.max_temperature, thermal_config.max_safe_temp);
                    }

                    if thermal_config.fan_control.enabled && !thermal_config.fan_control.curve.is_empty() {
                        let target_speed = calculate_fan_speed(thermal_status.max_temperature, &thermal_config.fan_control.curve);
                        let current_percent = pwm_opt.map(|raw| ((raw as f32) * 100.0 / 255.0).round() as u8);
                        let set_idx = fan_idx_opt.unwrap_or(thermal_config.fan_control_index);
                        if current_percent != Some(target_speed) {
                            if let Err(e) = tm.set_fan_speed(set_idx, target_speed) {
                                eprintln!("Failed to set fan speed: {}", e);
                            }
                        }
                    }

                    last_thermal_check = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }))
    } else {
        None
    };

    let gov_config = config.timing;
    let load_config = config.load_target;
    let freq_config = config.frequency_thresholds;
    let perf_config = config.performance_mode;

    let jh_gov: JoinHandle<()> = std::thread::spawn(move || {
        let mut state = GovernorState::new(current_freq);
        let mut last_adjustment = Instant::now();
        let mut last_finetune = Instant::now();
        let mut last_perf_check = Instant::now();
        let mut stats = GovernorStats::default();

        let max_samples = gov_config.ramp_up_samples.max(gov_config.ramp_down_samples).max(gov_config.burst_samples as u16) as usize;
        let mut sample_history: std::collections::VecDeque<bool> = std::collections::VecDeque::with_capacity(max_samples);

        let up_samples = gov_config.ramp_up_samples as usize;
        let down_samples = gov_config.ramp_down_samples as usize;
        let burst_samples = gov_config.burst_samples as usize;

        println!("🎯 Governor config: burst={} samples, up={} samples, down={} samples",
                 burst_samples, up_samples, down_samples);
        if perf_config.enabled {
            println!("⚡ Max Performance mode enabled - control file: {}", perf_config.control_file);
        }

        loop {
            // Check for performance mode file
            if perf_config.enabled && last_perf_check.elapsed() >= Duration::from_millis(perf_config.check_interval) {
                let perf_mode_active = std::path::Path::new(&perf_config.control_file).exists();
                let new_mode = if perf_mode_active {
                    PerformanceMode::MaxPerformance
                } else {
                    PerformanceMode::Normal
                };
                
                if new_mode != state.performance_mode {
                    state.performance_mode = new_mode;
                    match new_mode {
                        PerformanceMode::MaxPerformance => {
                            println!("🚀 MAX PERFORMANCE MODE ACTIVATED - Locking to {}MHz", max_freq);
                        }
                        PerformanceMode::Normal => {
                            println!("🔄 Returning to normal dynamic frequency scaling");
                        }
                    }
                }
                last_perf_check = Instant::now();
            }

            while let Ok(ack) = ack_recv.try_recv() {
                match ack {
                    SetterAck::Applied { freq, voltage: _, latency_us } => {
                        state.applied_freq = freq;
                        state.pending_freq = None;
                        state.last_ack = Instant::now();
                        
                        stats.record_apply(latency_us);
                        
                        #[cfg(feature = "debug-transitions")]
                        if latency_us > 10_000 {
                            eprintln!("⚠️  Slow apply detected: {}μs", latency_us);
                        }
                    }
                    SetterAck::Failed { freq, error } => {
                        eprintln!("❌ Apply failed for {}MHz: {}", freq, error);
                        state.pending_freq = None;
                        stats.record_failure();
                    }
                }
            }
            
            if state.pending_freq.is_some() && state.last_ack.elapsed() > Duration::from_millis(100) {
                eprintln!("⚠️  Setter thread appears stuck! Last ack: {}ms ago",
                         state.last_ack.elapsed().as_millis());
                state.pending_freq = None;
            }
            
            let res = dev_handle.read_mm_registers(GRBM_STATUS_REG).expect("Failed to read MM registers");
            let gui_busy = (res & (1 << GPU_ACTIVE_BIT)) > 0;
            
            sample_history.push_back(gui_busy);
            if sample_history.len() > max_samples {
                sample_history.pop_front();
            }

            let burst = if burst_samples > 0 && sample_history.len() >= burst_samples {
                sample_history.iter().rev().take(burst_samples).all(|&b| b)
            } else {
                false
            };
            if burst {
                stats.record_burst();
            }

            let busy_up = if sample_history.len() >= up_samples {
                let count = sample_history.iter().rev().take(up_samples).filter(|&&b| b).count();
                (count as f32) / (up_samples as f32)
            } else if !sample_history.is_empty() {
                let count = sample_history.iter().filter(|&&b| b).count();
                (count as f32) / (sample_history.len() as f32)
            } else {
                0.0
            };
            
            let busy_down = if sample_history.len() >= down_samples {
                let count = sample_history.iter().rev().take(down_samples).filter(|&&b| b).count();
                (count as f32) / (down_samples as f32)
            } else if !sample_history.is_empty() {
                let count = sample_history.iter().filter(|&&b| b).count();
                (count as f32) / (sample_history.len() as f32)
            } else {
                0.0
            };

            let delta_time_ms = gov_config.intervals.sample as f32 / 1000.0;
            
            // If in max performance mode, lock to max frequency
            if state.performance_mode == PerformanceMode::MaxPerformance {
                state.target_freq = f32::from(max_freq);
            } else {
                // Normal dynamic frequency scaling
                if burst {
                    state.target_freq += gov_config.ramp_rates.burst * delta_time_ms;
                } else if busy_up > load_config.upper {
                    state.target_freq += gov_config.ramp_rates.up * delta_time_ms;
                } else if busy_up > load_config.medium {
                    state.target_freq += gov_config.ramp_rates.up_medium * delta_time_ms;
                } else if busy_up > load_config.slow {
                    state.target_freq += gov_config.ramp_rates.up_slow * delta_time_ms;
                } else if busy_up > load_config.crawl {
                    state.target_freq += gov_config.ramp_rates.up_crawl * delta_time_ms;
                } else if busy_down < load_config.lower {
                    state.target_freq -= gov_config.ramp_rates.down * delta_time_ms;
                }
            }

            state.target_freq = state.target_freq.clamp(
                f32::from(min_freq),
                f32::from(max_freq)
            );

            let target_freq_u16 = state.target_freq as u16;
            let diff = state.applied_freq.abs_diff(target_freq_u16);

            let should_adjust = last_adjustment.elapsed() >= 
                Duration::from_micros(gov_config.intervals.adjust);
            let should_finetune = last_finetune.elapsed() >= 
                Duration::from_micros(gov_config.intervals.finetune);

            let should_apply = state.pending_freq.is_none() && (
                burst ||
                (should_adjust && diff >= freq_config.adjust) ||
                (should_finetune && diff >= freq_config.finetune)
            );

            if should_apply {
                if let Err(e) = gov_send.send(GovCommand::SetFrequency(target_freq_u16)) {
                    eprintln!("❌ Failed to send command: {}", e);
                    break;
                }
                state.pending_freq = Some(target_freq_u16);
                
                if diff >= freq_config.adjust {
                    last_adjustment = Instant::now();
                }
                if diff >= freq_config.finetune {
                    last_finetune = Instant::now();
                }
            }

            std::thread::sleep(Duration::from_micros(gov_config.intervals.sample));
        }
        
        let _ = gov_send.send(GovCommand::Shutdown);
        eprintln!("🛑 Governor thread exiting");
        eprintln!("📊 Stats: Applies={} Failed={} Bursts={} AvgLatency={}μs MaxLatency={}μs Success={:.1}%",
                 stats.total_applies, stats.failed_applies, stats.burst_activations,
                 stats.avg_latency_us(), stats.max_latency_us, stats.success_rate());
    });

    let jh_set: JoinHandle<()> = std::thread::spawn(move || {
        let mut pp_file = pp_file;
        
        loop {
            match gov_recv.recv() {
                Ok(GovCommand::SetFrequency(freq)) => {
                    let start = Instant::now();
                    
                    let freq = freq.clamp(min_freq, max_freq);
                    
                    let vol = safe_points.range(freq..)
                        .next()
                        .or_else(|| safe_points.last_key_value())
                        .map(|(_, &v)| v);
                    
                    let vol = match vol {
                        Some(v) => v,
                        None => {
                            eprintln!("⚠️  No safe voltage for {}MHz, skipping", freq);
                            let _ = ack_send.send(SetterAck::Failed {
                                freq,
                                error: "No safe voltage found".into(),
                            });
                            continue;
                        }
                    };
                    
                    let result = (|| -> Result<(), std::io::Error> {
                        pp_file.write_all(format!("vc 0 {freq} {vol}").as_bytes())?;
                        pp_file.flush()?;
                        pp_file.write_all(b"c")?;
                        pp_file.flush()?;
                        Ok(())
                    })();
                    
                    let latency = start.elapsed().as_micros() as u64;
                    
                    match result {
                        Ok(_) => {
                            let _ = ack_send.send(SetterAck::Applied {
                                freq,
                                voltage: vol,
                                latency_us: latency,
                            });
                        }
                        Err(e) => {
                            eprintln!("⚠️  Failed to apply {}MHz @ {}mV: {}", freq, vol, e);
                            
                            if let Some((&safe_freq, &safe_vol)) = safe_points.first_key_value() {
                                let _ = pp_file.write_all(format!("vc 0 {safe_freq} {safe_vol}").as_bytes());
                                let _ = pp_file.flush();
                                let _ = pp_file.write_all(b"c");
                                let _ = pp_file.flush();
                            }
                            
                            let _ = ack_send.send(SetterAck::Failed {
                                freq,
                                error: e.to_string(),
                            });
                        }
                    }
                }
                Ok(GovCommand::Shutdown) => {
                    eprintln!("🛑 Setter thread received shutdown signal");
                    break;
                }
                Err(_) => {
                    eprintln!("🛑 Setter thread: channel closed");
                    break;
                }
            }
        }
        
        eprintln!("🛑 Setter thread exiting");
    });

    jh_set.join().unwrap();
    jh_gov.join().unwrap();
    if let Some(jh) = thermal_jh {
        jh.join().unwrap();
    }

    Ok(())
}
