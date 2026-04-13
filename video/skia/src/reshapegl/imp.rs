use gst::{glib, subclass::prelude::*};
use gst_base::subclass::prelude::*;
use gst_gl::{
    prelude::*,
    subclass::{prelude::*, GLFilterMode},
    *,
};
use tracing::instrument;

use std::sync::{LazyLock, Mutex};

use crate::reshape_common::{ReshapeCommon, Settings, State};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "skiareshapegl",
        gst::DebugColorFlags::empty(),
        Some("Reshape video with skia using GL"),
    )
});

// TODO: Implement transform_ip to allow in-place transformation when ONLY the "draw"
// signal is used (without geometric transformations like crop/padding/border-radius).
// This would avoid unnecessary buffer copies when skiareshape is used solely for
// custom drawing on top of the video without any reshaping.

#[derive(Debug, Clone)]
struct SkContext(skia::gpu::DirectContext);

// SAFETY: All access to the GL interface happens from the GstGLContext thread
unsafe impl Send for SkContext {}
unsafe impl Sync for SkContext {}

#[derive(Default, Debug)]
pub struct SkiaReshapeGL {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    context: Mutex<Option<SkContext>>,
    gstglcontext: Mutex<Option<gst_gl::GLContext>>,
}

impl ReshapeCommon for SkiaReshapeGL {
    fn settings(&self) -> std::sync::MutexGuard<'_, Settings> {
        self.settings.lock().unwrap()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

impl SkiaReshapeGL {
    fn render_with_skia(
        &self,
        buffer: &crate::BufferRef,
        video_info: &gst_video::VideoInfo,
        skia_context: &mut skia::gpu::DirectContext,
        input_tex_id: u32,
        output_tex_id: u32,
        in_info: &gst_video::VideoInfo,
        out_info: &gst_video::VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        gst::debug!(
            CAT,
            imp = self,
            "Starting Skia rendering - Frame processing"
        );

        gst::log!(
            CAT,
            "Creating input backend texture from GL texture ID {} ({}x{})",
            input_tex_id,
            in_info.width(),
            in_info.height()
        );

        let input_backend_texture = crate::gl::create_backend_texture_from_gl(
            input_tex_id,
            in_info.width() as i32,
            in_info.height() as i32,
            in_info.format(),
        )?;

        gst::log!(
            CAT,
            "Creating output backend texture from GL texture ID {} ({}x{})",
            output_tex_id,
            out_info.width(),
            out_info.height()
        );

        let output_backend_texture = crate::gl::create_backend_texture_from_gl(
            output_tex_id,
            out_info.width() as i32,
            out_info.height() as i32,
            out_info.format(),
        )?;

        let mut out_surface = crate::gl::create_surface_from_backend_texture(
            skia_context,
            &output_backend_texture,
            out_info.format(),
        )?;

        // Create the Skia image with standard properties but comprehensive error checking
        let image = skia::Image::from_texture(
            skia_context,
            &input_backend_texture,
            skia::gpu::SurfaceOrigin::TopLeft,
            self.video_format_to_skia_color_type(in_info.format())
                .ok_or_else(|| gst::loggable_error!(CAT, "Unsupported input format"))?,
            skia::AlphaType::Unpremul,
            None, // FIXME, What does that colorspace represent exactly here?
        )
        .ok_or_else(|| gst::loggable_error!(CAT, "Failed to create image from backend texture"))?;

        if skia_context.abandoned() {
            return Err(gst::loggable_error!(CAT, "Skia context has been abandoned"));
        }

        if !image.is_valid(Some(skia_context.as_recorder())) {
            return Err(gst::loggable_error!(CAT, "Input image is invalid"));
        }

        // Draw everything normally so that canvas.surface().image_snapshot()
        // (used by the draw-signal snapshot provider) captures the current frame.
        {
            let canvas = out_surface.canvas();

            self.reshape(buffer, video_info, canvas, &image, Some(skia_context))
                .map_err(|e| gst::loggable_error!(CAT, "Failed to reshape: {}", e))?;
        }

        // Flush to commit all GPU drawing to the surface before snapshotting.
        skia_context.flush_and_submit();

        // Skia GPU surfaces produce premultiplied alpha, but GStreamer
        // expects straight (unpremultiplied) alpha. Apply conversion as a
        // post-processing pass now that all drawing is done.
        let rendered = out_surface.image_snapshot();

        let unpremul_effect = skia::RuntimeEffect::make_for_shader(
            "uniform shader child; \
             half4 main(float2 coord) { \
               half4 c = child.eval(coord); \
               if (c.a > 0.0) { return half4(c.rgb / c.a, c.a); } \
               return c; \
             }",
            None,
        )
        .map_err(|e| gst::loggable_error!(CAT, "SkSL compile error: {}", e))?;

        let builder = skia::runtime_effect::RuntimeShaderBuilder::new(unpremul_effect);
        let unpremul_filter = skia::image_filters::runtime_shader(&builder, "child", None)
            .ok_or_else(|| {
                gst::loggable_error!(CAT, "Failed to create unpremultiply image filter")
            })?;

        let canvas = out_surface.canvas();
        canvas.clear(skia::Color::TRANSPARENT);

        let mut unpremul_paint = skia::Paint::default();
        unpremul_paint.set_blend_mode(skia::BlendMode::Src);
        unpremul_paint.set_image_filter(unpremul_filter);
        canvas.draw_image(&rendered, (0, 0), Some(&unpremul_paint));

        skia_context.flush_and_submit();

        Ok(())
    }

    fn video_format_to_skia_color_type(
        &self,
        format: gst_video::VideoFormat,
    ) -> Option<skia::ColorType> {
        let color_type = match format {
            gst_video::VideoFormat::Rgba => Some(skia::ColorType::RGBA8888),
            gst_video::VideoFormat::Bgra => Some(skia::ColorType::BGRA8888),
            gst_video::VideoFormat::Rgb => Some(skia::ColorType::RGB888x),
            _ => None,
        };

        gst::debug!(
            CAT,
            "GStreamer format {:?} -> Skia ColorType {:?}",
            format,
            color_type
        );
        color_type
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SkiaReshapeGL {
    const NAME: &'static str = "GstSkiaReshapeGL";
    type Type = super::SkiaReshapeGL;
    type ParentType = gst_gl::GLFilter;
}

impl ObjectImpl for SkiaReshapeGL {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> =
            LazyLock::new(crate::reshape_common::reshape_properties);
        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        self.reshape_property(_id, pspec)
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        self.reshape_set_property(_id, value, pspec)
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> =
            LazyLock::new(|| crate::reshape_common::reshape_signals());
        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for SkiaReshapeGL {}

impl ElementImpl for SkiaReshapeGL {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "SkiaReshapeGL",
                "Filter/Effect/Converter/Video",
                "Applies geometric transformations (rounded corners, scaling, cropping) to video frames using Skia with GL acceleration",
                "Thibault Saunier <tsaunier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}
impl BaseTransformImpl for SkiaReshapeGL {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.reshape_start()
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        self.reshape_set_caps(incaps, outcaps)
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        self.reshape_transform_caps(direction, caps, filter)
    }

    fn submit_input_buffer(
        &self,
        is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.reshape_submit_input_buffer(is_discont, inbuf)
    }

    fn copy_metadata(
        &self,
        inbuf: &gst::BufferRef,
        outbuf: &mut gst::BufferRef,
    ) -> Result<(), gst::LoggableError> {
        self.reshape_copy_metadata(inbuf, outbuf)
    }

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        self.reshape_before_transform(inbuf)
    }
}
impl GLBaseFilterImpl for SkiaReshapeGL {
    fn gl_set_caps(
        &self,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        let in_info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse input caps"))?;
        let out_info = gst_video::VideoInfo::from_caps(outcaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse output caps"))?;

        gst::debug!(
            CAT,
            imp = self,
            "Setting caps: {}x{} -> {}x{}",
            in_info.width(),
            in_info.height(),
            out_info.width(),
            out_info.height()
        );

        {
            let mut state = self.state.lock().unwrap();
            state.in_info = Some(in_info);
            state.out_info = Some(out_info);
        }

        self.parent_gl_set_caps(incaps, outcaps)
    }

    fn gl_start(&self) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "Starting GL operations");

        let filter = self.obj();

        let context = match GLBaseFilterExt::context(&*filter) {
            Some(ctx) => ctx,
            None => {
                gst::error!(CAT, imp = self, "No GL context available");
                return Err(gst::loggable_error!(CAT, "No GL context available"));
            }
        };

        gl::load_with(|name| context.proc_address(name) as *const _);

        #[cfg(target_os = "emscripten")]
        {
            // On Emscripten, creating a second GL context requires a new
            // OffscreenCanvas which is not possible in PROXY_TO_PTHREAD
            // mode. Use the existing GStreamer GL context directly.
            //
            // Also use Skia's WebGL-specific GL interface rather than the
            // generic `new_load_with` loader: the latter probes for desktop
            // GL functions like glViewportArrayv which don't exist in
            // WebGL and fail proc_address lookups.
            let gl_interface = skia::gpu::interfaces::make_web_gl().ok_or_else(|| {
                gst::loggable_error!(CAT, "Failed to create Skia WebGL interface")
            })?;

            let direct_context =
                skia::gpu::direct_contexts::make_gl(gl_interface, None).ok_or_else(|| {
                    gst::loggable_error!(CAT, "Failed to create Skia GL direct context")
                })?;

            *self.context.lock().unwrap() = Some(SkContext(direct_context));
            *self.gstglcontext.lock().unwrap() = Some(context.clone());
            gst::debug!(CAT, imp = self, "Created Skia GL context (emscripten, shared)");
        }

        #[cfg(not(target_os = "emscripten"))]
        {
            let display = context.display();
            let our_context = gst_gl::GLContext::new(&display);

            our_context
                .create(Some(&context))
                .map_err(|e| gst::loggable_error!(CAT, "Couldn't start our context {e:?}."))?;
            gst::log!(CAT, imp = self, "Created our own {:?}", context);

            let gl_result: std::sync::Arc<std::sync::Mutex<Result<(), gst::LoggableError>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Ok(())));
            our_context.thread_add(glib::clone!(
                #[to_owned(rename_to = this)]
                self,
                #[strong]
                gl_result,
                move |context| {
                    if let Some(gl_interface) = skia::gpu::gl::Interface::new_load_with(|name| {
                        context.proc_address(name) as *const std::ffi::c_void
                    }) {
                        if let Some(direct_context) =
                            skia::gpu::direct_contexts::make_gl(gl_interface, None)
                        {
                            *this.context.lock().unwrap() = Some(SkContext(direct_context));
                            *this.gstglcontext.lock().unwrap() = Some(context.clone());
                            gst::debug!(CAT, imp = self, "Created Skia GL context");
                        } else {
                            *gl_result.lock().unwrap() = Err(gst::loggable_error!(
                                CAT,
                                "Failed to create Skia GL direct context"
                            ));
                            return;
                        }
                    } else {
                        *gl_result.lock().unwrap() = Err(gst::loggable_error!(
                            CAT,
                            "Failed to create Skia GL direct context"
                        ));
                        return;
                    }

                    gst::debug!(
                        CAT,
                        imp = self,
                        "Successfully created Skia GL context pair"
                    );
                }
            ));

            gl_result.lock().unwrap().clone()?;
        }

        self.parent_gl_start()
    }

    fn gl_stop(&self) {
        gst::debug!(CAT, imp = self, "Stopping GL operations");

        self.parent_gl_stop();

        *self.context.lock().unwrap() = None;
    }
}

impl GLFilterImpl for SkiaReshapeGL {
    const MODE: GLFilterMode = GLFilterMode::Buffer;

    fn transform_internal_caps(
        &self,
        _direction: gst::PadDirection,
        caps: &gst::Caps,
        _filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        Some(caps.clone())
    }

    #[instrument(skip(self, input, output))]
    fn filter(&self, input: &gst::Buffer, output: &gst::Buffer) -> Result<(), gst::LoggableError> {
        gst::trace!(CAT, imp = self, "Processing GL buffer");

        let state = self.state.lock().unwrap();
        let in_info = state
            .in_info
            .as_ref()
            .ok_or_else(|| gst::loggable_error!(CAT, "No input info available"))?
            .clone();
        let out_info = state
            .out_info
            .as_ref()
            .ok_or_else(|| gst::loggable_error!(CAT, "No output info available"))?
            .clone();
        drop(state);

        // Get input GLMemory directly from buffer
        let in_mem = input
            .peek_memory(0)
            .downcast_memory_ref::<gst_gl::GLMemory>()
            .ok_or_else(|| gst::loggable_error!(CAT, "Input memory is not GLMemory"))?;

        // Get output GLMemory directly from buffer
        let out_mem = output
            .peek_memory(0)
            .downcast_memory_ref::<gst_gl::GLMemory>()
            .ok_or_else(|| gst::loggable_error!(CAT, "Output memory is not GLMemory"))?;

        // Get texture IDs
        let input_tex_id = in_mem.texture_id();
        let output_tex_id = out_mem.texture_id();

        #[cfg(target_os = "emscripten")]
        {
            // On Emscripten we share the main GL context — run Skia directly.
            let skia_context_mutex = self.context.lock().unwrap();
            let mut skia_context = skia_context_mutex
                .as_ref()
                .expect("No Skia context while filtering")
                .clone();
            drop(skia_context_mutex);

            // GstGL has issued GL commands since the last Skia call, so
            // invalidate Skia's cached GL state before using the context.
            skia_context.0.reset(None);

            unsafe { gl::Finish(); }

            let render_result = self.render_with_skia(
                &crate::BufferRef::new(output),
                &out_info,
                &mut skia_context.0,
                input_tex_id,
                output_tex_id,
                &in_info,
                &out_info,
            );

            match render_result {
                Ok(_) => {
                    // Skia has touched GL state (viewport, fbo, program,
                    // scissor, blend etc). Flush to ensure all draws hit
                    // the output texture before returning control to
                    // GstGL, which will do its own state setup on next
                    // pass.
                    unsafe { gl::Finish(); }
                    gst::debug!(CAT, "Skia rendering completed successfully");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }

        #[cfg(not(target_os = "emscripten"))]
        {
            let gl_result: std::sync::Arc<std::sync::Mutex<Result<(), gst::LoggableError>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Ok(())));
            self.gstglcontext
                .lock()
                .unwrap()
                .as_ref()
                .expect("No dedicated GL context available")
                .thread_add(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    gl_result,
                    move |context| {
                        if let Err(err) = context.activate(true) {
                            *gl_result.lock().unwrap() =
                                Err(gst::loggable_error!(CAT, "{err:?}",));
                            return;
                        }
                        gst::debug!(
                            CAT,
                            "EXECUTING SKIA RENDERING IN DEDICATED CONTEXT: {:?}",
                            context
                        );

                        let skia_context_mutex = this.context.lock().unwrap();
                        let mut skia_context = skia_context_mutex
                            .as_ref()
                            .expect("No Skia context while filtering")
                            .clone();
                        drop(skia_context_mutex);

                        unsafe {
                            gl::Finish();
                        }

                        let render_result = this.render_with_skia(
                            &crate::BufferRef::new(output),
                            &out_info,
                            &mut skia_context.0,
                            input_tex_id,
                            output_tex_id,
                            &in_info,
                            &out_info,
                        );

                        match render_result {
                            Ok(_) => {
                                unsafe {
                                    gl::Finish();
                                }
                                gst::debug!(
                                    CAT,
                                    "Skia rendering completed successfully in dedicated context"
                                );
                            }
                            Err(e) => {
                                *gl_result.lock().unwrap() = Err(e);
                            }
                        }
                        if let Err(err) = context.activate(false) {
                            *gl_result.lock().unwrap() =
                                Err(gst::loggable_error!(CAT, "{err:?}",));
                            return;
                        }
                    }
                ));

            let result = gl_result.lock().unwrap().clone();
            result
        }
    }
}
