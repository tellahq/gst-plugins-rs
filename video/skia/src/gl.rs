use std::sync::LazyLock;

const TEXTURE_2D: u32 = 0x0DE1;
const RGBA8: u32 = 0x8058;

pub fn create_backend_texture_from_gl(
    texture_id: u32,
    width: i32,
    height: i32,
    format: gst_video::VideoFormat,
) -> Result<skia::gpu::BackendTexture, gst::LoggableError> {
    let gl_format = match format {
        gst_video::VideoFormat::Rgba => RGBA8,
        _ => {
            return Err(gst::loggable_error!(
                CAT,
                "Unsupported video format for GL backend texture: {:?}",
                format
            ));
        }
    };

    gst::debug!(
        CAT,
        "Creating backend texture: id={}, {}x{}, format={:?}",
        texture_id,
        width,
        height,
        format
    );

    // Create backend texture from GL texture ID
    Ok(unsafe {
        skia::gpu::backend_textures::make_gl(
            (width, height),
            skia::gpu::Mipmapped::No,
            skia::gpu::gl::TextureInfo {
                target: TEXTURE_2D,
                id: texture_id,
                format: gl_format,
                ..Default::default()
            },
            "",
        )
    })
}

pub fn create_surface_from_backend_texture(
    context: &mut skia::gpu::DirectContext,
    backend_texture: &skia::gpu::BackendTexture,
    format: gst_video::VideoFormat,
) -> Result<skia::Surface, gst::LoggableError> {
    let color_type = match format {
        gst_video::VideoFormat::Rgba => skia::ColorType::RGBA8888,
        _ => {
            return Err(gst::loggable_error!(
                CAT,
                "Unsupported video format for surface creation: {:?}",
                format
            ));
        }
    };

    gst::debug!(
        CAT,
        "Creating surface from backend texture: format={:?}, color_type={:?}",
        format,
        color_type
    );

    skia::gpu::surfaces::wrap_backend_texture(
        context,
        backend_texture,
        skia::gpu::SurfaceOrigin::TopLeft,
        None, // Keep it simple
        color_type,
        None,
        None,
    )
    .ok_or_else(|| gst::loggable_error!(CAT, "Failed to create output backenbd texture"))
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "skia-gl",
        gst::DebugColorFlags::empty(),
        Some("Skia GL utilities"),
    )
});
