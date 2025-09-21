use std::{
    fs,
    io::{Error as IoError, ErrorKind},
    path::Path,
};
use glob::glob;

#[derive(Debug, Clone)]
pub struct ThermalSensor {
    pub name: String,
    pub temp_input: String,
}

#[derive(Debug, Clone)]
pub struct FanControl {
    pub name: String,
    pub pwm_path: Option<String>,
    pub enable_path: Option<String>,
}

#[derive(Debug)]
pub struct ThermalManager {
    pub sensors: Vec<ThermalSensor>,
    pub fans: Vec<FanControl>,
    pub nct6687_available: bool,
}

impl ThermalManager {
    pub fn new() -> Result<Self, IoError> {
        Self::new_with_root("/sys/class/hwmon")
    }

    pub fn new_with_root(hwmon_root: &str) -> Result<Self, IoError> {
        let mut sensors = Vec::new();
        let mut fans = Vec::new();
        let mut nct6687_available = false;

        let pattern = format!("{}/hwmon*", hwmon_root.trim_end_matches('/'));
        for entry in glob(&pattern).unwrap() {
            if let Ok(hwmon_path) = entry {
                if let Ok(name) = fs::read_to_string(hwmon_path.join("name")) {
                    let name = name.trim().to_string();
                    let path = hwmon_path.to_string_lossy().to_string();

                    if hwmon_path.join("temp1_input").exists() {
                        sensors.push(ThermalSensor {
                            name: name.clone(),
                            temp_input: hwmon_path.join("temp1_input").to_string_lossy().to_string(),
                        });
                    }

                    if name.starts_with("nct6687") || name.starts_with("nct6686") {
                        nct6687_available = true;
                        
                        for pwm_entry in glob(&format!("{}/pwm*", path)).unwrap_or_else(|_| glob("").unwrap()) {
                            if let Ok(pwm_path) = pwm_entry {
                                if pwm_path.to_string_lossy().contains("_enable") {
                                    continue;
                                }
                                
                                let pwm_name = pwm_path.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string();
                                
                                let enable_path = format!("{}_enable", pwm_path.to_string_lossy());
                                let enable_exists = Path::new(&enable_path).exists();

                                fans.push(FanControl {
                                    name: format!("{}_{}", name, pwm_name),
                                    pwm_path: Some(pwm_path.to_string_lossy().to_string()),
                                    enable_path: if enable_exists { Some(enable_path) } else { None },
                                });
                            }
                        }
                    }
                }
            }
        }

        println!("🌡️  Thermal Manager initialized:");
        println!("   Sensors found: {}", sensors.len());
        for sensor in &sensors {
            println!("     - {}", sensor.name);
        }
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
            sensors,
            fans,
            nct6687_available,
        })
    }

    pub fn read_temperature(&self, sensor_name: &str) -> Result<f32, IoError> {
        let sensor = self.sensors.iter()
            .find(|s| s.name == sensor_name)
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, format!("Sensor {} not found", sensor_name)))?;

        let temp_str = fs::read_to_string(&sensor.temp_input)?;
        let temp_millidegrees: i32 = temp_str.trim().parse()
            .map_err(|_| IoError::new(ErrorKind::InvalidData, "Invalid temperature data"))?;
        
        Ok(temp_millidegrees as f32 / 1000.0)
    }

    pub fn get_max_temperature(&self) -> Result<f32, IoError> {
        let mut max_temp: f32 = 0.0;
        
        for sensor in &self.sensors {
            if let Ok(temp) = self.read_temperature(&sensor.name) {
                max_temp = max_temp.max(temp);
            }
        }
        
        if max_temp == 0.0 {
            Err(IoError::new(ErrorKind::NotFound, "No temperature readings available"))
        } else {
            Ok(max_temp)
        }
    }
    pub fn set_fan_speed(&self, fan_index: usize, speed_percent: u8) -> Result<(), IoError> {
        if !self.nct6687_available {
            return Err(IoError::new(ErrorKind::Unsupported, "NCT6687 not available"));
        }

        let fan = self.fans.get(fan_index)
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "Fan index out of range"))?;

        let pwm_path = fan.pwm_path.as_ref()
            .ok_or_else(|| IoError::new(ErrorKind::NotFound, "PWM path not available"))?;

        let pwm_value = (speed_percent.min(100) as u16 * 255 / 100) as u8;
        
        if let Some(enable_path) = &fan.enable_path {
            fs::write(enable_path, "1")?;
        }
        
        fs::write(pwm_path, pwm_value.to_string())?;
        
        Ok(())
    }

    pub fn get_thermal_status(&self) -> ThermalStatus {
        let max_temp = self.get_max_temperature().unwrap_or(0.0);
        let amdgpu_temp = self.read_temperature("amdgpu").unwrap_or(0.0);
        let cpu_temp = self.read_temperature("k10temp").unwrap_or(0.0);

        ThermalStatus {
            max_temperature: max_temp,
            amdgpu_temperature: amdgpu_temp,
            cpu_temperature: cpu_temp,
        }
    }

    pub fn print_current_fan_speeds(&self) {
        if self.fans.is_empty() {
            println!("No fans detected");
            return;
        }

        for (i, fan) in self.fans.iter().enumerate() {
            let pwm_str = fan.pwm_path.as_ref()
                .and_then(|p| fs::read_to_string(p).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "N/A".to_string());

            println!(
                "- Fan {}: {} | PWM: {}",
                i,
                fan.name,
                pwm_str
            );
        }
    }

    pub fn get_primary_fan_info(&self, fan_index: usize) -> (Option<u8>, Option<usize>) {
        if self.fans.is_empty() {
            return (None, None);
        }

        let fan = self.fans.get(fan_index);
        if fan.is_none() {
            return (None, None);
        }
        let fan = fan.unwrap();

        let mut pwm_opt = None;
        if let Some(p_path) = &fan.pwm_path {
            if let Ok(pwm_str) = fs::read_to_string(p_path) {
                if let Ok(pwm) = pwm_str.trim().parse::<u8>() {
                    pwm_opt = Some(pwm);
                }
            }
        }

        (pwm_opt, Some(fan_index))
    }

    pub fn probe_fans(&self) {
        for (i, fan) in self.fans.iter().enumerate() {
            println!("--- PWM {}: {} ---", i, fan.name);
            if let Some(pwm) = &fan.pwm_path {
                 println!("Probing fan {}. Please observe the fan connected to this PWM output.", i);

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
        let prev = self.fans[idx].pwm_path.as_ref().and_then(|p| fs::read_to_string(p).ok());
        
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
}

#[derive(Debug, Clone)]
pub struct ThermalStatus {
    pub max_temperature: f32,
    pub amdgpu_temperature: f32,
    pub cpu_temperature: f32,
}

