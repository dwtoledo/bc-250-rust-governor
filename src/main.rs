
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Error as IoError, ErrorKind, Write},
    os::fd::AsRawFd,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use libdrm_amdgpu_sys::{AMDGPU::DeviceHandle, PCI::BUS_INFO};

mod thermal;
use thermal::ThermalManager;

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
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields, default)]
struct Timing {
    intervals: Intervals,
    #[serde(rename = "burst-samples")]
    burst_samples: u8,
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
    normal: f32,
    burst: f32,
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
    lower: f32,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default)]
struct Thermal {
    max_safe_temp: f32,
    emergency_temp: f32,
    throttle_temp: f32,
    monitor_interval: u64,
    fan_control_index: usize,
    #[serde(rename = "fan-control")]
    fan_control: FanControl,
    pid: PID,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default)]
struct FanControl {
    enabled: bool,
    curve: Vec<(f32, u8)>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields, default)]
struct PID {
    kp: f32,
    ki: f32,
    kd: f32,
    setpoint: f32,
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
        }
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            intervals: Default::default(),
            burst_samples: 48,
            ramp_rates: Default::default(),
        }
    }
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            sample: 2000,
            adjust: 20_000,
            finetune: 1_000_000_000,
        }
    }
}

impl Default for RampRates {
    fn default() -> Self {
        Self {
            normal: 1.0,
            burst: 200.0,
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
            upper: 0.95,
            lower: 0.7,
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

            if (temp2 - temp1).abs() < f32::EPSILON {
                return speed1 as u8;
            }

            let factor = (temp - temp1) / (temp2 - temp1);
            let target_speed = speed1 + factor * (speed2 - speed1);
            return target_speed.round().clamp(0.0, 100.0) as u8;
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
    let info = dev_handle.device_info().map_err(IoError::from_raw_os_error)?;

    let min_engine_clock = info.min_engine_clock / 1000;
    let max_engine_clock = info.max_engine_clock / 1000;

    let min_freq = safe_points.first_key_value().map(|(&k, _)| k).unwrap_or(min_engine_clock as u16);
    let max_freq = safe_points.last_key_value().map(|(&k, _)| k).unwrap_or(max_engine_clock as u16);

    let mut pp_file = std::fs::OpenOptions::new().write(true).open(
        dev_handle.get_sysfs_path().map_err(IoError::from_raw_os_error)?.join("pp_od_clk_voltage"),
    )?;

    let (send, mut recv) = watch::channel(min_freq);

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

    let jh_gov: JoinHandle<()> = std::thread::spawn(move || {
        let mut curr_freq = min_freq;
        let mut target_freq = f32::from(min_freq);
        let mut samples: u64 = 0;
        let mut last_adjustment = Instant::now();
        let mut last_finetune = Instant::now();

        let burst_mask = if gov_config.burst_samples > 0 && gov_config.burst_samples < 64 {
            Some(!(u64::MAX << gov_config.burst_samples))
        } else if gov_config.burst_samples >= 64 {
            Some(u64::MAX)
        } else {
            None
        };

        loop {
            let res = dev_handle.read_mm_registers(GRBM_STATUS_REG).expect("Failed to read MM registers");
            let gui_busy = (res & (1 << GPU_ACTIVE_BIT)) > 0;
            samples = (samples << 1) | (gui_busy as u64);

            let busy_frac = (samples.count_ones() as f32) / 64.0;

            let burst = burst_mask.map(|mask| samples & mask == mask).unwrap_or(false);

            if burst {
                target_freq += gov_config.ramp_rates.burst * (gov_config.intervals.sample as f32 / 1000.0);
            } else if busy_frac > load_config.upper {
                target_freq += gov_config.ramp_rates.normal * (gov_config.intervals.sample as f32 / 1000.0);
            } else if busy_frac < load_config.lower {
                target_freq -= gov_config.ramp_rates.normal * (gov_config.intervals.sample as f32 / 1000.0);
            }

            target_freq = target_freq.clamp(f32::from(min_freq), f32::from(max_freq));

            if last_adjustment.elapsed() >= Duration::from_micros(gov_config.intervals.adjust) || burst {
                let target_freq_u16 = target_freq as u16;
                let diff = curr_freq.abs_diff(target_freq_u16);

                let is_finetune = last_finetune.elapsed() >= Duration::from_micros(gov_config.intervals.finetune);

                if (diff >= freq_config.adjust) || (is_finetune && diff >= freq_config.finetune) || burst {
                    send.send(target_freq_u16);
                    curr_freq = target_freq_u16;
                    if is_finetune { last_finetune = Instant::now(); }
                }
                last_adjustment = Instant::now();
            }

            std::thread::sleep(Duration::from_micros(gov_config.intervals.sample));
        }
    });

    let jh_set: JoinHandle<()> = std::thread::spawn(move || {
        loop {
            let freq = recv.wait();
            let vol = *safe_points.range(freq..).next().expect("Frequency is out of safe range").1;
            pp_file.write_all(format!("vc 0 {freq} {vol}").as_bytes()).expect("Failed to write to pp_od_clk_voltage");
            pp_file.write_all(b"c").expect("Failed to commit to pp_od_clk_voltage");
        }
    });

    jh_set.join().unwrap();
    jh_gov.join().unwrap();
    if let Some(jh) = thermal_jh {
        jh.join().unwrap();
    }

    Ok(())
}
