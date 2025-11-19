# BC-250 Rust Governor

A high-performance GPU frequency and thermal management daemon for AMD GPUs, written in Rust. This governor dynamically adjusts GPU clock speeds and voltages based on real-time load analysis, while managing thermals and fan curves.

## Features

- **Dynamic Frequency Scaling**: Adjusts GPU frequency based on workload with configurable ramp rates
- **Burst Detection**: Rapidly increases frequency when sustained high load is detected
- **Thermal Management**: Monitors GPU and CPU temperatures with emergency shutdown protection
- **Fan Curve Control**: Automated fan speed control based on temperature curves
- **Safe Voltage Tables**: Ensures stable operation with user-defined frequency/voltage pairs
- **Low Latency**: Optimized for minimal overhead and fast frequency transitions

## Requirements

- Linux system with AMD GPU
- libdrm_amdgpu
- nct6687 kernel module (for fan control)
- Rust toolchain (for building)

## Installation

### 1. Build the Project

```bash
# Navigate to the project directory
cd bc-250-rust-governor

# Build the release binary (optimized)
cargo build --release
```

The compiled binary will be at `target/release/bc-250-rust-governor`.

### 2. Install the Binary

```bash
# Copy the binary to system path
sudo cp target/release/bc-250-rust-governor /usr/local/bin/

# Make it executable
sudo chmod +x /usr/local/bin/bc-250-rust-governor
```

### 3. Setup Configuration

```bash
# Create configuration directory
sudo mkdir -p /etc/bc-250-rust-governor

# Copy the default configuration file
sudo cp default-config.toml /etc/bc-250-rust-governor/config.toml

# Edit the configuration (optional)
sudo nano /etc/bc-250-rust-governor/config.toml
```

### 4. Install and Enable the System Service

```bash
# Copy the systemd service file
sudo cp bc-250-rust-governor.service /etc/systemd/system/

# Reload systemd to recognize the new service
sudo systemctl daemon-reload

# Enable the service to start on boot
sudo systemctl enable bc-250-rust-governor

# Start the service now
sudo systemctl start bc-250-rust-governor

# Check if the service is running properly
sudo systemctl status bc-250-rust-governor
```

### 5. Verify Installation

```bash
# View real-time logs
journalctl -u bc-250-rust-governor -f

# Check for errors
journalctl -u bc-250-rust-governor -n 50
```

## Configuration

The governor is configured via a TOML file. By default, it looks for `/etc/bc-250-rust-governor/config.toml`.

### Safe Points (Frequency/Voltage Table)

Define stable frequency and voltage pairs for your GPU:

```toml
safe-points = [
    { frequency = 350, voltage = 570 },
    { frequency = 860, voltage = 600 },
    { frequency = 1090, voltage = 650 },
    { frequency = 2230, voltage = 1050 },
]
```

- `frequency`: GPU clock in MHz
- `voltage`: Core voltage in mV
- The governor will interpolate between points to find safe voltages

### Timing Configuration

```toml
[timing]
burst-samples = 20              # Samples needed to trigger burst mode
ramp-up-samples = 64            # Samples for calculating upward load
ramp-down-samples = 256         # Samples for calculating downward load
intervals = { sample = 2000, adjust = 8000, finetune = 50000 }
ramp-rates = { burst = 1000, up = 50, up-medium = 25, up-slow = 10, up-crawl = 2, down = 0.2 }
```

**Intervals** (in microseconds):
- `sample`: How often to check GPU activity (2ms default)
- `adjust`: Minimum time between large frequency changes (8ms)
- `finetune`: Minimum time between small frequency adjustments (50ms)

**Ramp Rates** (MHz per millisecond):
- `burst`: Frequency increase rate during burst mode
- `up`: Normal upward ramp rate (high load)
- `up-medium`: Medium load ramp rate
- `up-slow`: Slow load ramp rate
- `up-crawl`: Very light load ramp rate
- `down`: Downward ramp rate (idle)

### Frequency Thresholds

```toml
[frequency-thresholds]
adjust = 100      # MHz difference needed for adjust interval changes
finetune = 25     # MHz difference needed for finetune interval changes
```

### Load Targets

```toml
[load-target]
upper = 0.90      # Threshold for maximum ramp-up rate (90% busy)
medium = 0.80     # Threshold for medium ramp-up rate
slow = 0.70       # Threshold for slow ramp-up rate
crawl = 0.60      # Threshold for crawl ramp-up rate
lower = 0.40      # Below this, frequency ramps down
```

These values represent the percentage of samples where the GPU was active.

### Performance Mode (Gaming)

The governor can lock to maximum frequency while gaming, then automatically return to dynamic scaling when you exit the game.

```toml
[performance-mode]
enabled = true                              # Enable performance mode feature
control_file = "/tmp/bc250-max-performance" # File to check for activation
check_interval = 500                        # How often to check (ms)
```

When the `control_file` exists, the governor locks the GPU to maximum frequency. When removed, it returns to normal dynamic scaling.

### Thermal Configuration

```toml
[thermal]
monitor_interval = 1000        # Check temps every 1000ms
max_safe_temp = 85.0          # Warning threshold (°C)
emergency_temp = 95.0         # Emergency shutdown (°C)
fan_control_index = 1         # Fan device index to control

[thermal.fan-control]
enabled = true
curve = [
    [50.0, 10], # At 50°C, run fan at 10%
    [55.0, 20], # At 55°C, run fan at 20%
    [60.0, 30],
    [65.0, 40],
    [70.0, 50],
    [75.0, 60],
    [80.0, 70],
    [85.0, 80],
    [90.0, 90],
    [95.0, 100],
]
```

Each curve point is `[temperature_celsius, fan_speed_percent]`. The governor interpolates between points.

## Usage

### Running Manually

```bash
# With default config path
sudo bc-250-rust-governor /etc/bc-250-rust-governor/config.toml

# List available thermal sensors and fans
bc-250-rust-governor --list

# Show current fan speeds
bc-250-rust-governor --current-fan

# Test fan control by probing all fans
bc-250-rust-governor --probe-fans

# Pulse a specific fan (by index)
bc-250-rust-governor --pulse-fan 1
```

### Running as Service

```bash
# Start the service
sudo systemctl start bc-250-rust-governor

# View logs
journalctl -u bc-250-rust-governor -f

# Stop the service
sudo systemctl stop bc-250-rust-governor
```

### Gaming Mode (Max Performance)

For maximum performance in games, use the included wrapper script that locks the GPU to maximum frequency while playing:

#### 1. Install the Gaming Mode Script

```bash
# Copy the script to a system location
sudo cp bc250-gaming-mode.sh /usr/local/bin/

# Make it executable
sudo chmod +x /usr/local/bin/bc250-gaming-mode.sh
```

#### 2. Configure Steam Launch Options

For any game in your Steam library:

1. Right-click the game → **Properties**
2. Go to **General** → **Launch Options**
3. Add: `/usr/local/bin/bc250-gaming-mode.sh %command%`

Example with other tools (like MangoHUD):
```
MANGOHUD=1 /usr/local/bin/bc250-gaming-mode.sh %command%
```

Example with game launch arguments:
```
/usr/local/bin/bc250-gaming-mode.sh %command% -novid -console
```

#### 3. How It Works

- When the game starts, the script creates `/tmp/bc250-max-performance`
- The governor detects this file and locks GPU to maximum frequency
- You get consistent maximum performance throughout the gaming session
- When you exit the game, the file is removed automatically
- The governor returns to normal dynamic frequency scaling

#### 4. Manual Control (Advanced)

You can also control performance mode manually:

```bash
# Activate max performance mode
touch /tmp/bc250-max-performance

# Deactivate (return to normal)
rm /tmp/bc250-max-performance
```

#### 5. Custom Control File Path

Edit `/etc/bc-250-rust-governor/config.toml` to change the control file location:

```toml
[performance-mode]
enabled = true
control_file = "/tmp/my-custom-path"  # Change this
check_interval = 500
```

## Tuning Tips

1. **Finding Safe Points**: Start with conservative voltage values and gradually lower them while stress testing
2. **Burst Mode**: Increase `burst-samples` for less aggressive burst detection
3. **Smoothness**: Increase `ramp-down-samples` for smoother frequency transitions
4. **Responsiveness**: Decrease `sample` interval and increase `ramp-rates.up` for faster response
5. **Fan Curves**: Adjust temperature thresholds based on your cooling solution

## Troubleshooting

### Governor Not Changing Frequency

- Check permissions: The binary needs write access to `/sys/class/drm/card*/device/pp_od_clk_voltage`
- Verify safe-points are defined correctly
- Check logs: `journalctl -u bc-250-rust-governor -f`

### Fan Control Not Working

- List fans: `bc-250-rust-governor --list`
- Check if `nct6687` module is loaded: `lsmod | grep nct6687`
- Verify `fan_control_index` matches your desired fan
- Test manually: `bc-250-rust-governor --pulse-fan 1`

### High Temperatures

- Lower `max_safe_temp` threshold
- Adjust fan curve to be more aggressive (higher speeds at lower temps)
- Verify thermal paste and cooler mounting
- Reduce maximum frequency in safe-points

## License

MIT License - See LICENSE file for details

Copyright (c) 2025 Marcus Medom Ryding

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.
