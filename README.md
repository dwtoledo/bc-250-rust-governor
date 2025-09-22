# BC-250 Rust Governor - A Configuration Guide for the BC-250

This document provides a comprehensive guide to configuring the `bc-250-rust-governor`, specifically for use with the **AMD BC-250 APU**. This hardware, originally designed for cryptocurrency mining, can be repurposed for gaming, but it requires careful power and thermal management to function correctly. This governor provides the necessary intelligent control to unlock its gaming potential.

## Core Concepts

Before diving into the parameters, it's important to understand the core logic of the governor.

### Load Calculation

The governor does not use a traditional GPU utilization metric. Instead, it calculates "load" by directly polling the BC-250's hardware registers (`GRBM_STATUS_REG`) at a high frequency (e.g., every 3 milliseconds). It checks if the GPU's graphics engine was active at that exact moment.

The "load" is the percentage of "active" responses over a rolling window of the last 64 samples. For example, if the GPU was active in 58 of the last 64 checks, the load is calculated as `58 / 64 = 0.90` (or 90%).

This method provides a very direct and low-level view of how demanded the GPU is.

### Ramp-Up, Ramp-Down, and Burst

- **Ramp-Down**: When the calculated load drops below a defined threshold (`lower`), the governor starts to decrease the GPU frequency at a "normal" rate.
- **Ramp-Up**: When the load exceeds a defined threshold (`upper`), the governor starts to increase the GPU frequency at the same "normal" rate.
- **Burst Mode**: To achieve rapid frequency increases when high performance is suddenly needed (e.g., starting a game), a "burst" mode is used. If the load remains high for a specific, uninterrupted "confirmation time", the governor switches to a much faster "burst" ramp rate.

## Configuration (`default-config.toml`)

All tuning is done in the `default-config.toml` file, which will be copied to `/etc/bc-250-rust-governor/config.toml` during setup.

### `safe-points`

This is the most critical section for the safety and stability of your BC-250.

```toml
# Example values for a BC-250
safe-points = [
    { frequency = 350, voltage = 570 },
    { frequency = 860, voltage = 600 },
    ...
    { frequency = 2230, voltage = 1050 },
]
```

- **Purpose**: This table defines the Voltage/Frequency curve for the BC-250. For any given frequency target set by the governor, it looks up the corresponding voltage to apply.
- **Safety**: The stability of your system depends on providing a safe and stable voltage curve. The governor itself is safe; the risk lies in an improperly configured voltage curve.

### `[timing]`

This section controls all time-related aspects of the governor's behavior.

```toml
[timing]
burst-samples = 12
intervals = { sample = 3000, adjust = 1000000, finetune = 5000000 }
ramp-rates = { normal = 0.246, burst = 1.23 }
```

- `intervals.sample`: The interval, in **microseconds**, between each GPU load check. `3000` means a check is performed every 3ms.
- `burst-samples`: The number of consecutive "load-is-high" samples required to trigger burst mode. This defines the **Confirmation Time**.
  - **Formula**: `Confirmation Time (ms) = burst-samples * (intervals.sample / 1000)`
- `ramp-rates`: Defines the rate of frequency change.
  - `normal`: Used for ramping down and for the initial, slow ramp-up.
  - `burst`: Used for the fast ramp-up after the confirmation time is met.
  - **Formula**: `Ramp Speed (MHz/s) = ramp_rate_value * 1000`

### `[frequency-thresholds]`

This section prevents tiny, insignificant frequency changes, reducing system "noise".

- `adjust`: The minimum frequency change (in MHz) required to apply an update.
- `finetune`: A smaller threshold that is only used after a longer `finetune` interval has passed, allowing for minor, infrequent adjustments to "settle" the clock.

### `[load-target]`

This section defines the load thresholds that trigger frequency changes.

```toml
[load-target]
upper = 0.95
lower = 0.65
```

- `upper`: When the average load exceeds this value (e.g., 95%), the governor starts ramping the frequency up.
- `lower`: When the average load drops below this value (e.g., 65%), the governor starts ramping the frequency down. This is the key parameter to tune for eliminating clock oscillations in games.

### `[thermal]`

This section provides thermal protection and fan control, which is critical for the BC-250.

- `max_safe_temp`, `emergency_temp`: Temperature limits to prevent overheating.
- `fan_control`: Allows for defining a custom fan curve based on temperature. Proper fan control is essential as the BC-250's default behavior is not suitable for gaming workloads.

## Installation and Setup

Follow these steps to build the governor and run it as a systemd service on your BC-250.

### Building from Source

**Prerequisites:**
- A working Rust development environment, including `rustc` and `cargo`.

1.  Clone the repository and navigate into the project directory.
2.  Build the project in release mode:
    ```sh
    cargo build --release
    ```
3.  The compiled binary will be available at `target/release/bc-250-rust-governor`.

### Systemd Service Setup

The included `bc-250-rust-governor.service` file is configured to run the governor as a system-wide service.

**1. Copy the Binary:**
Copy the compiled binary to a location in your system's PATH. The service file expects it to be in `/usr/local/bin/`.
```sh
sudo cp target/release/bc-250-rust-governor /usr/local/bin/
```

**2. Copy the Configuration File:**
The service expects a global configuration file. You must create the directory and copy your tuned config file there.
```sh
sudo mkdir -p /etc/bc-250-rust-governor/
sudo cp default-config.toml /etc/bc-250-rust-governor/config.toml
```
**Note:** From now on, all changes to the configuration must be made to `/etc/bc-250-rust-governor/config.toml`.

**3. Install the Fan Controller Driver (Mandatory):**
The BC-250 uses a **Nuvoton NCT6687** chip for temperature monitoring and fan control. The driver for this chip is **required** for the governor's thermal management to function. The service file will automatically load this driver (`modprobe nct6687`), but it must be installed first.

- On Arch Linux, for example, the driver can be installed from the AUR. You will need to do this step before proceeding.
  ```
  # Example for Arch Linux users:
  https://aur.archlinux.org/packages/nct6687d-dkms-git
  https://github.com/Fred78290/nct6687d
  ```

**4. Copy the Service File:**
Copy the service unit file to the systemd directory.
```sh
sudo cp bc-250-rust-governor.service /etc/systemd/system/
```

**5. Enable and Start the Service:**
Reload the systemd daemon to recognize the new service, then enable it to start on boot and start it immediately.
```sh
sudo systemctl daemon-reload
sudo systemctl enable bc-250-rust-governor.service
sudo systemctl start bc-250-rust-governor.service
```

**6. Check the Status:**
You can check if the service is running correctly using the status command.
```sh
sudo systemctl status bc-250-rust-governor.service
```

## Tuning Profiles for the BC-250

Here are a few example profiles for different user preferences. The "Balanced" profile is the one we developed through extensive tuning.

---

### Profile 1: Balanced & Stable (Recommended - I TESTED)

This profile is the result of fine-tuning for a responsive yet highly stable experience. It prioritizes eliminating clock fluctuations during gameplay while still providing a fast ramp-up when needed.

**Configuration:**
```toml
[timing]
burst-samples = 12
intervals = { sample = 3000, adjust = 1000000, finetune = 5000000 }
ramp-rates = { normal = 0.246, burst = 1.23 }

[load-target]
upper = 0.95
lower = 0.65
```

**Behavior:**
- **Confirmation Time**: `12 * 3ms = 36ms`
- **Normal Ramp (Up/Down)**: `246 MHz/s` (Takes ~5 seconds to climb/descend 1230 MHz).
- **Burst Ramp**: `1230 MHz/s` (Takes 1 second to climb 1230 MHz).

---

### Profile 2: Aggressive Performance (I DID NOT TEST, this is an AI suggestion)

This profile prioritizes the fastest possible response time, making the clock jump to the maximum almost instantly. It may be less smooth than the balanced profile.

**Configuration:**
```toml
[timing]
burst-samples = 10
intervals = { sample = 2000, adjust = 1000000, finetune = 5000000 }
ramp-rates = { normal = 1.0, burst = 200.0 }

[load-target]
upper = 0.95
lower = 0.75
```

**Behavior:**
- **Confirmation Time**: `10 * 2ms = 20ms`
- **Normal Ramp (Up/Down)**: `1000 MHz/s`
- **Burst Ramp**: `200,000 MHz/s` (Effectively instantaneous).

---

### Profile 3: Smooth & Responsive (I DID NOT TEST, this is an AI suggestion)

This profile is a good middle-ground, offering a faster-than-default response without being as aggressive as the profile above. It uses a slightly longer confirmation time to smooth out minor load variations.

**Configuration:**
```toml
[timing]
burst-samples = 20
intervals = { sample = 4000, adjust = 1000000, finetune = 5000000 }
ramp-rates = { normal = 0.5, burst = 10.0 }

[load-target]
upper = 0.95
lower = 0.80
```

**Behavior:**
- **Confirmation Time**: `20 * 4ms = 80ms`
- **Normal Ramp (Up/Down)**: `500 MHz/s`
- **Burst Ramp**: `10,000 MHz/s` (Takes ~0.12 seconds to climb 1230 MHz).

# Thanks
Thanks for:
https://github.com/Fred78290/nct6687d (sensor's driver)
https://github.com/Magnap/cyan-skillfish-governor (the first Rust project)

And all BC-250 community from Discord!!
