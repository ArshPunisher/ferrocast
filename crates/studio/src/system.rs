//! What the Studio's stats show about this computer, with no libobs: free disk space where
//! recordings go, and Ferrocast's own CPU and memory use.

use std::path::Path;
use std::sync::Mutex;

/// Free space on the disk holding `dir`, in megabytes (0 if it can't be told).
pub fn free_disk_mb(dir: &Path) -> u64 {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    // Windows's full form (\\?\C:\…) doesn't match how disks are listed (C:\): plain it.
    #[cfg(windows)]
    let dir = std::path::PathBuf::from(dir.display().to_string().trim_start_matches(r"\\?\"));
    // The disk mounted deepest above the folder.
    disks
        .list()
        .iter()
        .filter(|d| dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map_or(0, |d| d.available_space() / (1024 * 1024))
}

/// Ferrocast's CPU use (since the last ask) as a share of the whole computer, as libobs's meter
/// (and OBS's status bar) gives it: one core busy on a 10-core Mac is 10%. Measured per core it
/// read ten times OBS's figure for the same work, and the 85%/90% warnings meant the machine.
/// And its memory (MB).
pub fn usage() -> (f64, u64) {
    static SYS: Mutex<Option<sysinfo::System>> = Mutex::new(None);
    let Ok(pid) = sysinfo::get_current_pid() else { return (0.0, 0) };
    let mut guard = SYS.lock().unwrap_or_else(|e| e.into_inner());
    let sys = guard.get_or_insert_with(sysinfo::System::new);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64;
    sys.process(pid).map_or((0.0, 0), |p| (f64::from(p.cpu_usage()) / cores, p.memory() / (1024 * 1024)))
}

/// Overall CPU usage across the entire machine (`0..=100` %), for auto-config benchmarking.
pub fn machine_cpu_pct() -> f64 {
    static SYS: Mutex<Option<(sysinfo::System, std::time::Instant, f64)>> = Mutex::new(None);
    let mut guard = SYS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.get_or_insert_with(|| {
        let mut s = sysinfo::System::new();
        s.refresh_cpu_usage();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        s.refresh_cpu_usage();
        let pct = f64::from(s.global_cpu_usage()).clamp(0.0, 100.0);
        (s, std::time::Instant::now(), pct)
    });
    if entry.1.elapsed() >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL {
        entry.0.refresh_cpu_usage();
        entry.1 = std::time::Instant::now();
        entry.2 = f64::from(entry.0.global_cpu_usage()).clamp(0.0, 100.0);
    }
    entry.2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disk_and_this_process_are_measured() {
        assert!(free_disk_mb(&std::env::temp_dir()) > 0, "the temp folder's disk has room");
        let (_, mb) = usage();
        assert!(mb > 0, "this process uses some memory");
    }

    #[test]
    fn h3_machine_cpu_pct_first_call_samples_real_delta() {
        let cpu = machine_cpu_pct();
        assert!(cpu > 0.0 && cpu <= 100.0, "cold first call to machine_cpu_pct() returned {cpu} (0 ms refresh bug)");
    }
}

/// About how many pixels a second one of the Mac's video encode engines does in H.264 or HEVC:
/// one 3840×2160 picture at 60 fps (measured on an M4, 19 Nov: one 4K60 at 1.0x, two at 0.5x
/// each, H.264 and HEVC alike).
pub const MPX_PER_ENGINE: f64 = 3840.0 * 2160.0 * 60.0 / 1e6;

/// The Mac's video encode engines (Apple's specs: one on M1-M4 and their Pro, two on Max, four
/// on Ultra); None off Apple silicon, where it isn't known.
pub fn video_engines() -> Option<u32> {
    static N: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let out = std::process::Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]).output().ok()?;
        engines_of(&String::from_utf8_lossy(&out.stdout))
    })
}

/// [`video_engines`] from the chip's name ("Apple M4 Max").
fn engines_of(chip: &str) -> Option<u32> {
    let chip = chip.trim();
    if !chip.starts_with("Apple M") {
        return None;
    }
    Some(if chip.ends_with("Ultra") {
        4
    } else if chip.ends_with("Max") {
        2
    } else {
        1
    })
}

#[cfg(test)]
mod engine_count_tests {
    use super::*;

    #[test]
    fn the_macs_video_engines_come_from_its_chip() {
        assert_eq!(engines_of("Apple M4"), Some(1));
        assert_eq!(engines_of("Apple M3 Pro"), Some(1));
        assert_eq!(engines_of("Apple M2 Max"), Some(2));
        assert_eq!(engines_of("Apple M1 Ultra\n"), Some(4));
        assert_eq!(engines_of("Intel(R) Core(TM) i9"), None);
    }
}
