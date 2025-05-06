use ges::prelude::*;
use gst::{
    glib::{self, Properties},
    subclass::prelude::*,
};
use gst_base::{
    subclass::base_transform::{InputBuffer, PrepareOutputBufferSuccess},
};
use gst_video::{prelude::*, subclass::prelude::*, VideoFormat};

use std::sync::{LazyLock, Mutex};
use tracing::*;

const DEFAULT_BORDER_RADIUS: f64 = 0.0;
const OPTICROP: bool = true;

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

    // Wanted position of the image taking into account padding
    compositor_position: Option<skia::Rect>,

    // Output size of the **compositor itself**
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

#[derive(Debug, Copy, Clone)]
struct TranslationRects {
    // The rectangle that is used to crop the source image
    src_with_cropping_applied: skia::Rect,

    // The rectangle that is used to draw the image
    dst_rect: skia::Rect,

    // The rectangle that correspond to where the optimized cropped image correspond
    // after cropping is applied
    original_dst_rect: skia::Rect,

    // The size of the total outputed frame, taking into account padding
    out_size: skia::Size,
}

impl SkiaReshape {

    fn skia_rect_from_meta(
        &self,
        meta: Option<gst::MetaRef<ges::prelude::FrameCompositionMeta>>,
    ) -> Option<skia::Rect> {
        let padding = self.settings.lock().unwrap().padding_px as f32;
        meta.map(|meta| {
            let x = meta.pos_x() as f32;
            let y = meta.pos_y() as f32;
            let w = meta.width() as f32 + padding * 2.;
            let h = meta.height() as f32 + padding * 2.;
            skia::Rect::from_xywh(x, y, w, h)
        })
    }

    fn compute_output_size(&self, caps: &gst::Caps) -> (Option<i32>, Option<i32>) {
        if let Ok(rects) = self.compute_src_image_and_dest_rects(Some(caps)) {
            (Some(rects.out_size.width.ceil() as i32), Some(rects.out_size.height.ceil() as i32))
        } else if let Some(rect) = self.state.lock().unwrap().compositor_position {
            (Some(rect.width().ceil() as i32), Some(rect.height().ceil() as i32))
        } else {
         (None, None)
        }
    }

    fn compute_src_image_and_dest_rects(
        &self,
        incaps: Option<&gst::Caps>,
    ) -> Result<TranslationRects, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let in_info = if let Some(in_info) = state.in_info.as_ref() {
            in_info.clone()
        } else if let Some(incaps) = incaps {
            if !incaps.is_fixed() {
                return Err(gst::FlowError::NotNegotiated);
            }

            if let Ok(info) = gst_video::VideoInfo::from_caps(incaps) {
                info
            } else {
                return Err(gst::FlowError::NotNegotiated);
            }
        } else {
            gst::element_imp_error!(
                self,
                gst::CoreError::Negotiation,
                ["Have no state yet"]
            );
            return Err(gst::FlowError::NotNegotiated);
        };

        let compositor_size =
            state.compositor_size.unwrap_or(skia::Size::new(
                    in_info.width() as f32,
                    in_info.height() as f32,
                ));

        let out_frame_size = if let Some(out_info) = state.out_info.as_ref() {
            skia::Size::new(out_info.width() as f32, out_info.height() as f32)
        } else {
            compositor_size
        };

        let (padding_px, mut src_crop_left, crop_right, mut src_crop_top, crop_bottom) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.padding_px as f32,
                settings.crop_left as f32,
                settings.crop_right as f32,
                settings.crop_top as f32,
                settings.crop_bottom as f32,
            )
        };

        // We have extended the video frame to get rid of floats in transform_caps,
        // we will draw the video frame anti-aliassed on the x_offset and y_offset.
        // Doing it this way means the compositor doesn't need to do any
        // scaling/anti-aliassing, we already do it here instead.
        let (mut dst_left, mut dst_top, compositor_rect, img_compositor_rect) = if let Some(ref position_in_compositor) =
            state.compositor_position
        {
            let x = position_in_compositor.left();
            let y = position_in_compositor.top();

            (x - x.floor(), y - y.floor(),
                *position_in_compositor,
                skia::Rect::from_xywh(
                    position_in_compositor.x() + padding_px,
                    position_in_compositor.y() + padding_px,
                    position_in_compositor.width() - 2. * padding_px,
                    position_in_compositor.height() - 2. * padding_px,
                )
            )
        } else {
            (
                0.0,
                0.0,
                skia::Rect::from_xywh(0., 0., out_frame_size.width, out_frame_size.height),
                skia::Rect::from_xywh(0., 0., out_frame_size.width, out_frame_size.height),
            )
        };
        drop(state);

        let mut src_width = in_info.width() as f32 - crop_right;
        let mut src_height = in_info.height() as f32 - crop_bottom;

        let mut dst_width = img_compositor_rect.width();
        let mut dst_height = img_compositor_rect.height();

        let mut original_dst_left = dst_left;
        let mut original_dst_top = dst_top;
        let original_dst_width = dst_width;
        let original_dst_height = dst_height;

        let mut out_size = skia::Size::new(compositor_rect.width(), compositor_rect.height());

        if OPTICROP {
            let width_factor = (in_info.width() as f32) / dst_width;
            let height_factor = in_info.height() as f32 / dst_height;

            if img_compositor_rect.left() < 0. {
                let extra_crop_left_src = img_compositor_rect.left() * width_factor;

                src_crop_left -= extra_crop_left_src;
                dst_width += img_compositor_rect.left();

                original_dst_left += img_compositor_rect.left();
                out_size.width += compositor_rect.left();
            } else if compositor_rect.left() < 0. {
                dst_left += img_compositor_rect.left() * width_factor;
                original_dst_left += img_compositor_rect.left() * width_factor;
                out_size.width += compositor_rect.left();
            }

            if img_compositor_rect.right() > compositor_size.width {
                let cropped = compositor_size.width - img_compositor_rect.right();
                src_width += cropped * width_factor;
                dst_width += cropped;
            }

            if compositor_rect.right() > compositor_size.width {
                out_size.width += compositor_size.width - compositor_rect.right();
            }

            if img_compositor_rect.top() < 0. {
                let extra_crop_top = img_compositor_rect.top() * height_factor;

                src_crop_top -= extra_crop_top;
                dst_height += img_compositor_rect.top();
                original_dst_top += img_compositor_rect.top();
                out_size.height += compositor_rect.top();
            } else if compositor_rect.top() < 0. {
                dst_top += img_compositor_rect.top() * height_factor;
                original_dst_top += img_compositor_rect.top() * height_factor;
                out_size.height += compositor_rect.top();
            }

            if img_compositor_rect.bottom() > compositor_size.height {
                let cropped = compositor_size.height - img_compositor_rect.bottom();
                let extra_crop_bottom = cropped * height_factor;

                src_height += extra_crop_bottom;
                dst_height += cropped;
            }

            if compositor_rect.bottom() > compositor_size.height {
                out_size.height += compositor_size.height - compositor_rect.bottom();
            }
        } else {
            dst_left += padding_px;
            dst_top += padding_px;

            original_dst_left += padding_px;
            original_dst_top += padding_px;
        }

        let src_with_cropping_applied =
            skia::Rect::from_ltrb(src_crop_left, src_crop_top, src_width, src_height);

        let dst_rect = skia::Rect::from_xywh(dst_left, dst_top, dst_width, dst_height);

        let original_dst_rect = skia::Rect::from_xywh(
            original_dst_left,
            original_dst_top,
            original_dst_width,
            original_dst_height,
        );

        Ok(TranslationRects {
            src_with_cropping_applied,
            dst_rect,
            original_dst_rect,
            out_size
        })
    }
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

        gst::error!(
            CAT,
            imp = self,
            "Scaling from {}x{} to {}x{}",
            in_info.width(),
            in_info.height(),
            out_info.width(),
            out_info.height(),
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
                let (width, height) = self.compute_output_size(caps);

                if (width, height) == (None, None) {
                    let mut caps = caps.copy();
                    let settings = self.settings.lock().unwrap();
                    caps.get_mut()
                        .unwrap()
                        .map_in_place(move |_features, structure| {
                            if let Ok(width) = structure.get::<i32>("width") {
                                structure
                                    .set("width", width - settings.crop_left - settings.crop_right + 2 * settings.padding_px);
                            }

                            if let Ok(height) = structure.get::<i32>("height") {
                                structure
                                    .set("height", height - settings.crop_top - settings.crop_top + 2 * settings.padding_px);
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

                self.parent_transform_caps(direction, &caps, filter)
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
            self.skia_rect_from_meta(inbuf.meta::<ges::prelude::FrameCompositionMeta>().clone());

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

        let (rect, in_size) = match inbuf {
            InputBuffer::Writable(ref buf) => (
                self.skia_rect_from_meta(buf.meta::<ges::prelude::FrameCompositionMeta>()),
                buf.size(),
            ),
            InputBuffer::Readable(ref buf) => (
                self.skia_rect_from_meta(buf.meta::<ges::prelude::FrameCompositionMeta>()),
                0,
            ),
        };

        let state = self.state.lock().unwrap();
        let out_info = state.out_info.as_ref().unwrap().clone();
        assert!(rect == state.compositor_position);
        drop(state);
        if out_info.size() == in_size {
            if let InputBuffer::Writable(buf) = inbuf {
                let settings = self.settings.lock().unwrap();
                add_custom_meta(buf, settings);
                gst::fixme!(
                    CAT,
                    imp = self,
                    "UPDATE METADATAS!!",
                );
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
        add_custom_meta(mut_outbuf, settings);
        add_original_frame_meta(
            mut_outbuf,
            &inbuf.meta::<ges::prelude::FrameCompositionMeta>().unwrap(),
        );

        self.copy_metadata(inbuf, mut_outbuf).map_err(|_| {
            gst::error!(
                CAT,
                "Failed to copy metadata from input buffer to output buffer"
            );
            gst::FlowError::Error
        })?;

        // Update FrameCompositionMeta to also have the floored/ceiled values.
        let compositor_position = self.state.lock().unwrap().compositor_position;
        if let Some(compositor_rect) = compositor_position {
            let rects = self.compute_src_image_and_dest_rects(None);
            if let Some(mut meta) = mut_outbuf.meta_mut::<ges::prelude::FrameCompositionMeta>() {
                gst::debug!(CAT, imp = self, "Original meta {meta:#?}");
                // We need to floor the x and y values, and ceil the width and height
                let mut x = compositor_rect.x() as f64;
                let mut y = compositor_rect.y() as f64;
                let mut width = compositor_rect.width() as f64;
                let mut height = compositor_rect.height() as f64;

                if OPTICROP {
                    if let Ok(rects) = rects {
                        width = rects.out_size.width as f64;
                        height = rects.out_size.height as f64;
                        if compositor_rect.left() < 0. {
                            x = 0.;
                        }

                        if compositor_rect.top() < 0. {
                            y = 0.;
                        }
                    }
                }

                meta.set_pos_x(x.floor());
                meta.set_pos_y(y.floor());
                meta.set_width(width.ceil());
                meta.set_height(height.ceil());

                gst::debug!(CAT, imp = self, "New meta: {}x{} Meta: {meta:#?}",
                    out_info.width(),
                    out_info.height()
                );
            }


            if OPTICROP {
                if let Ok(mut meta) =  gst::meta::CustomMeta::from_mut_buffer(
                    mut_outbuf,
                    "OriginalFrameCompositionMeta",
                ) {
                    if let Ok(rects) = rects {
                        let s = meta.mut_structure();

                        s.set("posx", rects.original_dst_rect.left());
                        s.set("posy", rects.original_dst_rect.top());
                        s.set("height", rects.original_dst_rect.height());
                        s.set("width", rects.original_dst_rect.width());
                    }
                } else {
                    gst::info!(CAT, imp = self, "Failed to get OriginalFrameCompositionMeta");
                }
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
        let meta = if let Some(meta) = frame.buffer().meta::<ges::prelude::FrameCompositionMeta>() {
            // Optimisation, when alpha is 0, we don't need to draw anything.
            if meta.alpha() == 0. {
                gst::debug!(CAT, imp = self, "Alpha is 0, skipping drawing");
                return Ok(gst::FlowSuccess::Ok);
            }
            Some(meta.clone())
        } else {
            None
        };

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
        let rects =
            self.compute_src_image_and_dest_rects(None)?;

        gst::debug!(
            CAT,
            "\n- Drawing with rects:\n{:#?\
                imp = self,
            - Meta {meta:#?}"
        );

        // Clear the whole canvas, else we get artifacts from the previous frame
        canvas.clear(skia::Color::TRANSPARENT);

        // Draw the video frame at the correct position, with anti-aliasing.
        let mut paint = skia::Paint::default();
        paint.set_anti_alias(true);
        paint.set_blend_mode(skia::BlendMode::Src);

        let src_rect = Some((&rects.src_with_cropping_applied, skia::canvas::SrcRectConstraint::Strict));
        canvas.draw_image_rect_with_sampling_options(
            &image,
            src_rect,
            rects.dst_rect,
            skia::SamplingOptions::new(skia::FilterMode::Linear, skia::MipmapMode::Linear),
            &paint,
        );

        // Clip out the rounded corners
        let border_radius = self.settings.lock().unwrap().border_radius_px as f32;
        let rounded_dst_rect = skia::RRect::new_rect_xy(rects.original_dst_rect, border_radius, border_radius);

        canvas.clip_rrect(rounded_dst_rect, skia::ClipOp::Difference, true);

        canvas.clear(skia::Color::TRANSPARENT);

        Ok(gst::FlowSuccess::Ok)
    }
}

fn add_custom_meta(outbuf: &mut gst::BufferRef, settings: std::sync::MutexGuard<'_, Settings>) {
    let mut meta = if let Ok(meta) = gst::meta::CustomMeta::add(outbuf, "RoundedCornersFrameMeta") {
        meta
    } else {
        gst::info!(CAT, "RoundedCornersFrameMeta not registered");
        return;
    };
    let s = meta.mut_structure();
    s.set("border-radius-px", settings.border_radius_px);
    s.set("corner-smoothing-pct", settings.corner_smoothing_pct);
}

fn add_original_frame_meta(outbuf: &mut gst::BufferRef, meta: &ges::prelude::FrameCompositionMeta) {
    let mut new_meta = if let Ok(meta) = gst::meta::CustomMeta::add(outbuf, "OriginalFrameCompositionMeta") {
        meta
    } else {
        gst::info!(CAT, "OriginalFrameCompositionMeta not registered");
        return;
    };
    let s = new_meta.mut_structure();
    s.set("alpha", meta.alpha());
    s.set("posx", meta.pos_x());
    s.set("posy", meta.pos_y());
    s.set("height", meta.height());
    s.set("width", meta.width());
    s.set("zorder", meta.zorder());
    s.set("operator", meta.operator());
    gst::error!(CAT, "OriginalFrameCompositionMeta: {:#?}", s);
}
