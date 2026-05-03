use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

const METRICS_FNAME: &str = "gpu_metrics";
const PATCHED_METRICS_PATH: &str = "/dev/shm/patched_gpu_metrics";
const USAGE_OFFSET: usize = 0x1C; // Byte 28: average_gfx_activity in gpu_metrics_v2_x

/// Intercepts the kernel's gpu_metrics sysfs file via bind mount and injects
/// the governor's calculated GPU usage, fixing the broken 655% display in MangoHUD
/// caused by the BC-250 returning 0xFFFF in the average_gfx_activity field.
///
/// Technique: bind-mounts a regular file over the sysfs path, then unlinks the
/// regular file path so it lives only as an anonymous fd + the mount point.
/// Any process reading gpu_metrics (MangoHUD, etc.) reads the patched copy.
/// The governor's own fd to the original sysfs file is unaffected by the mount.
pub struct GpuUsageFix {
    /// fd to the real sysfs gpu_metrics (unaffected by our bind mount)
    real_file: File,
    /// fd to the anonymous patched file (living only under the bind mount)
    patched_file: File,
    /// path of the bind mount destination, needed for umount on shutdown
    path: String,
}

impl GpuUsageFix {
    pub fn start(sysfs_path: PathBuf) -> io::Result<Self> {
        let real_metrics_path = sysfs_path
            .join(METRICS_FNAME)
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Non-UTF8 sysfs path"))?;

        // Unmount any stale bind from a previous run
        let _ = umount_bind(&real_metrics_path);

        // Open the real sysfs file before the bind mount so this fd always
        // reads kernel data, not our patched copy
        let mut real_file = OpenOptions::new().read(true).open(&real_metrics_path)?;

        // Read the current 128-byte metrics blob as baseline for the patched copy
        let mut raw = [0u8; 128];
        real_file.seek(SeekFrom::Start(0))?;
        real_file.read(&mut raw)?;

        // Create the regular file that will shadow the sysfs path
        let mut patched_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(PATCHED_METRICS_PATH)?;
        patched_file.write_all(&raw)?;
        patched_file.flush()?;

        // Bind-mount the regular file over the sysfs path
        mount_bind(PATCHED_METRICS_PATH, &real_metrics_path)?;

        // Unlink the path — the fd stays alive (anonymous file trick).
        // No external process can accidentally write to it.
        fs::remove_file(PATCHED_METRICS_PATH)?;

        eprintln!("✅ GPU metrics fix active: {} shadowed with patched copy", real_metrics_path);

        Ok(Self {
            real_file,
            patched_file,
            path: real_metrics_path,
        })
    }

    /// Write the current GPU usage into the patched metrics file.
    /// `usage` is in percent (0.0–100.0).
    /// Reads the full real metrics blob first so all other fields stay current.
    pub fn set_usage_percent(&mut self, usage: f32) -> io::Result<()> {
        // average_gfx_activity is stored as basis points (0–10000) in a u16 LE
        let basis_points = (usage * 100.0).clamp(0.0, 10000.0).round() as u16;

        let mut raw = [0u8; 128];
        self.real_file.seek(SeekFrom::Start(0))?;
        let n = self.real_file.read(&mut raw)?;

        if n < USAGE_OFFSET + 2 {
            return Ok(());
        }

        raw[USAGE_OFFSET] = (basis_points & 0x00FF) as u8;
        raw[USAGE_OFFSET + 1] = (basis_points >> 8) as u8;

        self.patched_file.seek(SeekFrom::Start(0))?;
        self.patched_file.write_all(&raw)?;
        self.patched_file.flush()?;

        Ok(())
    }

    /// Remove the bind mount, restoring normal sysfs behaviour.
    pub fn shutdown(&self) -> io::Result<()> {
        let result = umount_bind(&self.path);
        if result.is_ok() {
            eprintln!("✅ GPU metrics fix removed: {} restored", self.path);
        }
        result
    }
}

fn mount_bind(src: &str, dst: &str) -> io::Result<()> {
    let status = Command::new("mount").args(["--bind", src, dst]).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mount --bind {src} {dst} failed with exit code: {status}"
        )))
    }
}

fn umount_bind(dst: &str) -> io::Result<()> {
    let status = Command::new("umount")
        .arg(dst)
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "umount {dst} failed with exit code: {status}"
        )))
    }
}
