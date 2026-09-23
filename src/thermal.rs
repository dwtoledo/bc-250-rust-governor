use glob::glob;
use std::{
    fs,
    io::{Error as IoError, ErrorKind},
    path::Path,
};

#[derive(Debug, Clone)]
pub struct FanControl {
    pub name: String,
    pub pwm_path: Option<String>,
    pub enable_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ThermalManager {
    pub fans: Vec<FanControl>,
    pub nct6687_available: bool,
}

impl ThermalManager {
    pub fn new() -> Result<Self, IoError> {
        Self::new_with_root("/sys/class/hwmon")
    }

    pub fn new_with_root(hwmon_root: &str) -> Result<Self, IoError> {
        let mut fans = Vec::new();
        let mut nct6687_available = false;

        let pattern = format!("{}/hwmon*", hwmon_root.trim_end_matches('/'));
        for hwmon_path in glob(&pattern).unwrap().flatten() {
            if let Ok(name) = fs::read_to_string(hwmon_path.join("name")) {
                let name = name.trim().to_string();
                let path = hwmon_path.to_string_lossy().to_string();

                if name.starts_with("nct6687") || name.starts_with("nct6686") {
                    nct6687_available = true;

                    for pwm_path in glob(&format!("{}/pwm*", path))
                        .unwrap_or_else(|_| glob("").unwrap())
                        .flatten()
                    {
                        if pwm_path.to_string_lossy().contains("_enable") {
                            continue;
                        }

                        let pwm_name = pwm_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();

                        let enable_path = format!("{}_enable", pwm_path.to_string_lossy());
                        let enable_exists = Path::new(&enable_path).exists();

                        fans.push(FanControl {
                            name: format!("{}_{}", name, pwm_name),
                            pwm_path: Some(pwm_path.to_string_lossy().to_string()),
                            enable_path: if enable_exists {
                                Some(enable_path)
                            } else {
                                None
                            },
                        });
                    }
                }
            }
        }

        println!("🌡️  Thermal Manager initialized:");
        println!("   Fans found: {}", fans.len());
        for fan in &fans {
            println!("     - {}", fan.name);
        }
        println!("   NCT6687 available: {}", nct6687_available);

        if !nct6687_available {
            println!("⚠️  NCT6687 not detected. Fan control disabled.");
            println!("   To enable: sudo modprobe nct6687");
        }

        Ok(ThermalManager {
            fans,
            nct6687_available,
        })
    }

    pub fn set_fan_speed(&self, fan_index: usize, speed_percent: u8) -> Result<(), IoError> {
        if !self.nct6687_available {
            return Err(IoError::new(
                ErrorKind::Unsupported,
                "NCT6687 not available",
            ));
        }

        let fan = self
            .fans
            .get(fan_index)
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "Fan index out of range"))?;

        let pwm_path = fan
            .pwm_path
            .as_ref()
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "PWM path not available"))?;

        let pwm_value = (speed_percent.min(100) as u16 * 255 / 100) as u8;

        if let Some(enable_path) = &fan.enable_path {
            fs::write(enable_path, "1")?;
        }

        fs::write(pwm_path, pwm_value.to_string())?;

        Ok(())
    }

    pub fn print_current_fan_speeds(&self) {
        if self.fans.is_empty() {
            println!("No fans detected");
            return;
        }

        for (i, fan) in self.fans.iter().enumerate() {
            let pwm_str = fan
                .pwm_path
                .as_ref()
                .and_then(|p| fs::read_to_string(p).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "N/A".to_string());

            println!("- Fan {}: {} | PWM: {}", i, fan.name, pwm_str);
        }
    }

    pub fn get_primary_fan_info(&self, fan_index: usize) -> Option<u8> {
        let fan = self.fans.get(fan_index)?;
        let pwm_str = fs::read_to_string(fan.pwm_path.as_ref()?).ok()?;
        pwm_str.trim().parse().ok()
    }

    pub fn probe_fans(&self) {
        for (i, fan) in self.fans.iter().enumerate() {
            println!("--- PWM {}: {} ---", i, fan.name);
            if let Some(pwm) = &fan.pwm_path {
                println!(
                    "Probing fan {}. Please observe the fan connected to this PWM output.",
                    i
                );

                if let Some(en_path) = &fan.enable_path {
                    let _ = fs::write(en_path, "1");
                }

                println!("Setting fan to 40% for 5 seconds...");
                let _ = fs::write(pwm, "102");
                std::thread::sleep(std::time::Duration::from_secs(5));

                println!("Setting fan to 0%...");
                let _ = fs::write(pwm, "0");

                println!("Probe for fan {} complete.", i);
            } else {
                println!("No pwm path for this fan");
            }
        }
    }

    pub fn pulse_fan(&self, idx: usize) -> Result<(), IoError> {
        if idx >= self.fans.len() {
            eprintln!("Invalid fan index");
            return Ok(());
        }
        println!("Pulsing fan {}: 25% for 5s then 100% for 5s", idx);
        let prev = self.fans[idx]
            .pwm_path
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok());

        self.set_fan_speed(idx, 25)?;
        std::thread::sleep(std::time::Duration::from_secs(5));

        self.set_fan_speed(idx, 100)?;
        std::thread::sleep(std::time::Duration::from_secs(5));

        if let Some(prev_txt) = prev {
            if let Ok(val) = prev_txt.trim().parse::<u8>() {
                let percent = ((val as u16) * 100 / 255) as u8;
                self.set_fan_speed(idx, percent).ok();
            }
        }
        println!("Pulse complete");
        Ok(())
    }

    pub fn restore_auto_fan_control(&self) -> Result<(), IoError> {
        if !self.nct6687_available {
            return Ok(());
        }

        for (i, fan) in self.fans.iter().enumerate() {
            if let Some(enable_path) = &fan.enable_path {
                match fs::write(enable_path, "2") {
                    Ok(_) => println!("🔄 Fan {} restored to automatic control", i),
                    Err(e) => eprintln!("⚠️  Failed to restore fan {} to auto: {}", i, e),
                }
            }
        }

        Ok(())
    }
}

pub const GPU_TEMP_READ_FAILURES: u32 = 3;
const TEMP_MIN_C: f32 = -40.0;
const TEMP_MAX_C: f32 = 150.0;

/// Temperature of the GPU device the governor opened.
///
/// The decision value is `temp2_input` (hotspot) when the kernel exposes it,
/// otherwise `temp1_input` (edge) of that same device.
#[derive(Debug, Clone)]
pub struct GpuTempSource {
    edge_path: Option<std::path::PathBuf>,
    decision_path: Option<std::path::PathBuf>,
    decision_is_hotspot: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuTempReading {
    pub edge_c: Option<f32>,
    pub hotspot_c: Option<f32>,
    pub decision_c: f32,
}

impl GpuTempSource {
    pub fn open(device_sysfs: &Path) -> Self {
        let Some(hwmon) = find_gpu_hwmon(device_sysfs) else {
            eprintln!("⚠️  GPU hwmon not found under {}", device_sysfs.display());
            return Self {
                edge_path: None,
                decision_path: None,
                decision_is_hotspot: false,
            };
        };

        let edge = hwmon.join("temp1_input");
        let hotspot = hwmon.join("temp2_input");
        let edge_ok = edge.is_file();
        let hotspot_ok = hotspot.is_file();

        if hotspot_ok {
            eprintln!(
                "🌡️  GPU temperature: hotspot {} (primary){}",
                hotspot.display(),
                if edge_ok {
                    format!(", edge {}", edge.display())
                } else {
                    String::new()
                }
            );
        } else if edge_ok {
            eprintln!(
                "🌡️  GPU hotspot temp2_input not found under {}. Using edge {} as primary.",
                hwmon.display(),
                edge.display()
            );
        } else {
            eprintln!("⚠️  GPU temperature files not found under {}", hwmon.display());
        }

        Self {
            edge_path: edge_ok.then(|| edge.clone()),
            decision_path: if hotspot_ok {
                Some(hotspot)
            } else if edge_ok {
                Some(edge)
            } else {
                None
            },
            decision_is_hotspot: hotspot_ok,
        }
    }

    #[cfg(test)]
    fn uses_hotspot(&self) -> bool {
        self.decision_is_hotspot
    }

    pub fn read(&self) -> Result<GpuTempReading, IoError> {
        let decision_path = self
            .decision_path
            .as_ref()
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "GPU temperature sensor not found"))?;
        let decision_c = read_temp_c(decision_path)?;
        let edge_c = if self.decision_is_hotspot {
            self.edge_path
                .as_ref()
                .and_then(|path| read_temp_c(path).ok())
        } else {
            Some(decision_c)
        };
        let hotspot_c = self.decision_is_hotspot.then_some(decision_c);
        Ok(GpuTempReading {
            edge_c,
            hotspot_c,
            decision_c,
        })
    }
}

/// Returns true when consecutive failures reach the shutdown threshold.
pub fn temp_failure_should_stop(consecutive: &mut u32) -> bool {
    *consecutive = consecutive.saturating_add(1);
    *consecutive >= GPU_TEMP_READ_FAILURES
}

fn find_gpu_hwmon(device_sysfs: &Path) -> Option<std::path::PathBuf> {
    let mut dirs: Vec<_> = fs::read_dir(device_sysfs.join("hwmon"))
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join("temp2_input").is_file() || path.join("temp1_input").is_file())
        .collect();
    dirs.sort();
    dirs.into_iter().next()
}

fn read_temp_c(path: &Path) -> Result<f32, IoError> {
    let raw = fs::read_to_string(path)?;
    let millidegrees: i64 = raw.trim().parse().map_err(|_| {
        IoError::new(
            ErrorKind::InvalidData,
            format!("{} did not contain an integer", path.display()),
        )
    })?;
    let celsius = millidegrees as f32 / 1000.0;
    if !(TEMP_MIN_C..=TEMP_MAX_C).contains(&celsius) {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            format!("{} reported an implausible {celsius}°C", path.display()),
        ));
    }
    Ok(celsius)
}

pub fn calculate_fan_speed(temp: f32, curve: &[(f32, u8)]) -> u8 {
    if curve.is_empty() {
        return 0;
    }

    if temp <= curve[0].0 {
        return curve[0].1;
    }

    let last = curve[curve.len() - 1];
    if temp >= last.0 {
        return last.1;
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

    last.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_hwmon() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let hwmon = std::env::temp_dir()
            .join(format!("bc250-gpu-temp-{nanos}"))
            .join("hwmon")
            .join("hwmon0");
        fs::create_dir_all(&hwmon).unwrap();
        hwmon
    }

    #[test]
    fn hotspot_is_the_decision_temperature() {
        let hwmon = scratch_hwmon();
        fs::write(hwmon.join("temp1_input"), "70000\n").unwrap();
        fs::write(hwmon.join("temp2_input"), "95000\n").unwrap();
        let device = hwmon.parent().unwrap().parent().unwrap();

        let source = GpuTempSource::open(device);
        let reading = source.read().unwrap();

        assert!(source.uses_hotspot());
        assert_eq!(reading.decision_c, 95.0);
        assert_eq!(reading.edge_c, Some(70.0));
        assert_eq!(reading.hotspot_c, Some(95.0));

        let _ = fs::remove_dir_all(device);
    }

    #[test]
    fn edge_of_the_same_gpu_is_used_when_hotspot_is_absent() {
        let hwmon = scratch_hwmon();
        fs::write(hwmon.join("temp1_input"), "80000\n").unwrap();
        let device = hwmon.parent().unwrap().parent().unwrap();

        let source = GpuTempSource::open(device);
        let reading = source.read().unwrap();

        assert!(!source.uses_hotspot());
        assert_eq!(reading.decision_c, 80.0);
        assert_eq!(reading.edge_c, Some(80.0));
        assert_eq!(reading.hotspot_c, None);

        let _ = fs::remove_dir_all(device);
    }

    #[test]
    fn implausible_reading_is_an_error() {
        let hwmon = scratch_hwmon();
        fs::write(hwmon.join("temp2_input"), "200000\n").unwrap();
        let device = hwmon.parent().unwrap().parent().unwrap();

        let source = GpuTempSource::open(device);
        assert!(source.read().is_err());

        let _ = fs::remove_dir_all(device);
    }

    #[test]
    fn third_consecutive_failure_stops() {
        let mut failures = 0;
        assert!(!temp_failure_should_stop(&mut failures));
        assert!(!temp_failure_should_stop(&mut failures));
        assert!(temp_failure_should_stop(&mut failures));
    }

    #[test]
    fn fan_curve_holds_the_ends_and_steps_between_points() {
        assert_eq!(calculate_fan_speed(40.0, &[]), 0);
        let curve = [(50.0, 10u8), (60.0, 30u8)];
        assert_eq!(calculate_fan_speed(40.0, &curve), 10);
        assert_eq!(calculate_fan_speed(50.0, &curve), 10);
        assert_eq!(calculate_fan_speed(55.0, &curve), 20);
        assert_eq!(calculate_fan_speed(70.0, &curve), 30);
    }
}
