//! Offscreen surface → wgpu texture.
//!
//! `GtkOffscreenWindow` paints into an X11-backed cairo surface, so its pixels
//! cannot be read directly. Blitting it into an ARGB32 `ImageSurface` gives a
//! CPU-addressable copy in roughly 0.4 ms at 1280x800 — an order of magnitude
//! cheaper than `webkit_web_view_get_snapshot`, which re-renders the document.
//!
//! Cairo's ARGB32 is premultiplied and, on little-endian hosts, laid out as
//! `[B, G, R, A]`, which is exactly `Bgra8Unorm` with the premultiplied-alpha
//! convention viso's overlay pass expects.

use gtk::cairo::{Context, Format, ImageSurface, Operator};
use gtk::prelude::*;
use viso::wgpu;
use winit::dpi::PhysicalSize;

/// Matches cairo ARGB32 byte-for-byte on little-endian hosts. `Unorm` rather
/// than `UnormSrgb`: the bytes are sRGB-encoded, and the overlay shader — not
/// the sampler — decides whether to decode them.
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

pub(super) struct Capture {
    size: PhysicalSize<u32>,
    /// Reused blit target and upload source. Allocated with the texture.
    image: Option<ImageSurface>,
    target: Option<Target>,
}

struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl Capture {
    pub(super) fn new(size: PhysicalSize<u32>) -> Self {
        Self {
            size: clamp(size),
            image: None,
            target: None,
        }
    }

    pub(super) const fn size(&self) -> PhysicalSize<u32> {
        self.size
    }

    /// Request a new size. The texture is rebuilt on the next
    /// [`Self::ensure_texture`].
    pub(super) fn resize(&mut self, size: PhysicalSize<u32>) {
        self.size = clamp(size);
        self.image = None;
        self.target = None;
    }

    /// Allocate the texture if it is missing. Returns its view **only when it
    /// was just created**, so callers reinstall it on viso exactly once per
    /// size rather than rebuilding a bind group every frame.
    pub(super) fn ensure_texture(&mut self, device: &wgpu::Device) -> Option<&wgpu::TextureView> {
        if self.target.is_some() {
            return None;
        }
        let (width, height) = (self.size.width, self.size.height);
        let image = ImageSurface::create(Format::ARgb32, width.cast_signed(), height.cast_signed())
            .inspect_err(|e| log::error!("Could not allocate the webview blit surface: {e}"))
            .ok()?;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Webview Overlay"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TEXTURE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.image = Some(image);
        Some(&self.target.insert(Target { texture, view }).view)
    }

    /// Copy the window's latest paint into the texture. A no-op before the
    /// first paint, when the texture is still the zeroed (fully transparent)
    /// one wgpu handed us.
    pub(super) fn upload(&mut self, window: &gtk::OffscreenWindow, queue: &wgpu::Queue) {
        let (Some(image), Some(target)) = (self.image.as_mut(), self.target.as_ref()) else {
            return;
        };
        let Some(painted) = window.surface() else {
            return;
        };
        painted.flush();

        // `Operator::Source` overwrites rather than blends, so stale pixels
        // never bleed through. The pattern does not extend past the painted
        // surface, so a webview that has not caught up to a resize leaves the
        // remainder transparent instead of smearing its edge.
        match Context::new(&*image) {
            Ok(cr) => {
                cr.set_operator(Operator::Source);
                if let Err(e) = cr.set_source_surface(&painted, 0.0, 0.0).and_then(|()| cr.paint()) {
                    log::warn!("Could not blit the webview surface: {e}");
                    return;
                }
            }
            Err(e) => {
                log::warn!("Could not open a cairo context on the blit surface: {e}");
                return;
            }
        }
        image.flush();

        let stride = image.stride().cast_unsigned();
        let height = image.height().cast_unsigned();
        let Ok(pixels) = image.data() else {
            log::warn!("Could not borrow the webview blit surface; a cairo context is still alive");
            return;
        };
        queue.write_texture(
            target.texture.as_image_copy(),
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(height),
            },
            target.texture.size(),
        );
    }
}

/// wgpu rejects zero-sized textures, and GTK rejects zero-sized windows.
fn clamp(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.max(1), size.height.max(1))
}
