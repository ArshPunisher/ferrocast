//! OBS's mux helper, run from Ferrocast.app (or where OBS is installed). libobs starts
//! `obs-ffmpeg-mux` from the folder of the program it runs in (here, Ferrocast's), for MKV,
//! FLV, TS and fragmented MP4/MOV recordings. This stands in for it: it runs OBS's own helper
//! with the same arguments, input and output, so its exit is the helper's exit.

use std::path::PathBuf;
use std::process::ExitCode;

/// OBS's helper: the one Ferrocast carries (Contents/Helpers on a Mac, obs/bin/64bit on
/// Windows), else an installed OBS's.
fn helper() -> Option<PathBuf> {
    // Linux has no OBS helper to look for, so nothing is added there.
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(unused_mut))]
    let mut places: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        // …/Contents/MacOS/obs-ffmpeg-mux (this) → …/Contents/Helpers/obs-ffmpeg-mux
        if let Some(contents) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.to_path_buf())) {
            places.push(contents.join("Helpers/obs-ffmpeg-mux"));
        }
        places.push(PathBuf::from("/Applications/OBS.app/Contents/MacOS/obs-ffmpeg-mux"));
        if let Some(home) = std::env::var_os("HOME") {
            places.push(PathBuf::from(home).join("Applications/OBS.app/Contents/MacOS/obs-ffmpeg-mux"));
        }
    }
    #[cfg(windows)]
    {
        // …\Ferrocast\obs-ffmpeg-mux.exe (this) → …\Ferrocast\obs\bin\64bit\obs-ffmpeg-mux.exe
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.to_path_buf())) {
            places.push(dir.join("obs").join("bin").join("64bit").join("obs-ffmpeg-mux.exe"));
        }
        for var in ["ProgramFiles", "ProgramW6432"] {
            if let Some(p) = std::env::var_os(var) {
                places.push(PathBuf::from(p).join("obs-studio/bin/64bit/obs-ffmpeg-mux.exe"));
            }
        }
    }
    places.into_iter().find(|p| p.is_file())
}

fn main() -> ExitCode {
    let Some(exe) = helper() else {
        eprintln!("obs-ffmpeg-mux: OBS's mux helper is missing from Ferrocast.app (reinstall Ferrocast)");
        return ExitCode::FAILURE;
    };
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Becomes OBS's helper: the same process, pipes and exit status.
        let err = std::process::Command::new(&exe).args(&args).exec();
        eprintln!("obs-ffmpeg-mux: {err}");
        ExitCode::FAILURE
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&exe).args(&args).status() {
            Ok(s) => ExitCode::from(u8::try_from(s.code().unwrap_or(1)).unwrap_or(1)),
            Err(e) => {
                eprintln!("obs-ffmpeg-mux: {e}");
                ExitCode::FAILURE
            }
        }
    }
}
