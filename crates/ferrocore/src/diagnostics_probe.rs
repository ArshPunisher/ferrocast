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

/// Enumerates connected OS audio inputs and outputs along with default & supported sample rates.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn probe_audio_devices() -> Vec<AudioDeviceProbe> {
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

    devices
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn probe_audio_devices() -> Vec<AudioDeviceProbe> {
    Vec::new()
}
