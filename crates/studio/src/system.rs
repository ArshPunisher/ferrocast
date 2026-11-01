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

/// Ferrocast's CPU use (% of one core, since the last ask) and its memory (MB).
pub fn usage() -> (f64, u64) {
    static SYS: Mutex<Option<sysinfo::System>> = Mutex::new(None);
    let Ok(pid) = sysinfo::get_current_pid() else { return (0.0, 0) };
    let mut guard = SYS.lock().unwrap_or_else(|e| e.into_inner());
    let sys = guard.get_or_insert_with(sysinfo::System::new);
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map_or((0.0, 0), |p| (f64::from(p.cpu_usage()), p.memory() / (1024 * 1024)))
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
}
