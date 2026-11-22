//! Lightweight, non-panicking probes of the local GPU adapter and audio hardware sample rates
//! used when assembling diagnostic support reports.

/// Summary of the GPU adapter Ferrocore selects via `wgpu`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuProbe {
    pub name: String,
    pub backend: String,
    pub device_type: String,
    pub driver: String,
    pub driver_info: String,
    pub is_software: bool,
}

/// One enumerated OS audio endpoint with its default and supported sample rates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDeviceProbe {
    /// `"input"` or `"output"`.
    pub kind: String,
    pub name: String,
    pub is_default: bool,
    pub sample_rate_hz: Option<u32>,
    pub supported_sample_rates_hz: Vec<u32>,
    pub channels: Option<u16>,
}

/// Queries `wgpu` for the active graphics adapter without panicking on headless hosts.
pub fn probe_gpu() -> GpuProbe {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    #[cfg(target_os = "windows")]
    let opts =
        wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() };
    #[cfg(not(target_os = "windows"))]
    let opts = wgpu::RequestAdapterOptions::default();

    let adapter = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pollster::block_on(instance.request_adapter(&opts)).ok()
    }))
    .ok()
    .flatten();

    let Some(adapter) = adapter else {
        return GpuProbe {
            name: String::new(),
            backend: "None".into(),
            device_type: "Other".into(),
            driver: String::new(),
            driver_info: String::new(),
            is_software: true,
        };
    };

    let info = adapter.get_info();
    let lower = info.name.to_ascii_lowercase();
    let is_software = info.device_type == wgpu::DeviceType::Cpu
        || lower.contains("swiftshader")
        || lower.contains("llvmpipe")
        || lower.contains("softpipe")
        || lower.contains("basic render");

    GpuProbe {
        name: info.name,
        backend: format!("{:?}", info.backend),
        device_type: format!("{:?}", info.device_type),
        driver: info.driver,
        driver_info: info.driver_info,
        is_software,
    }
}

/// Enumerates connected OS audio inputs and outputs along with default & supported sample rates,
/// or returns `None` when device enumeration is unsupported on this OS target.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn probe_audio_devices() -> Option<Vec<AudioDeviceProbe>> {
    use cpal::traits::{DeviceTrait, HostTrait};

    const COMMON_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
    let host = cpal::default_host();
    let default_in = host.default_input_device().and_then(|d| d.description().ok().map(|n| n.to_string()));
    let default_out = host.default_output_device().and_then(|d| d.description().ok().map(|n| n.to_string()));

    let mut devices = Vec::new();
    if let Ok(list) = host.input_devices() {
        for dev in list {
            let Ok(desc) = dev.description() else { continue };
            let name = desc.to_string();
            let def_cfg = dev.default_input_config().ok();
            let mut supported: Vec<u32> = Vec::new();
            if let Ok(ranges) = dev.supported_input_configs() {
                for r in ranges {
                    let lo = r.min_sample_rate();
                    let hi = r.max_sample_rate();
                    for rate in COMMON_RATES {
                        if (lo..=hi).contains(&rate) && !supported.contains(&rate) {
                            supported.push(rate);
                        }
                    }
                }
                supported.sort_unstable();
            }
            devices.push(AudioDeviceProbe {
                kind: "input".into(),
                is_default: default_in.as_deref() == Some(name.as_str()),
                name,
                sample_rate_hz: def_cfg.as_ref().map(|c| c.sample_rate()),
                supported_sample_rates_hz: supported,
                channels: def_cfg.as_ref().map(|c| c.channels()),
            });
        }
    }

    if let Ok(list) = host.output_devices() {
        for dev in list {
            let Ok(desc) = dev.description() else { continue };
            let name = desc.to_string();
            let def_cfg = dev.default_output_config().ok();
            let mut supported: Vec<u32> = Vec::new();
            if let Ok(ranges) = dev.supported_output_configs() {
                for r in ranges {
                    let lo = r.min_sample_rate();
                    let hi = r.max_sample_rate();
                    for rate in COMMON_RATES {
                        if (lo..=hi).contains(&rate) && !supported.contains(&rate) {
                            supported.push(rate);
                        }
                    }
                }
                supported.sort_unstable();
            }
            devices.push(AudioDeviceProbe {
                kind: "output".into(),
                is_default: default_out.as_deref() == Some(name.as_str()),
                name,
                sample_rate_hz: def_cfg.as_ref().map(|c| c.sample_rate()),
                supported_sample_rates_hz: supported,
                channels: def_cfg.as_ref().map(|c| c.channels()),
            });
        }
    }

    Some(devices)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn probe_audio_devices() -> Option<Vec<AudioDeviceProbe>> {
    None
}

/// Opens a regular file at `path` without following a symlink on the final path component.
pub fn open_regular_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK: opening a FIFO with no writer would wait for one, before the regular-file
        // check below could refuse it. It has no effect on a regular file.
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?
    };
    #[cfg(not(unix))]
    let file = {
        let sym = std::fs::symlink_metadata(path)?;
        if sym.file_type().is_symlink() || !sym.file_type().is_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
        }
        std::fs::File::open(path)?
    };
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::open_regular_nofollow;

    #[test]
    fn a_fifo_is_refused_promptly_and_a_regular_file_still_opens() {
        let dir =
            std::env::temp_dir().join(format!("fc-fifo-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let p = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(open_regular_nofollow(&p).is_err());
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(3)),
            Ok(true),
            "a FIFO must be refused, not waited on"
        );
        let file = dir.join("log.txt");
        std::fs::write(&file, b"hello").unwrap();
        assert!(open_regular_nofollow(&file).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
