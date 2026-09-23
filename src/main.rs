use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Error as IoError, ErrorKind, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc, OnceLock,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use libdrm_amdgpu_sys::{AMDGPU::DeviceHandle, PCI::BUS_INFO};

use bc_250_rust_governor::clock_limit::{
    recovery_candidates, SkippedFrequency, CLOCK_APPLY_ATTEMPTS, CLOCK_SKIP,
};
use bc_250_rust_governor::governor::{
    GovCommand, GovernorState, GovernorStats, PerformanceMode, SetterAck,
};
use bc_250_rust_governor::governor_core::{
    busy_ratio, clamp_safe_points, interpolate_voltage, is_burst, parse_hardware_od_limits,
    parse_performance_control, should_request_clock, step_target, LoadBands, RampStep,
};
use bc_250_rust_governor::gpu_metrics_fix::GpuUsageFix;
use bc_250_rust_governor::thermal::{
    calculate_fan_speed, temp_failure_should_stop, GpuTempSource, ThermalManager,
    GPU_TEMP_READ_FAILURES,
};

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    timing: Timing,
    #[serde(default, rename = "frequency-thresholds")]
    frequency_thresholds: FrequencyThresholds,
    #[serde(default, rename = "load-target")]
    load_target: LoadTarget,
    #[serde(default, rename = "safe-points")]
    safe_points: Vec<SafePoint>,
    #[serde(default = "builtin_thermal")]
    thermal: Thermal,
    #[serde(default, rename = "performance-mode")]
    performance_mode: PerformanceModeConfig,
    #[serde(default)]
    gpu: Gpu,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
struct Timing {
    #[serde(default = "default_intervals")]
    intervals: Intervals,
    #[serde(default = "default_burst_samples", rename = "burst-samples")]
    burst_samples: u8,
    #[serde(default = "default_ramp_up_samples", rename = "ramp-up-samples")]
    ramp_up_samples: u16,
    #[serde(default = "default_ramp_down_samples", rename = "ramp-down-samples")]
    ramp_down_samples: u16,
    #[serde(default = "default_ramp_rates", rename = "ramp-rates")]
    ramp_rates: RampRates,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
struct Intervals {
    #[serde(default = "default_sample_interval")]
    sample: u64,
    #[serde(default = "default_adjust_interval")]
    adjust: u64,
    #[serde(default = "default_finetune_interval")]
    finetune: u64,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
struct RampRates {
    #[serde(default = "default_ramp_up")]
    up: f32,
    #[serde(default = "default_ramp_down")]
    down: f32,
    #[serde(default = "default_ramp_burst")]
    burst: f32,
    #[serde(default = "default_ramp_up_medium", rename = "up-medium")]
    up_medium: f32,
    #[serde(default = "default_ramp_up_slow", rename = "up-slow")]
    up_slow: f32,
    #[serde(default = "default_ramp_up_crawl", rename = "up-crawl")]
    up_crawl: f32,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
struct FrequencyThresholds {
    #[serde(default = "default_adjust_threshold")]
    adjust: u16,
    #[serde(default = "default_finetune_threshold")]
    finetune: u16,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
struct LoadTarget {
    #[serde(default = "default_load_upper")]
    upper: f32,
    #[serde(default = "default_load_medium")]
    medium: f32,
    #[serde(default = "default_load_slow")]
    slow: f32,
    #[serde(default = "default_load_crawl")]
    crawl: f32,
    #[serde(default = "default_load_lower")]
    lower: f32,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
struct Thermal {
    #[serde(default = "default_max_safe_temp")]
    max_safe_temp: f32,
    #[serde(default = "default_emergency_temp")]
    emergency_temp: f32,
    #[serde(default = "default_monitor_interval")]
    monitor_interval: u64,
    #[serde(default = "default_fan_control_index")]
    fan_control_index: usize,
    #[serde(rename = "fan-control", default = "default_fan_control")]
    fan_control: FanControl,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
struct PerformanceModeConfig {
    #[serde(default = "default_performance_enabled")]
    enabled: bool,
    #[serde(default = "default_performance_control_file")]
    control_file: String,
    #[serde(default = "default_performance_check_interval")]
    check_interval: u64,
}

impl Default for PerformanceModeConfig {
    fn default() -> Self {
        builtin_parts().performance_mode.clone()
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
struct FanControl {
    #[serde(default = "default_fan_enabled")]
    enabled: bool,
    #[serde(default = "default_fan_curve")]
    curve: Vec<(f32, u8)>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
struct Gpu {
    #[serde(default = "default_pci_bus")]
    pci_bus: u8,
}

impl Default for Gpu {
    fn default() -> Self {
        builtin_parts().gpu
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
struct SafePoint {
    frequency: u16,
    voltage: u16,
}

impl Default for Timing {
    fn default() -> Self {
        builtin_parts().timing
    }
}

impl Default for Intervals {
    fn default() -> Self {
        builtin_parts().timing.intervals
    }
}

impl Default for RampRates {
    fn default() -> Self {
        builtin_parts().timing.ramp_rates
    }
}

impl Default for FrequencyThresholds {
    fn default() -> Self {
        builtin_parts().frequency_thresholds
    }
}

impl Default for LoadTarget {
    fn default() -> Self {
        builtin_parts().load_target
    }
}

const BUILTIN_COMMIT_ATTEMPTS: u32 = 3;
const BUILTIN_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);
const GOVERNOR_SHUTDOWN_WAIT: Duration = Duration::from_secs(8);
const GRBM_STATUS_REG: u32 = 0x2004;
const GPU_ACTIVE_BIT: u8 = 31;
const BUILTIN_CONFIG: &str = include_str!("../builtin-config.toml");
const CREATED_CONFIG_HEADER: &str = "\
# Created by bc-250-rust-governor because this file did not exist.
# These are the built-in conservative values. Edit this file to change
# them. The service will not replace it on later starts.

";
#[cfg(test)]
const DOCUMENTED_CONFIG: &str = include_str!("../default-config.toml");
const DEFAULT_CONFIG_PATH: &str = "/etc/bc-250-rust-governor/config.toml";

#[derive(Deserialize)]
struct BuiltinFile {
    timing: Timing,
    #[serde(rename = "frequency-thresholds")]
    frequency_thresholds: FrequencyThresholds,
    #[serde(rename = "load-target")]
    load_target: LoadTarget,
    #[serde(rename = "performance-mode")]
    performance_mode: PerformanceModeConfig,
    gpu: Gpu,
    thermal: Thermal,
}

fn builtin_parts() -> &'static BuiltinFile {
    static PARTS: OnceLock<BuiltinFile> = OnceLock::new();
    PARTS.get_or_init(|| {
        toml::from_str(BUILTIN_CONFIG).expect(
            "builtin-config.toml must include timing, frequency-thresholds, load-target, performance-mode, gpu, and thermal",
        )
    })
}

fn builtin_thermal() -> Thermal {
    builtin_parts().thermal.clone()
}

fn default_intervals() -> Intervals {
    builtin_parts().timing.intervals
}

fn default_burst_samples() -> u8 {
    builtin_parts().timing.burst_samples
}

fn default_ramp_up_samples() -> u16 {
    builtin_parts().timing.ramp_up_samples
}

fn default_ramp_down_samples() -> u16 {
    builtin_parts().timing.ramp_down_samples
}

fn default_ramp_rates() -> RampRates {
    builtin_parts().timing.ramp_rates
}

fn default_sample_interval() -> u64 {
    builtin_parts().timing.intervals.sample
}

fn default_adjust_interval() -> u64 {
    builtin_parts().timing.intervals.adjust
}

fn default_finetune_interval() -> u64 {
    builtin_parts().timing.intervals.finetune
}

fn default_ramp_up() -> f32 {
    builtin_parts().timing.ramp_rates.up
}

fn default_ramp_down() -> f32 {
    builtin_parts().timing.ramp_rates.down
}

fn default_ramp_burst() -> f32 {
    builtin_parts().timing.ramp_rates.burst
}

fn default_ramp_up_medium() -> f32 {
    builtin_parts().timing.ramp_rates.up_medium
}

fn default_ramp_up_slow() -> f32 {
    builtin_parts().timing.ramp_rates.up_slow
}

fn default_ramp_up_crawl() -> f32 {
    builtin_parts().timing.ramp_rates.up_crawl
}

fn default_adjust_threshold() -> u16 {
    builtin_parts().frequency_thresholds.adjust
}

fn default_finetune_threshold() -> u16 {
    builtin_parts().frequency_thresholds.finetune
}

fn default_load_upper() -> f32 {
    builtin_parts().load_target.upper
}

fn default_load_medium() -> f32 {
    builtin_parts().load_target.medium
}

fn default_load_slow() -> f32 {
    builtin_parts().load_target.slow
}

fn default_load_crawl() -> f32 {
    builtin_parts().load_target.crawl
}

fn default_load_lower() -> f32 {
    builtin_parts().load_target.lower
}

fn default_performance_enabled() -> bool {
    builtin_parts().performance_mode.enabled
}

fn default_performance_control_file() -> String {
    builtin_parts().performance_mode.control_file.clone()
}

fn default_performance_check_interval() -> u64 {
    builtin_parts().performance_mode.check_interval
}

fn default_pci_bus() -> u8 {
    builtin_parts().gpu.pci_bus
}

fn default_max_safe_temp() -> f32 {
    builtin_thermal().max_safe_temp
}

fn default_emergency_temp() -> f32 {
    builtin_thermal().emergency_temp
}

fn default_monitor_interval() -> u64 {
    builtin_thermal().monitor_interval
}

fn default_fan_control_index() -> usize {
    builtin_thermal().fan_control_index
}

fn default_fan_control() -> FanControl {
    builtin_thermal().fan_control
}

fn default_fan_enabled() -> bool {
    builtin_thermal().fan_control.enabled
}

fn default_fan_curve() -> Vec<(f32, u8)> {
    builtin_thermal().fan_control.curve
}

fn config_path_from_args(args: &[String]) -> &str {
    args.get(1)
        .map(String::as_str)
        .filter(|s| !s.starts_with("--"))
        .unwrap_or(DEFAULT_CONFIG_PATH)
}

fn installed_config_text() -> String {
    format!("{CREATED_CONFIG_HEADER}{BUILTIN_CONFIG}")
}

/// Creates `path` from the config embedded in the binary when it does not exist.
/// The installed copy starts with a note that the service created it.
/// An existing file is left untouched. Returns whether a file was created.
fn install_builtin_config(path: &Path) -> Result<bool, IoError> {
    if path.exists() {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    file.write_all(installed_config_text().as_bytes())?;
    Ok(true)
}

/// First safe-point of the config embedded in the binary. Changing
/// `builtin-config.toml` changes the clock committed on shutdown.
fn builtin_safe_point() -> Result<SafePoint, Box<dyn std::error::Error>> {
    let config: Config = toml::from_str(BUILTIN_CONFIG)?;
    config.safe_points.into_iter().next().ok_or_else(|| {
        IoError::new(
            ErrorKind::InvalidInput,
            "builtin-config.toml safe-points must not be empty",
        )
        .into()
    })
}

fn commit_clock(pp_file: &mut File, freq: u16, vol: u16) -> Result<(), IoError> {
    pp_file.write_all(format!("vc 0 {freq} {vol}").as_bytes())?;
    pp_file.flush()?;
    pp_file.write_all(b"c")?;
    pp_file.flush()?;
    Ok(())
}

enum FrequencyApply {
    Landed { freq: u16, voltage: u16 },
    Neighbor { requested: u16, freq: u16 },
    Exhausted { requested: u16 },
}

/// Retries the requested pair, then walks neighboring safe-points until one is accepted.
fn apply_frequency(
    pp_file: &mut File,
    freq: u16,
    voltage: u16,
    safe_points: &BTreeMap<u16, u16>,
) -> FrequencyApply {
    let mut last_error = None;
    for _ in 0..CLOCK_APPLY_ATTEMPTS {
        match commit_clock(pp_file, freq, voltage) {
            Ok(()) => return FrequencyApply::Landed { freq, voltage },
            Err(error) => last_error = Some(error),
        }
    }
    eprintln!(
        "Clock {freq} MHz @ {voltage} mV refused after {CLOCK_APPLY_ATTEMPTS} attempts: {}",
        last_error.expect("a failed attempt stores its error")
    );

    for (candidate_freq, candidate_vol) in recovery_candidates(freq, safe_points) {
        match commit_clock(pp_file, candidate_freq, candidate_vol) {
            Ok(()) => {
                return FrequencyApply::Neighbor {
                    requested: freq,
                    freq: candidate_freq,
                };
            }
            Err(error) => {
                eprintln!("Clock {candidate_freq} MHz @ {candidate_vol} mV refused: {error}");
            }
        }
    }
    FrequencyApply::Exhausted { requested: freq }
}

#[derive(Debug)]
enum ExactAck {
    Applied,
    Failed(String),
    TimedOut,
}

/// Waits until the setter reports this exact frequency and voltage.
/// Acks for other clocks stay ignored, so an in-flight write cannot confirm the builtin point.
fn wait_for_exact_ack(
    ack_recv: &mpsc::Receiver<SetterAck>,
    frequency: u16,
    voltage: u16,
    timeout: Duration,
) -> ExactAck {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ExactAck::TimedOut;
        }
        match ack_recv.recv_timeout(remaining) {
            Ok(SetterAck::Applied {
                freq, voltage: vol, ..
            }) if freq == frequency && vol == voltage => {
                return ExactAck::Applied;
            }
            Ok(SetterAck::Failed {
                freq,
                voltage: vol,
                error,
            }) if freq == frequency && vol == voltage => {
                return ExactAck::Failed(error);
            }
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => return ExactAck::TimedOut,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return ExactAck::Failed("setter is gone".into());
            }
        }
    }
}

/// Sends the builtin safe-point and retries until the setter confirms it.
fn commit_builtin_safe_point(
    gov_send: &mpsc::Sender<GovCommand>,
    ack_recv: &mpsc::Receiver<SetterAck>,
    point: SafePoint,
) -> bool {
    for attempt in 1..=BUILTIN_COMMIT_ATTEMPTS {
        if gov_send
            .send(GovCommand::SetExact {
                frequency: point.frequency,
                voltage: point.voltage,
            })
            .is_err()
        {
            eprintln!(
                "Builtin safe-point commit failed (attempt {attempt}/{BUILTIN_COMMIT_ATTEMPTS}): setter is gone"
            );
            return false;
        }

        match wait_for_exact_ack(
            ack_recv,
            point.frequency,
            point.voltage,
            BUILTIN_COMMIT_TIMEOUT,
        ) {
            ExactAck::Applied => {
                eprintln!(
                    "Builtin safe-point {}MHz @ {}mV committed",
                    point.frequency, point.voltage
                );
                return true;
            }
            ExactAck::Failed(error) => {
                eprintln!(
                    "Builtin safe-point commit failed (attempt {attempt}/{BUILTIN_COMMIT_ATTEMPTS}): {error}"
                );
            }
            ExactAck::TimedOut => {
                eprintln!(
                    "Builtin safe-point commit failed (attempt {attempt}/{BUILTIN_COMMIT_ATTEMPTS}): no confirmation within {}s",
                    BUILTIN_COMMIT_TIMEOUT.as_secs()
                );
            }
        }
    }
    false
}

fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let created = install_builtin_config(path).map_err(|e| {
        IoError::new(
            e.kind(),
            format!("Failed to create config {}: {e}", path.display()),
        )
    })?;
    if created {
        eprintln!(
            "Config file missing. Wrote built-in default to {}",
            path.display()
        );
    }

    let config_str = fs::read_to_string(path).map_err(|e| {
        IoError::new(
            e.kind(),
            format!("Failed to read config {}: {e}", path.display()),
        )
    })?;
    toml::from_str(&config_str).map_err(|e| {
        IoError::new(
            ErrorKind::InvalidData,
            format!(
                "Invalid config file {}: {e}. Refusing to start; the file was left unchanged.",
                path.display()
            ),
        )
        .into()
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list") {
        if let Ok(tm) = ThermalManager::new() {
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

    let config = load_config(Path::new(config_path_from_args(&args)))?;
    let emergency_point = builtin_safe_point()?;

    let raw_points: Vec<(u16, u16)> = config
        .safe_points
        .iter()
        .map(|p| (p.frequency, p.voltage))
        .collect();
    if raw_points.is_empty() {
        return Err(Box::new(IoError::new(
            ErrorKind::InvalidInput,
            "safe-points must not be empty",
        )));
    }

    let location = BUS_INFO {
        domain: 0,
        bus: config.gpu.pci_bus,
        dev: 0,
        func: 0,
    };
    let card = File::open(location.get_drm_render_path()?)?;
    let (dev_handle, _, _) =
        DeviceHandle::init(card.as_raw_fd()).map_err(IoError::from_raw_os_error)?;
    let gpu_sysfs = dev_handle
        .get_sysfs_path()
        .map_err(IoError::from_raw_os_error)?;

    let od_content = std::fs::read_to_string(gpu_sysfs.join("pp_od_clk_voltage")).ok();

    let safe_points_vec = if let Some(ref od_text) = od_content {
        let (sclk_limits, vddc_limits) = parse_hardware_od_limits(od_text);
        let clamped = clamp_safe_points(&raw_points, sclk_limits, vddc_limits);
        for adj in &clamped.adjustments {
            println!("⚠️  {adj}");
        }
        clamped.points
    } else {
        raw_points
    };

    let safe_points: BTreeMap<u16, u16> = safe_points_vec.into_iter().collect();

    let (&min_freq, _) = safe_points
        .first_key_value()
        .expect("safe-points was checked above");
    let (&max_freq, _) = safe_points
        .last_key_value()
        .expect("safe-points was checked above");

    let current_freq = od_content
        .as_deref()
        .and_then(|content| {
            content
                .lines()
                .skip_while(|line| !line.contains("OD_SCLK:"))
                .nth(1)
                .and_then(|line| {
                    line.split_whitespace()
                        .nth(1)
                        .and_then(|s| s.trim_end_matches("Mhz").parse::<u16>().ok())
                })
        })
        .unwrap_or(min_freq);

    println!(
        "🎮 GPU: PCI bus {} ({})",
        config.gpu.pci_bus,
        gpu_sysfs.display()
    );
    println!(
        "⚡ Safe-points: {} points loaded ({}MHz @ {}mV -> {}MHz @ {}mV)",
        safe_points.len(),
        min_freq,
        safe_points[&min_freq],
        max_freq,
        safe_points[&max_freq]
    );
    println!(
        "🚀 Initial frequency: {}MHz (min: {}MHz, max: {}MHz)",
        current_freq, min_freq, max_freq
    );

    let gpu_temp = GpuTempSource::open(&gpu_sysfs);
    let pp_file = std::fs::OpenOptions::new()
        .write(true)
        .open(gpu_sysfs.join("pp_od_clk_voltage"))?;

    let (gov_send, gov_recv) = mpsc::channel::<GovCommand>();
    let (ack_send, ack_recv) = mpsc::channel::<SetterAck>();

    // Shared shutdown flag for graceful termination
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let builtin_committed = Arc::new(AtomicBool::new(false));
    let peak_temp_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));

    // Register Ctrl+C handler for graceful shutdown
    let shutdown_flag_signal = Arc::clone(&shutdown_flag);
    ctrlc::set_handler(move || {
        eprintln!("\n🛑 SIGINT / Ctrl+C detected! Initiating graceful shutdown...");
        shutdown_flag_signal.store(true, Ordering::SeqCst);
    })
    .expect("Failed to register Ctrl+C handler");

    let thermal_manager = ThermalManager::new().ok();
    let thermal_manager_clone = thermal_manager.clone();

    let thermal_config = config.thermal;
    let shutdown_flag_thermal = Arc::clone(&shutdown_flag);
    let peak_temp_bits_thermal = Arc::clone(&peak_temp_bits);
    let thermal_jh = std::thread::spawn(move || {
        let tm = thermal_manager;
        let mut last_thermal_check = Instant::now();
        let mut temp_failures = 0u32;
        let mut read_immediately = true;
        let mut last_logged_temp: Option<f32> = None;
        let mut last_logged_pwm: Option<Option<u8>> = None;
        let mut last_logged_instant = Instant::now();

        loop {
            if shutdown_flag_thermal.load(Ordering::SeqCst) {
                break;
            }

            if read_immediately
                || last_thermal_check.elapsed()
                    >= Duration::from_millis(thermal_config.monitor_interval)
            {
                let pwm_raw = tm
                    .as_ref()
                    .and_then(|tm| tm.get_primary_fan_info(thermal_config.fan_control_index));

                match gpu_temp.read() {
                    Ok(reading) => {
                        temp_failures = 0;

                        // Track peak temperature observed across daemon lifetime
                        let mut current_peak =
                            f32::from_bits(peak_temp_bits_thermal.load(Ordering::Relaxed));
                        while reading.decision_c > current_peak {
                            match peak_temp_bits_thermal.compare_exchange_weak(
                                current_peak.to_bits(),
                                reading.decision_c.to_bits(),
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            ) {
                                Ok(_) => break,
                                Err(actual) => current_peak = f32::from_bits(actual),
                            }
                        }

                        let edge = reading
                            .edge_c
                            .map(|c| format!("{c:.1}°C"))
                            .unwrap_or_else(|| "N/A".to_string());
                        let hotspot = reading
                            .hotspot_c
                            .map(|c| format!("{c:.1}°C"))
                            .unwrap_or_else(|| "N/A".to_string());

                        let is_emergency = reading.decision_c > thermal_config.emergency_temp;
                        let is_warning = reading.decision_c > thermal_config.max_safe_temp;
                        let temp_jumped = last_logged_temp
                            .map_or(true, |t| (reading.decision_c - t).abs() >= 2.0);
                        let pwm_changed = last_logged_pwm != Some(pwm_raw);
                        let heartbeat =
                            last_logged_instant.elapsed() >= Duration::from_secs(10);

                        if read_immediately
                            || is_emergency
                            || is_warning
                            || temp_jumped
                            || pwm_changed
                            || heartbeat
                        {
                            let (pwm_str, pwm_pct_str) = match pwm_raw {
                                Some(raw) => {
                                    let pct = ((raw as f32) * 100.0 / 255.0).round() as u8;
                                    (raw.to_string(), format!("{pct}%"))
                                }
                                None => ("N/A".to_string(), "N/A".to_string()),
                            };
                            println!(
                                "🌡️  GPU edge:{edge} hotspot:{hotspot} decision:{:.1}°C - PWM:{pwm_str} ({pwm_pct_str})",
                                reading.decision_c
                            );
                            last_logged_temp = Some(reading.decision_c);
                            last_logged_pwm = Some(pwm_raw);
                            last_logged_instant = Instant::now();
                        }

                        read_immediately = false;

                        if is_emergency {
                            eprintln!(
                                "🚨 EMERGENCY: GPU {:.1}°C > {:.1}°C. Shutting down!",
                                reading.decision_c, thermal_config.emergency_temp
                            );
                            shutdown_flag_thermal.store(true, Ordering::SeqCst);
                            break;
                        } else if is_warning {
                            eprintln!(
                                "🔥 THERMAL WARNING: GPU {:.1}°C > {:.1}°C",
                                reading.decision_c, thermal_config.max_safe_temp
                            );
                        }

                        if let Some(tm) = tm.as_ref() {
                            if thermal_config.fan_control.enabled
                                && !thermal_config.fan_control.curve.is_empty()
                            {
                                let target_speed = calculate_fan_speed(
                                    reading.decision_c,
                                    &thermal_config.fan_control.curve,
                                );
                                let current_percent = pwm_raw
                                    .map(|raw| ((raw as f32) * 100.0 / 255.0).round() as u8);
                                if current_percent != Some(target_speed) {
                                    if let Err(e) = tm.set_fan_speed(
                                        thermal_config.fan_control_index,
                                        target_speed,
                                    ) {
                                        eprintln!("Failed to set fan speed: {}", e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if temp_failure_should_stop(&mut temp_failures) {
                            eprintln!(
                                "GPU temperature unreadable after {GPU_TEMP_READ_FAILURES} attempts ({e}). Shutting down."
                            );
                            shutdown_flag_thermal.store(true, Ordering::SeqCst);
                            break;
                        }
                        eprintln!(
                            "GPU temperature unreadable ({temp_failures}/{GPU_TEMP_READ_FAILURES}): {e}"
                        );
                    }
                }

                last_thermal_check = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });

    let gov_config = config.timing;
    let load_config = config.load_target;
    let freq_config = config.frequency_thresholds;
    let perf_config = config.performance_mode;

    let gpu_fix = match GpuUsageFix::start(gpu_sysfs) {
        Ok(fix) => Some(fix),
        Err(e) => {
            eprintln!(
                "⚠️  GPU metrics fix unavailable: {e}. MangoHUD may show incorrect GPU usage."
            );
            None
        }
    };

    // Clone for governor thread
    let gov_send_clone = gov_send.clone();
    let shutdown_flag_gov = Arc::clone(&shutdown_flag);
    let builtin_committed_gov = Arc::clone(&builtin_committed);
    let peak_temp_bits_gov = Arc::clone(&peak_temp_bits);
    let emergency_for_log = emergency_point.clone();

    let jh_gov: JoinHandle<()> = std::thread::spawn(move || {
        let gov_send = gov_send_clone;
        let mut gpu_fix = gpu_fix;
        let mut state = GovernorState::new(current_freq);
        let mut last_adjustment = Instant::now();
        let mut last_finetune = Instant::now();
        let mut last_perf_check = Instant::now();
        let mut last_metrics_update = Instant::now();
        let mut stats = GovernorStats::default();

        let max_samples = gov_config
            .ramp_up_samples
            .max(gov_config.ramp_down_samples)
            .max(gov_config.burst_samples as u16) as usize;
        let mut sample_history: std::collections::VecDeque<bool> =
            std::collections::VecDeque::with_capacity(max_samples);

        let up_samples = gov_config.ramp_up_samples as usize;
        let down_samples = gov_config.ramp_down_samples as usize;
        let burst_samples = gov_config.burst_samples as usize;

        println!(
            "🎯 Governor config: burst={} samples, up={} samples, down={} samples",
            burst_samples, up_samples, down_samples
        );
        if perf_config.enabled {
            println!(
                "⚡ Max Performance mode enabled - control file: {}",
                perf_config.control_file
            );
        }

        let mut skipped_freq: Option<SkippedFrequency> = None;

        'governor: loop {
            // Check for shutdown signal
            if shutdown_flag_gov.load(Ordering::SeqCst) {
                break;
            }

            if let Some(skip) = skipped_freq {
                if skip.expired(Instant::now()) {
                    eprintln!("Clock {} MHz is allowed again.", skip.freq);
                    skipped_freq = None;
                }
            }

            // Check for performance mode file
            if perf_config.enabled
                && last_perf_check.elapsed() >= Duration::from_millis(perf_config.check_interval)
            {
                let path = std::path::Path::new(&perf_config.control_file);
                let new_mode = if path.exists() {
                    let content = fs::read_to_string(path).unwrap_or_default();
                    parse_performance_control(&content, min_freq, max_freq)
                } else {
                    PerformanceMode::Normal
                };

                if new_mode != state.performance_mode {
                    state.performance_mode = new_mode;
                    match new_mode {
                        PerformanceMode::Fixed(freq) => {
                            println!("🚀 PERFORMANCE MODE ACTIVATED - Locking to {freq}MHz");
                        }
                        PerformanceMode::Range { min, max } => {
                            println!(
                                "🚀 PERFORMANCE MODE ACTIVATED - Dynamic scaling constrained to {min}MHz..={max}MHz"
                            );
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
                    SetterAck::Applied {
                        freq, latency_us, ..
                    } => {
                        state.applied_freq = freq;
                        state.pending_freq = None;
                        state.last_ack = Instant::now();

                        stats.record_apply(latency_us);

                        #[cfg(feature = "debug-transitions")]
                        if latency_us > 10_000 {
                            eprintln!("⚠️  Slow apply detected: {}μs", latency_us);
                        }
                    }
                    SetterAck::Failed { freq, error, .. } => {
                        eprintln!("❌ Apply failed for {}MHz: {}", freq, error);
                        state.pending_freq = None;
                        stats.record_failure();
                    }
                    SetterAck::Recovered {
                        requested,
                        freq,
                        latency_us,
                    } => {
                        state.applied_freq = freq;
                        state.pending_freq = None;
                        state.last_ack = Instant::now();
                        skipped_freq = Some(SkippedFrequency::start(requested, Instant::now()));
                        stats.record_apply(latency_us);
                        eprintln!(
                            "Clock {requested} MHz refused. Skipping it for {}s. GPU left at {freq} MHz.",
                            CLOCK_SKIP.as_secs()
                        );
                    }
                    SetterAck::Exhausted { requested } => {
                        eprintln!(
                            "No safe-point accepted after {requested} MHz was refused. Shutting down."
                        );
                        stats.record_failure();
                        shutdown_flag_gov.store(true, Ordering::SeqCst);
                        break 'governor;
                    }
                }
            }

            if state.pending_freq.is_some() && state.last_ack.elapsed() > Duration::from_millis(100)
            {
                eprintln!(
                    "⚠️  Setter thread appears stuck! Last ack: {}ms ago",
                    state.last_ack.elapsed().as_millis()
                );
                state.pending_freq = None;
            }

            // Read GPU activity register with graceful error handling
            let res = match dev_handle.read_mm_registers(GRBM_STATUS_REG) {
                Ok(value) => value,
                Err(e) => {
                    eprintln!("⚠️  Failed to read MM registers: {}. Assuming GPU idle.", e);
                    0 // Assume GPU is idle on error
                }
            };
            let gui_busy = (res & (1 << GPU_ACTIVE_BIT)) > 0;

            sample_history.push_back(gui_busy);
            if sample_history.len() > max_samples {
                sample_history.pop_front();
            }

            let burst = is_burst(&sample_history, burst_samples);
            if burst {
                stats.record_burst();
            }

            let busy_up = busy_ratio(&sample_history, up_samples);
            let busy_down = busy_ratio(&sample_history, down_samples);

            // Update patched gpu_metrics every 200ms so MangoHUD shows correct usage
            if let Some(ref mut fix) = gpu_fix {
                if last_metrics_update.elapsed() >= Duration::from_millis(200) {
                    if let Err(e) = fix.set_usage_percent(busy_up * 100.0) {
                        eprintln!("⚠️  GPU metrics fix write failed: {}", e);
                    }
                    last_metrics_update = Instant::now();
                }
            }

            let delta_time_ms = gov_config.intervals.sample as f32 / 1000.0;

            let (eff_min, eff_max, is_fixed) = match state.performance_mode {
                PerformanceMode::Normal => (min_freq, max_freq, None),
                PerformanceMode::Fixed(freq) => (freq, freq, Some(freq)),
                PerformanceMode::Range { min, max } => (min, max, None),
            };

            state.target_freq = if let Some(fixed) = is_fixed {
                f32::from(fixed)
            } else {
                step_target(
                    state.target_freq,
                    false,
                    burst,
                    busy_up,
                    busy_down,
                    RampStep {
                        burst: gov_config.ramp_rates.burst,
                        up: gov_config.ramp_rates.up,
                        up_medium: gov_config.ramp_rates.up_medium,
                        up_slow: gov_config.ramp_rates.up_slow,
                        up_crawl: gov_config.ramp_rates.up_crawl,
                        down: gov_config.ramp_rates.down,
                    },
                    LoadBands {
                        upper: load_config.upper,
                        medium: load_config.medium,
                        slow: load_config.slow,
                        crawl: load_config.crawl,
                        lower: load_config.lower,
                    },
                    delta_time_ms,
                    eff_min,
                    eff_max,
                )
            };

            let target_freq_u16 = state.target_freq as u16;
            let diff = state.applied_freq.abs_diff(target_freq_u16);

            let should_adjust =
                last_adjustment.elapsed() >= Duration::from_micros(gov_config.intervals.adjust);
            let should_finetune =
                last_finetune.elapsed() >= Duration::from_micros(gov_config.intervals.finetune);

            let skipping =
                skipped_freq.is_some_and(|skip| skip.blocks(target_freq_u16, Instant::now()));
            let should_apply = should_request_clock(
                state.pending_freq.is_some(),
                skipping,
                burst,
                should_adjust,
                should_finetune,
                diff,
                freq_config.adjust,
                freq_config.finetune,
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

        // Remove the bind mount before the process exits so sysfs is restored
        if let Some(fix) = gpu_fix {
            if let Err(e) = fix.shutdown() {
                eprintln!("⚠️  GPU metrics fix shutdown failed: {}", e);
            }
        }

        let committed = commit_builtin_safe_point(&gov_send, &ack_recv, emergency_point);
        builtin_committed_gov.store(committed, Ordering::SeqCst);
        let _ = gov_send.send(GovCommand::Shutdown);
        let peak_temp = f32::from_bits(peak_temp_bits_gov.load(Ordering::Relaxed));
        eprintln!(
            "📊 Stats: Applies={} Failed={} Bursts={} PeakTemp={:.1}°C AvgLatency={}μs MaxLatency={}μs Success={:.1}%",
            stats.total_applies,
            stats.failed_applies,
            stats.burst_activations,
            peak_temp,
            stats.avg_latency_us(),
            stats.max_latency_us,
            stats.success_rate()
        );
    });

    let jh_set: JoinHandle<()> = std::thread::spawn(move || {
        let mut pp_file = pp_file;

        loop {
            match gov_recv.recv() {
                Ok(GovCommand::SetFrequency(freq)) => {
                    let start = Instant::now();

                    let freq = freq.clamp(min_freq, max_freq);

                    // Interpolate voltage between safe-points
                    let vol = interpolate_voltage(freq, &safe_points);

                    let vol = match vol {
                        Some(v) => v,
                        None => {
                            eprintln!("⚠️  No safe voltage for {}MHz, skipping", freq);
                            let _ = ack_send.send(SetterAck::Failed {
                                freq,
                                voltage: 0,
                                error: "No safe voltage found".into(),
                            });
                            continue;
                        }
                    };

                    match apply_frequency(&mut pp_file, freq, vol, &safe_points) {
                        FrequencyApply::Landed { freq, voltage } => {
                            let _ = ack_send.send(SetterAck::Applied {
                                freq,
                                voltage,
                                latency_us: start.elapsed().as_micros() as u64,
                            });
                        }
                        FrequencyApply::Neighbor { requested, freq } => {
                            let _ = ack_send.send(SetterAck::Recovered {
                                requested,
                                freq,
                                latency_us: start.elapsed().as_micros() as u64,
                            });
                        }
                        FrequencyApply::Exhausted { requested } => {
                            let _ = ack_send.send(SetterAck::Exhausted { requested });
                        }
                    }
                }
                Ok(GovCommand::SetExact { frequency, voltage }) => {
                    let start = Instant::now();
                    match commit_clock(&mut pp_file, frequency, voltage) {
                        Ok(()) => {
                            let _ = ack_send.send(SetterAck::Applied {
                                freq: frequency,
                                voltage,
                                latency_us: start.elapsed().as_micros() as u64,
                            });
                        }
                        Err(e) => {
                            eprintln!(
                                "⚠️  Failed to commit builtin safe-point {}MHz @ {}mV: {}",
                                frequency, voltage, e
                            );
                            let _ = ack_send.send(SetterAck::Failed {
                                freq: frequency,
                                voltage,
                                error: e.to_string(),
                            });
                        }
                    }
                }
                Ok(GovCommand::Shutdown) | Err(_) => {
                    break;
                }
            }
        }
    });

    // Wait for shutdown signal (blocking poll with timeout for graceful shutdown)
    loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // The governor confirms the builtin safe-point before stopping the setter.
    eprintln!("🛑 Shutting down governor threads...");

    let start = Instant::now();
    let mut governor_finished = false;
    while start.elapsed() < GOVERNOR_SHUTDOWN_WAIT {
        if jh_gov.is_finished() {
            governor_finished = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let committed = if governor_finished {
        let _ = jh_gov.join();
        builtin_committed.load(Ordering::SeqCst)
    } else {
        eprintln!(
            "Builtin safe-point was not confirmed within {}s",
            GOVERNOR_SHUTDOWN_WAIT.as_secs()
        );
        false
    };

    let timeout = Duration::from_secs(5);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if jh_set.is_finished() {
            let _ = jh_set.join();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let start = Instant::now();
    while start.elapsed() < timeout {
        if thermal_jh.is_finished() {
            let _ = thermal_jh.join();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if committed {
        if let Some(tm) = thermal_manager_clone {
            eprintln!("🔄 Restoring fans to automatic control...");
            if let Err(e) = tm.restore_auto_fan_control() {
                eprintln!("⚠️  Failed to restore fan control: {}", e);
            }
        }
    } else {
        eprintln!(
            "Builtin safe-point {}MHz @ {}mV was not committed. Leaving fans in manual control.",
            emergency_for_log.frequency, emergency_for_log.voltage
        );
    }

    eprintln!("✅ Shutdown complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bc250-governor-config-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn builtin_config_is_one_conservative_point() {
        let config: Config = toml::from_str(BUILTIN_CONFIG).unwrap();
        assert_eq!(
            config.safe_points,
            vec![SafePoint {
                frequency: 1500,
                voltage: 775
            }]
        );
        assert!(config.thermal.emergency_temp > config.thermal.max_safe_temp);
    }

    #[test]
    fn exact_ack_requires_the_builtin_voltage() {
        let (tx, rx) = mpsc::channel();
        tx.send(SetterAck::Applied {
            freq: 1500,
            voltage: 763,
            latency_us: 1,
        })
        .unwrap();
        tx.send(SetterAck::Applied {
            freq: 1500,
            voltage: 775,
            latency_us: 1,
        })
        .unwrap();

        assert!(matches!(
            wait_for_exact_ack(&rx, 1500, 775, Duration::from_millis(200)),
            ExactAck::Applied
        ));
    }

    #[test]
    fn exact_ack_reports_a_matching_failure() {
        let (tx, rx) = mpsc::channel();
        tx.send(SetterAck::Failed {
            freq: 1500,
            voltage: 775,
            error: "rejected".into(),
        })
        .unwrap();

        match wait_for_exact_ack(&rx, 1500, 775, Duration::from_millis(200)) {
            ExactAck::Failed(error) => assert_eq!(error, "rejected"),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn omitted_thermal_comes_from_builtin() {
        let builtin: Config = toml::from_str(BUILTIN_CONFIG).unwrap();
        let missing_section: Config =
            toml::from_str("safe-points = [{ frequency = 1500, voltage = 775 }]\n").unwrap();
        assert_eq!(
            missing_section.thermal.emergency_temp,
            builtin.thermal.emergency_temp
        );
        assert_eq!(
            missing_section.thermal.max_safe_temp,
            builtin.thermal.max_safe_temp
        );
        assert_eq!(
            missing_section.thermal.monitor_interval,
            builtin.thermal.monitor_interval
        );
        assert_eq!(
            missing_section.thermal.fan_control.enabled,
            builtin.thermal.fan_control.enabled
        );
        assert_eq!(
            missing_section.thermal.fan_control.curve,
            builtin.thermal.fan_control.curve
        );
        assert!(missing_section.thermal.emergency_temp > 0.0);

        let missing_key: Config = toml::from_str(
            "safe-points = [{ frequency = 1500, voltage = 775 }]\n[thermal]\nmax_safe_temp = 70.0\n",
        )
        .unwrap();
        assert_eq!(missing_key.thermal.max_safe_temp, 70.0);
        assert_eq!(
            missing_key.thermal.emergency_temp,
            builtin.thermal.emergency_temp
        );

        let explicit: Config = toml::from_str(
            "safe-points = [{ frequency = 1500, voltage = 775 }]\n[thermal]\nemergency_temp = 80.0\n",
        )
        .unwrap();
        assert_eq!(explicit.thermal.emergency_temp, 80.0);
    }

    #[test]
    fn omitted_sections_and_keys_come_from_builtin() {
        let builtin: Config = toml::from_str(BUILTIN_CONFIG).unwrap();
        let missing_sections: Config =
            toml::from_str("safe-points = [{ frequency = 1500, voltage = 775 }]\n").unwrap();
        assert_eq!(missing_sections.timing, builtin.timing);
        assert_eq!(
            missing_sections.frequency_thresholds,
            builtin.frequency_thresholds
        );
        assert_eq!(missing_sections.load_target, builtin.load_target);
        assert_eq!(missing_sections.performance_mode, builtin.performance_mode);
        assert_eq!(missing_sections.gpu, builtin.gpu);
        assert_eq!(missing_sections.timing.burst_samples, 20);
        assert_eq!(missing_sections.timing.ramp_rates.burst, 1000.0);
        assert_eq!(missing_sections.timing.ramp_rates.down, 0.2);
        assert_eq!(missing_sections.frequency_thresholds.finetune, 25);
        assert_eq!(missing_sections.load_target.medium, 0.80);
        assert_eq!(missing_sections.load_target.lower, 0.40);

        let missing_key: Config = toml::from_str(
            "safe-points = [{ frequency = 1500, voltage = 775 }]\n[timing]\nburst-samples = 4\n",
        )
        .unwrap();
        assert_eq!(missing_key.timing.burst_samples, 4);
        assert_eq!(missing_key.timing.ramp_rates, builtin.timing.ramp_rates);
        assert_eq!(missing_key.timing.intervals, builtin.timing.intervals);
    }

    #[test]
    fn documented_settings_match_builtin_except_safe_points() {
        let builtin: Config = toml::from_str(BUILTIN_CONFIG).unwrap();
        let documented: Config = toml::from_str(DOCUMENTED_CONFIG).unwrap();
        assert_eq!(builtin.timing, documented.timing);
        assert_eq!(
            builtin.frequency_thresholds,
            documented.frequency_thresholds
        );
        assert_eq!(builtin.load_target, documented.load_target);
        assert_eq!(builtin.performance_mode, documented.performance_mode);
        assert_eq!(builtin.gpu, documented.gpu);
        assert_eq!(builtin.thermal, documented.thermal);
        assert_ne!(builtin.safe_points, documented.safe_points);
    }

    #[test]
    fn emergency_clock_is_the_builtin_safe_point() {
        let emergency = builtin_safe_point().unwrap();
        let builtin: Config = toml::from_str(BUILTIN_CONFIG).unwrap();
        let documented: Config = toml::from_str(DOCUMENTED_CONFIG).unwrap();

        assert_eq!(emergency, builtin.safe_points[0]);
        assert_ne!(
            emergency.frequency,
            documented.safe_points.last().unwrap().frequency
        );
    }

    #[test]
    fn documented_config_keeps_the_full_curve() {
        let config: Config = toml::from_str(DOCUMENTED_CONFIG).unwrap();
        assert_eq!(config.safe_points.first().unwrap().frequency, 350);
        assert_eq!(config.safe_points.last().unwrap().frequency, 2230);
        assert!(config.safe_points.len() > 1);
    }

    #[test]
    fn installs_builtin_config_when_missing_and_keeps_existing() {
        let dir = scratch_dir();
        let path = dir.join("config.toml");

        assert!(install_builtin_config(&path).unwrap());
        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written, installed_config_text());
        assert!(written.starts_with("# Created by bc-250-rust-governor"));
        let parsed: Config = toml::from_str(&written).unwrap();
        let builtin: Config = toml::from_str(BUILTIN_CONFIG).unwrap();
        assert_eq!(parsed.safe_points, builtin.safe_points);

        fs::write(
            &path,
            "safe-points = [{ frequency = 1800, voltage = 850 }]\n",
        )
        .unwrap();
        assert!(!install_builtin_config(&path).unwrap());
        assert!(fs::read_to_string(&path).unwrap().contains("1800"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recreates_config_when_directory_was_removed() {
        let dir = scratch_dir();
        let path = dir
            .join("etc")
            .join("bc-250-rust-governor")
            .join("config.toml");
        fs::remove_dir_all(&dir).unwrap();

        assert!(install_builtin_config(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), installed_config_text());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_config_is_left_in_place() {
        let dir = scratch_dir();
        let path = dir.join("config.toml");
        let original = "this is not toml {{{";
        fs::write(&path, original).unwrap();

        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("Refusing to start"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        let _ = fs::remove_dir_all(&dir);
    }
}
