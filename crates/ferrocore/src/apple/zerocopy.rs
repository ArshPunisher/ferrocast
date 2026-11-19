//! A screen frame on the card with no copy: the IOSurface ScreenCaptureKit drew into becomes a
//! Metal texture, wrapped for wgpu. Before, each frame was copied into a staging buffer (8 MB a
//! frame of a 1080p display, more for a 4K one) and uploaded. The frame's buffer is kept until
//! wgpu lets the texture go, which is after the card has finished reading it, so the system can't
//! reuse it while it's being drawn.

use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};

use super::screen::Surface;

/// Off when FERROCORE_COPY_SCREEN is set (to compare against the copying path).
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("FERROCORE_COPY_SCREEN").is_none())
}

/// `s` as a wgpu texture (BGRA, viewable as sRGB too), or None when the card isn't Metal or the
/// frame has no IOSurface: the caller copies it then.
pub fn texture(device: &wgpu::Device, s: &Surface, label: &str) -> Option<wgpu::Texture> {
    let (w, h) = s.size();
    let io = s.io_surface()?;
    // Kept alive until wgpu drops the texture (after the GPU is done with it). A Mutex makes the
    // Send-only buffer Sync, as wgpu's callback must be.
    let keep = std::sync::Mutex::new(Some(s.clone()));
    // SAFETY: `as_hal` is only used to reach the Metal device, which wgpu keeps alive while the
    // returned guard is held; nothing else of wgpu's internal state is touched.
    let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
    // SAFETY: a plain 2D BGRA8 descriptor for the IOSurface's own size.
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            w as usize,
            h as usize,
            false,
        )
    };
    // Read by shaders, and through an sRGB view as well as the plain one (compose.rs `view`).
    desc.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::PixelFormatView);
    let raw = hal_device.raw_device().newTextureWithDescriptor_iosurface_plane(&desc, &io, 0)?;
    // SAFETY: `raw` is a 2D BGRA8Unorm texture of w×h with one mip level and one layer, made
    // just above from a live IOSurface; the surface's buffer stays retained (`keep`) until
    // wgpu destroys the texture, which it does only after the GPU has finished with it.
    let hal_texture = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            raw,
            wgpu::TextureFormat::Bgra8Unorm,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent { width: w, height: h, depth: 1 },
            Some(Box::new(move || drop(keep))),
        )
    };
    drop(hal_device);
    // SAFETY: the descriptor matches the hal texture made above (size, format, one mip, 2D);
    // only TEXTURE_BINDING is asked for, which ShaderRead allows.
    Some(unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Metal>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[wgpu::TextureFormat::Bgra8UnormSrgb],
            },
            // Its pixels are there already: it starts as something shaders read.
            wgpu::TextureUses::RESOURCE,
        )
    })
}
