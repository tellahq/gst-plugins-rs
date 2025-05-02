use ges::prelude::*;
use gst::{
    glib::{self, Properties},
    subclass::prelude::*,
};
use gst_base::{
    prelude::*,
    subclass::base_transform::{InputBuffer, PrepareOutputBufferSuccess},
};
use gst_video::{prelude::*, subclass::prelude::*, VideoFormat};

use std::sync::{Mutex, LazyLock};
use tracing::*;

const DEFAULT_BORDER_RADIUS: f64 = 0.0;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "skiareshape",
        gst::DebugColorFlags::empty(),
        Some("Reshape video with skia"),
    )
});

#[derive(Debug, Clone, Copy)]
struct Settings {
    border_radius_px: f64,
    corner_smoothing_pct: f64,
    padding_px: i32,
    crop_left: i32,
    crop_right: i32,
    crop_top: i32,
    crop_bottom: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            border_radius_px: DEFAULT_BORDER_RADIUS,
            corner_smoothing_pct: 0.,
            padding_px: 0,
            crop_left: 0,
            crop_right: 0,
            crop_top: 0,
            crop_bottom: 0,
        }
    }
}

#[derive(Default, Debug)]
struct State {
    in_info: Option<gst_video::VideoInfo>,
    out_info: Option<gst_video::VideoInfo>,
    compositor_position: Option<skia::Rect>,

    compositor_size: Option<skia::Size>,
}

#[derive(Default, Properties, Debug)]
#[properties(wrapper_type = super::SkiaReshape)]
pub struct SkiaReshape {
    #[property(
        name = "border-radius-px",
        type = f64,
        get,
        set,
        nick = "Border radius in pixels",
        blurb = "Draw rounded corners with given border radius",
        default_value = DEFAULT_BORDER_RADIUS,
        controllable,
        mutable_playing,
        member = border_radius_px
    )]
    #[property(
        name = "corner-smoothing-pct",
        type = f64,
        get,
        set,
        nick = "Corner smoothing in percentage",
        blurb = "Draw rounded corners with corner smoothing",
        default_value = 0.0,
        controllable,
        mutable_playing,
        member = corner_smoothing_pct
    )]
    #[property(
        name = "padding-px",
        type = i32,
        get,
        set,
        nick = "Padding in pixels",
        blurb = "Extending the box for drawing borders/shadows",
        default_value = 0,
        controllable,
        mutable_playing,
        member = padding_px
    )]
    #[property(
        name = "left",
        type = i32,
        get,
        set,
        nick = "Crop left in pixels",
        blurb = "Crop left in pixels",
        default_value = 0,
        controllable,
        mutable_playing,
        member = crop_left
    )]
    #[property(
        name = "right",
        type = i32,
        get,
        set,
        nick = "Crop right in pixels",
        blurb = "Crop right in pixels",
        default_value = 0,
        controllable,
        mutable_playing,
        member = crop_right
    )]
    #[property(
        name = "top",
        type = i32,
        get,
        set,
        nick = "Crop top in pixels",
        blurb = "Crop top in pixels",
        default_value = 0,
        controllable,
        mutable_playing,
        member = crop_top
    )]
    #[property(
        name = "bottom",
        type = i32,
        get,
        set,
        nick = "Crop bottom in pixels",
        blurb = "Crop bottom in pixels",
        default_value = 0,
        controllable,
        mutable_playing,
        member = crop_bottom
    )]
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl SkiaReshape {
    fn compute_output_size(&self) -> (Option<i32>, Option<i32>) {
        let rect = if let Some(rect) = self.state.lock().unwrap().compositor_position {
            rect
        } else {
            return (None, None);
        };

        let (mut width, mut height) = (None, None);
        let settings = self.settings.lock().unwrap();
        if rect.width() >= 0. {
            let mut tmpwidth = rect.width().ceil() as i32;
            tmpwidth += settings.padding_px * 2;
            tmpwidth -= settings.crop_left - settings.crop_right;

            width = Some(tmpwidth);
        }

        if rect.height() >= 0. {
            let mut tmpheight = rect.height().ceil() as i32;

            tmpheight += settings.padding_px * 2;
            tmpheight -= settings.crop_top - settings.crop_bottom;

            height = Some(tmpheight);
        }

        gst::error!( CAT, imp = self, "Width: {width:?} - Height: {height:?}");

        (width, height)
    }

    fn compute_src_image_and_dest_rects(
        &self,
    ) -> Result<(skia::Rect, skia::Rect), gst::FlowError> {
        let state = self.state.lock().unwrap();
        let in_info = state
            .in_info
            .as_ref()
            .ok_or_else(|| {
                gst::element_imp_error!(self, gst::CoreError::Negotiation, ["Have no state yet"]);
                gst::FlowError::NotNegotiated
            })?
            .clone();
        let out_info = state
            .out_info
            .as_ref()
            .ok_or_else(|| {
                gst::element_imp_error!(self, gst::CoreError::Negotiation, ["Have no state yet"]);
                gst::FlowError::NotNegotiated
            })?
            .clone();
        let compositor_size = state.compositor_size;
        let position_in_compositor_rect = state.compositor_position;

        // We have extended the video frame to get rid of floats in transform_caps,
        // we will draw the video frame anti-aliassed on the x_offset and y_offset.
        // Doing it this way means the compositor doesn't need to do any
        // scaling/anti-aliassing, we already do it here instead.
        let (x_offset, y_offset, width_in_compositor, height_in_compositor) =
            if let Some(ref position_in_compositor) = position_in_compositor_rect {
                let x = position_in_compositor.left();
                let y = position_in_compositor.top();
                let width = position_in_compositor.width();
                let height = position_in_compositor.height();
                let nop =  (x - x.floor(), y - y.floor(), width, height);

                gst::error!(
                    CAT,
                    imp = self,
                    "Nop: {nop:?}"
                );
                // (0.0, 0.0, out_info.width() as f32, out_info.height() as f32)
                nop
            } else {
                (0.0, 0.0, out_info.width() as f32, out_info.height() as f32)
            };
        gst::error!(
            CAT,
            imp = self,
            "Yes: {:?}", (x_offset, y_offset, width_in_compositor, height_in_compositor)
        );
        drop(state);

        // Figure out the right rect for the video frame.
        let (padding_px, crop_left, crop_right, crop_top, crop_bottom) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.padding_px,
                settings.crop_left,
                settings.crop_right,
                settings.crop_top,
                settings.crop_bottom,
            )
        };

        let src_with_cropping_applied = skia::Rect::from_xywh(
            crop_left as f32,
            crop_top as f32,
            in_info.width() as f32 - (crop_left as f32 + crop_right as f32),
            in_info.height() as f32 - (crop_top as f32 + crop_bottom as f32),
        );

        let x_offset = x_offset + padding_px as f32;
        let y_offset = y_offset + padding_px as f32;
        let dst_rect = skia::Rect::from_xywh(
            x_offset,
            y_offset,
            width_in_compositor,
            height_in_compositor,
        );

        // log only once
        gst::error!(CAT, imp = self, "\nIN WIDTH: {:?} - in with - crop_left - crop_right {:?} -- \
            src_rect with crop applied --> {src_with_cropping_applied:#?} -- (width: {:?})\n{dst_rect:#?}",
            in_info.width(), in_info.width() as f32 - (crop_left as f32 + crop_right as f32),
            src_with_cropping_applied.width()
        );

        Ok((src_with_cropping_applied, dst_rect))
    }
}

fn skia_rect_from_meta(
    meta: Option<gst::MetaRef<ges::prelude::FrameCompositionMeta>>,
) -> Option<skia::Rect> {
    meta.map(|meta| {
        let x = meta.pos_x() as f32;
        let y = meta.pos_y() as f32;
        let w = meta.width() as f32;
        let h = meta.height() as f32;
        skia::Rect::from_xywh(x, y, w, h)
    })
}

#[glib::object_subclass]
impl ObjectSubclass for SkiaReshape {
    const NAME: &'static str = "GstSkiaReshape";
    type Type = super::SkiaReshape;
    type ParentType = gst_video::VideoFilter;
}

#[glib::derived_properties]
impl ObjectImpl for SkiaReshape {}

impl GstObjectImpl for SkiaReshape {}

impl ElementImpl for SkiaReshape {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "SkiaReshape",
                "Filter/Effect/Converter/Video",
                "Applies geometric transformations (rounded corners, scaling, cropping) to video frames using Skia",
                "Michiel Westerbeek <michiel@tella.tv>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .format(VideoFormat::Rgba)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new()
                .format(VideoFormat::Rgba)
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for SkiaReshape {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting");

        let mut parent = Some(self.obj().clone().upcast::<gst::Object>());
        while let Some(p) = parent {
            if let Some(track) = p.downcast_ref::<ges::VideoTrack>() {
                let mut state = self.state.lock().unwrap();
                state.compositor_size = None;
                if let Some(caps) = track.restriction_caps() {
                    for structure in caps.iter() {
                        let (mut width, mut height) = (None, None);
                        if let Ok(w) = structure.get::<i32>("width") {
                            width = Some(w);
                        }
                        if let Ok(h) = structure.get::<i32>("height") {
                            height = Some(h);
                        }

                        if let (Some(w), Some(h)) = (width, height) {
                            state.compositor_size = Some(skia::Size::new(w as f32, h as f32));
                        } else {
                            gst::error!(CAT, "Failed to get width/height from restriction caps");
                        }
                    }
                }

                gst::info!(
                    CAT,
                    imp = self,
                    "Compositor size: {:?}",
                    state.compositor_size
                );

                break;
            }

            parent = p.parent()
        }

        self.parent_start()
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let (in_info, out_info) = match (
            gst_video::VideoInfo::from_caps(incaps),
            gst_video::VideoInfo::from_caps(outcaps),
        ) {
            (Ok(in_info), Ok(out_info)) => (in_info, out_info),
            _ => return Err(gst::loggable_error!(CAT, "Failed to parse output caps")),
        };

        gst::debug!(
            CAT,
            imp = self,
            "Configured for caps {} to {}",
            incaps,
            outcaps
        );

        {
            let mut state = self.state.lock().unwrap();
            state.in_info = Some(in_info);
            state.out_info = Some(out_info);
        }

        self.parent_set_caps(incaps, outcaps)
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        match direction {
            gst::PadDirection::Src => {
                let mut caps = caps.copy();
                caps.make_mut().map_in_place(move |_features, structure| {
                    structure.remove_fields(["width", "height"]);

                    std::ops::ControlFlow::Continue(())
                });
                self.parent_transform_caps(direction, &caps, filter)
            }
            gst::PadDirection::Sink => {
                let (width, height) = self.compute_output_size();

                if (width, height) == (None, None) {
                    let mut caps = caps.copy();
                    let settings = self.settings.lock().unwrap();
                    caps.get_mut()
                        .unwrap()
                        .map_in_place(move |_features, structure| {
                            if let Ok(width) = structure.get::<i32>("width") {
                                structure.set("width", width - settings.crop_left - settings.crop_right);
                            }

                            if let Ok(height) = structure.get::<i32>("height") {
                                structure.set("height", height - settings.crop_top - settings.crop_top);
                            }

                            std::ops::ControlFlow::Continue(())
                        });
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Not in GES.... transformed caps: {caps:#?}"
                    );
                    return self.parent_transform_caps(direction, &caps, filter);
                }

                let mut caps = caps.copy();
                caps.get_mut()
                    .unwrap()
                    .map_in_place(move |_features, structure| {
                        if let Some(ref width) = width {
                            structure.set("width", width);
                        }

                        if let Some(ref height) = height {
                            structure.set("height", height);
                        }

                        std::ops::ControlFlow::Continue(())
                    });

                let res = self.parent_transform_caps(direction, &caps, filter);

                res
            }
            _ => unreachable!(),
        }
    }

    fn submit_input_buffer(
        &self,
        is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let compositor_position =
            skia_rect_from_meta(inbuf.meta::<ges::prelude::FrameCompositionMeta>().clone());

        {
            let mut state = self.state.lock().unwrap();
            if state.compositor_position != compositor_position {
                state.compositor_position = compositor_position;
                drop(state);

                self.obj().reconfigure_src();
            }
        }

        self.parent_submit_input_buffer(is_discont, inbuf)
    }

    fn prepare_output_buffer(
        &self,
        inbuf: InputBuffer,
    ) -> Result<PrepareOutputBufferSuccess, gst::FlowError> {
        if self.obj().is_passthrough() {
            return Ok(PrepareOutputBufferSuccess::InputBuffer);
        }

        let state = self.state.lock().unwrap();
        let out_info = state.out_info.as_ref().unwrap().clone();

        let (rect, in_size) = match inbuf {
            InputBuffer::Writable(ref buf) => (
                skia_rect_from_meta(buf.meta::<ges::prelude::FrameCompositionMeta>()),
                buf.size(),
            ),
            InputBuffer::Readable(ref buf) => (
                skia_rect_from_meta(buf.meta::<ges::prelude::FrameCompositionMeta>()),
                0,
            ),
        };

        assert!(rect == state.compositor_position);
        drop(state);
        if out_info.size() == in_size {
            if let InputBuffer::Writable(buf) = inbuf {
                let settings = self.settings.lock().unwrap();
                // add_custom_meta(buf, settings);
                return Ok(PrepareOutputBufferSuccess::InputBuffer);
            }
        }

        let mut outbuf = if let (Some(allocator), params) = self.obj().allocator() {
            let mem = allocator
                .alloc(out_info.size(), Some(&params))
                .map_err(|_| {
                    gst::error!(CAT, "Failed to allocate memory of size {}", out_info.size());
                    gst::FlowError::Error
                })?;

            let mut buf = gst::Buffer::new();
            let mut_buf = buf.make_mut();
            mut_buf.append_memory(mem);

            unsafe {
                gst::ffi::gst_mini_object_lock(
                    buf.as_mut_ptr() as *mut _,
                    gst::ffi::GST_LOCK_FLAG_EXCLUSIVE,
                );
            }

            buf
        } else {
            gst::Buffer::with_size(out_info.size()).map_err(|_| {
                gst::error!(CAT, "Failed to allocate buffer of size {}", out_info.size());
                gst::FlowError::Error
            })?
        };

        let mut_outbuf = outbuf.make_mut();
        let inbuf = match inbuf {
            InputBuffer::Writable(ref buf) => buf,
            InputBuffer::Readable(buf) => buf,
        };

        inbuf
            .copy_into(mut_outbuf, gst::BufferCopyFlags::all(), 0..0)
            .map_err(|_| {
                gst::error!(CAT, "Failed to copy buffer of size {}", out_info.size());
                gst::FlowError::Error
            })?;
        let settings = self.settings.lock().unwrap();
        let padding = settings.padding_px as f64;
        // add_custom_meta(mut_outbuf, settings);
        // add_original_frame_meta(
        //     mut_outbuf,
        //     &inbuf.meta::<ges::prelude::FrameCompositionMeta>().unwrap(),
        // );

        self.copy_metadata(inbuf, mut_outbuf).map_err(|_| {
            gst::error!(
                CAT,
                "Failed to copy metadata from input buffer to output buffer"
            );
            gst::FlowError::Error
        })?;

        // Update FrameCompositionMeta to also have the floored/ceiled values.
        if let Some(rect) = self.state.lock().unwrap().compositor_position {
            if let Some(mut meta) = mut_outbuf.meta_mut::<ges::prelude::FrameCompositionMeta>() {
                // We need to floor the x and y values, and ceil the width and height
                let x = rect.x().floor() as f64;
                let y = rect.y().floor() as f64;
                let width = rect.width().ceil() as f64;
                let height = rect.height().ceil() as f64;

                // Add padding
                let width = width + (padding * 2.0);
                let height = height + (padding * 2.0);

                meta.set_pos_x(x);
                meta.set_pos_y(y);
                meta.set_width(width);
                meta.set_height(height);
            }
        }

        Ok(PrepareOutputBufferSuccess::Buffer(mut_outbuf.to_owned()))
    }

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let timestamp = inbuf.pts().expect("Buffer without PTS");
        let segment = self.obj().segment().downcast::<gst::ClockTime>().ok();
        let stream_time = segment.as_ref().and_then(|s| s.to_stream_time(timestamp));

        match stream_time {
            Some(stream_time) => match self.obj().sync_values(stream_time) {
                Ok(_) => (),
                Err(_) => {
                    // error!("Failed to sync values: {:?}", err);
                    // Ignoring this error for now. It seems harmless.
                }
            },
            None => {
                warn!("No stream time available");
            }
        }
    }
}

impl VideoFilterImpl for SkiaReshape {
    #[instrument(skip(self, frame))]
    fn transform_frame(
        &self,
        frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
        outframe: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let Some(meta) = frame.buffer().meta::<ges::prelude::FrameCompositionMeta>() {
            // Optimisation, when alpha is 0, we don't need to draw anything.
            if meta.alpha() == 0. {
                gst::debug!(CAT, imp = self, "Alpha is 0, skipping drawing");
                return Ok(gst::FlowSuccess::Ok);
            }
        }

        let img_info = skia::ImageInfo::new(
            skia::ISize {
                width: frame.width() as i32,
                height: frame.height() as i32,
            },
            skia::ColorType::RGBA8888,
            skia::AlphaType::Unpremul,
            None,
        );

        // SAFETY: We own the data throughout all the drawing process as we own a readable
        // reference on the underlying GStreamer buffer
        let image = unsafe {
            skia::image::images::raster_from_data(
                &img_info,
                skia::Data::new_bytes(frame.plane_data(0).unwrap()),
                frame.info().stride()[0] as usize,
            )
        }
        .expect("Wrong image parameters to raster from data.");

        let out_info = self
            .state
            .lock()
            .unwrap()
            .out_info
            .as_ref()
            .ok_or_else(|| {
                gst::element_imp_error!(self, gst::CoreError::Negotiation, ["Have no state yet"]);
                gst::FlowError::NotNegotiated
            })?
            .clone();

        let out_img_info = skia::ImageInfo::new(
            skia::ISize {
                width: out_info.width() as i32,
                height: out_info.height() as i32,
            },
            skia::ColorType::RGBA8888,
            skia::AlphaType::Unpremul,
            None,
        );

        let row_bytes = outframe.info().stride()[0] as usize;

        if row_bytes < out_img_info.min_row_bytes() {
            gst::error!(
                CAT,
                imp = self,
                "Row bytes too small: {} < {}",
                row_bytes,
                out_img_info.min_row_bytes()
            );
            return Err(gst::FlowError::Error);
        }

        let plane_data = match outframe.plane_data_mut(0) {
            Err(e) => {
                error!("Failed to get plane data: {:?}", e);
                return Err(gst::FlowError::Error);
            }
            Ok(data) => data,
        };

        if plane_data.len() < out_img_info.compute_byte_size(row_bytes) {
            gst::error!(
                CAT,
                imp = self,
                "Plane data too small: {} < {}",
                plane_data.len(),
                out_img_info.compute_byte_size(row_bytes),
            );
            return Err(gst::FlowError::Error);
        }

        let mut out_surface =
            skia::surface::surfaces::wrap_pixels(&out_img_info, plane_data, row_bytes, None)
                .ok_or(gst::FlowError::Error)?;

        let canvas = out_surface.canvas();
        let (crop_rect, dst_rect) = self.compute_src_image_and_dest_rects()?;

        // Clear the whole canvas, else we get artifacts from the previous frame
        canvas.clear(skia::Color::from_rgb(0, 255, 0));

        // Draw the video frame at the correct position, with anti-aliasing.
        let mut paint = skia::Paint::default();
        paint.set_anti_alias(true);
        paint.set_blend_mode(skia::BlendMode::Src);

        // let crop_rect = skia::Rect::from_xywh(0., 0., 540., 480.);

        // Create a crop image filter
        let crop_filter = skia::ImageFilter::crop(
            crop_rect,
            None,
            None,
        );
        // paint.set_image_filter(crop_filter);
        canvas.draw_image_rect_with_sampling_options(
            &image,
            Some((&crop_rect, skia::canvas::SrcRectConstraint::Strict)),
            dst_rect,
            skia::SamplingOptions::new(skia::FilterMode::Linear, skia::MipmapMode::Linear),
            &paint,
        );

        // Clip out the rounded corners
        let border_radius = self.settings.lock().unwrap().border_radius_px as f32;

        let rounded_dst_rect = skia::RRect::new_rect_xy(dst_rect, border_radius, border_radius);

        canvas.clip_rrect(rounded_dst_rect, skia::ClipOp::Difference, true);

        canvas.clear(skia::Color::TRANSPARENT);

        Ok(gst::FlowSuccess::Ok)
    }
}

// fn add_custom_meta(outbuf: &mut gst::BufferRef, settings: std::sync::MutexGuard<'_, Settings>) {
//     let mut meta = gst::meta::CustomMeta::add(outbuf, "RoundedCornersFrameMeta").unwrap();
//     let s = meta.mut_structure();
//     s.set("border-radius-px", settings.border_radius_px);
//     s.set("corner-smoothing-pct", settings.corner_smoothing_pct);
// }

// fn add_original_frame_meta(outbuf: &mut gst::BufferRef, meta: &ges::prelude::FrameCompositionMeta) {
//     let mut new_meta = gst::meta::CustomMeta::add(outbuf, "OriginalFrameCompositionMeta").unwrap();
//     let s = new_meta.mut_structure();
//     s.set("alpha", meta.alpha());
//     s.set("posx", meta.pos_x());
//     s.set("posy", meta.pos_y());
//     s.set("height", meta.height());
//     s.set("width", meta.width());
//     s.set("zorder", meta.zorder());
//     s.set("operator", meta.operator());
// }
