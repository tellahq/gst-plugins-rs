#[cfg(feature = "ges")]
use ges::prelude::*;
use gst::{glib, subclass::prelude::*};
use gst_video::{prelude::*, subclass::prelude::*};

use std::sync::LazyLock;

pub const DEFAULT_BORDER_RADIUS: f64 = 0.0;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "skiareshape-common",
        gst::DebugColorFlags::empty(),
        Some("Common Reshape video with skia functionality"),
    )
});

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub border_radius_px: f64,
    pub corner_smoothing_pct: f64,
    pub padding_px: i32,
    pub crop_left: i32,
    pub crop_right: i32,
    pub crop_top: i32,
    pub crop_bottom: i32,
    pub disable_crop_optimization: bool,
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
            disable_crop_optimization: true,
        }
    }
}

#[derive(Default, Debug)]
pub struct State {
    pub in_info: Option<gst_video::VideoInfo>,
    pub out_info: Option<gst_video::VideoInfo>,

    // Wanted position of the image taking into account padding
    pub compositor_position: Option<skia::Rect>,

    // Output size of the **compositor itself**
    pub compositor_size: Option<skia::Size>,
}

#[derive(Debug, Copy, Clone)]
pub struct TranslationRects {
    // The rectangle that is used to crop the source image
    pub src_with_cropping_applied: skia::Rect,

    // The rectangle that is used to draw the image
    pub dst_rect: skia::Rect,

    // The rectangle that correspond to where the optimized cropped image correspond
    // after cropping is applied
    pub original_dst_rect: skia::Rect,

    // The final position in the compositor, after taking account all cropping
    pub final_position: skia::Rect,
}

pub(crate) fn reshape_properties() -> Vec<glib::ParamSpec> {
    vec![
        glib::ParamSpecDouble::builder("border-radius-px")
            .nick("Border radius in pixels")
            .blurb("Draw rounded corners with given border radius")
            .minimum(0.0)
            .default_value(DEFAULT_BORDER_RADIUS)
            .mutable_playing()
            .controllable()
            .build(),
        glib::ParamSpecDouble::builder("corner-smoothing-pct")
            .nick("Corner smoothing in percentage")
            .blurb("Draw rounded corners with corner smoothing")
            .default_value(0.0)
            .mutable_playing()
            .controllable()
            .build(),
        glib::ParamSpecInt::builder("padding-px")
            .nick("Padding in pixels")
            .blurb("Extending the box for drawing borders/shadows")
            .default_value(0)
            .mutable_playing()
            .build(),
        glib::ParamSpecInt::builder("left")
            .nick("Crop left in pixels")
            .blurb("Crop left in pixels")
            .default_value(0)
            .mutable_playing()
            .build(),
        glib::ParamSpecInt::builder("right")
            .nick("Crop right in pixels")
            .blurb("Crop right in pixels")
            .default_value(0)
            .mutable_playing()
            .controllable()
            .build(),
        glib::ParamSpecInt::builder("top")
            .nick("Crop top in pixels")
            .blurb("Crop top in pixels")
            .default_value(0)
            .mutable_playing()
            .controllable()
            .build(),
        glib::ParamSpecInt::builder("bottom")
            .nick("Crop bottom in pixels")
            .blurb("Crop bottom in pixels")
            .default_value(0)
            .mutable_playing()
            .controllable()
            .build(),
        glib::ParamSpecBoolean::builder("disable-crop-optimization")
            .nick("Disable crop optimization")
            .blurb("Disable the 'big zoom' crop optimization that reduces output buffer size")
            .default_value(true)
            .mutable_playing()
            .controllable()
            .build(),
    ]
}

/// Common functionality for reshape implementations
pub trait ReshapeCommon: BaseTransformImpl + ObjectImpl {
    fn settings(&self) -> std::sync::MutexGuard<'_, Settings>;
    fn state(&self) -> std::sync::MutexGuard<'_, State>;

    fn reshape_set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings();
        match pspec.name() {
            "border-radius-px" => {
                settings.border_radius_px = value.get().expect("type checked upstream");
            }
            "corner-smoothing-pct" => {
                settings.corner_smoothing_pct = value.get().expect("type checked upstream");
            }
            "padding-px" => {
                settings.padding_px = value.get().expect("type checked upstream");
            }
            "left" => {
                settings.crop_left = value.get().expect("type checked upstream");
            }
            "right" => {
                settings.crop_right = value.get().expect("type checked upstream");
            }
            "top" => {
                settings.crop_top = value.get().expect("type checked upstream");
            }
            "bottom" => {
                settings.crop_bottom = value.get().expect("type checked upstream");
            }
            "disable-crop-optimization" => {
                settings.disable_crop_optimization = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn reshape_property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings();
        match pspec.name() {
            "border-radius-px" => settings.border_radius_px.to_value(),
            "corner-smoothing-pct" => settings.corner_smoothing_pct.to_value(),
            "padding-px" => settings.padding_px.to_value(),
            "left" => settings.crop_left.to_value(),
            "right" => settings.crop_right.to_value(),
            "top" => settings.crop_top.to_value(),
            "bottom" => settings.crop_bottom.to_value(),
            "disable-crop-optimization" => settings.disable_crop_optimization.to_value(),
            _ => unimplemented!(),
        }
    }

    fn reshape(
        &self,
        canvas: &skia::Canvas,
        image: &skia::Image,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let crop_optimization_enabled = !self.settings().disable_crop_optimization;
        gst::trace!(
            CAT,
            imp = self,
            "Drawing frame with crop optimization: {}",
            if crop_optimization_enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
        let rects = self.compute_src_image_and_dest_rects(None)?;

        // Clear the whole canvas, else we get artifacts from the previous frame
        canvas.clear(skia::Color::TRANSPARENT);

        // Draw the video frame at the correct position, with anti-aliasing.
        let mut paint = skia::Paint::default();
        paint.set_anti_alias(true);

        let src_rect = Some((
            &rects.src_with_cropping_applied,
            skia::canvas::SrcRectConstraint::Strict,
        ));

        canvas.draw_image_rect_with_sampling_options(
            image,
            src_rect,
            rects.dst_rect,
            skia::SamplingOptions::new(skia::FilterMode::Linear, skia::MipmapMode::Linear),
            &paint,
        );

        // Clip out the rounded corners
        let border_radius = self.settings().border_radius_px as f32;
        let rounded_dst_rect =
            skia::RRect::new_rect_xy(rects.original_dst_rect, border_radius, border_radius);

        canvas.clip_rrect(rounded_dst_rect, skia::ClipOp::Difference, true);
        canvas.clear(skia::Color::TRANSPARENT);

        Ok(gst::FlowSuccess::Ok)
    }

    fn reshape_before_transform(&self, inbuf: &gst::BufferRef) {
        let timestamp = inbuf.pts().expect("Buffer without PTS");
        let segment = self.obj().segment().downcast::<gst::ClockTime>().ok();
        let stream_time = segment.as_ref().and_then(|s| s.to_stream_time(timestamp));

        match stream_time {
            Some(stream_time) => match self.obj().sync_values(stream_time) {
                Ok(_) => (),
                Err(err) => {
                    gst::trace!(CAT, imp = self, "Failed to sync values: {:?}", err);
                }
            },
            None => {
                gst::trace!(CAT, imp = self, "No stream time available");
            }
        }
    }

    fn reshape_set_caps(
        &self,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
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
            "Scaling from {}x{} to {}x{}",
            in_info.width(),
            in_info.height(),
            out_info.width(),
            out_info.height(),
        );

        {
            let mut state = self.state();
            state.in_info = Some(in_info);
            state.out_info = Some(out_info);
        }

        self.parent_set_caps(incaps, outcaps)
    }

    fn reshape_start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting");
        self.setup_compositor_size();
        self.parent_start()
    }

    fn reshape_submit_input_buffer(
        &self,
        is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (compositor_position, _) = self.frame_composition_info(&inbuf);

        {
            let mut state = self.state();
            if state.compositor_position != compositor_position {
                state.compositor_position = compositor_position;
                gst::debug!(
                    CAT,
                    imp = self,
                    "Compositor position changed to: {:?}",
                    state.compositor_position,
                );
                drop(state);

                self.obj().reconfigure_src();
            }
        }

        self.parent_submit_input_buffer(is_discont, inbuf)
    }

    fn reshape_copy_metadata(
        &self,
        inbuf: &gst::BufferRef,
        mut_outbuf: &mut gst::BufferRef,
    ) -> Result<(), gst::LoggableError> {
        let settings = self.settings();
        let crop_optimization_enabled = !settings.disable_crop_optimization;

        add_custom_meta(mut_outbuf, settings);
        add_original_frame_meta(mut_outbuf, inbuf);

        self.parent_copy_metadata(inbuf, mut_outbuf)?;

        // Update FrameCompositionMeta to also have the floored/ceiled values.
        let compositor_position = self.state().compositor_position;
        let (mut croppedx, mut croppedy): (f64, f64) = (0.0, 0.0);
        if let Some(compositor_rect) = compositor_position {
            let rects = self.compute_src_image_and_dest_rects(None);
            #[cfg(feature = "ges")]
            if let Some(mut meta) = mut_outbuf.meta_mut::<ges::prelude::FrameCompositionMeta>() {
                // We need to floor the x and y values, and ceil the width and height
                let mut x = compositor_rect.x() as f64;
                let mut y = compositor_rect.y() as f64;
                let mut width = compositor_rect.width() as f64;
                let mut height = compositor_rect.height() as f64;

                if crop_optimization_enabled {
                    gst::log!(CAT, imp = self, "Big zoom optimization enabled");
                    if let Ok(rects) = rects {
                        width = rects.final_position.width() as f64;
                        height = rects.final_position.height() as f64;
                        if compositor_rect.left() < 0. {
                            x = 0.;
                            croppedx = compositor_rect.left() as f64;
                        }

                        if compositor_rect.top() < 0. {
                            y = 0.;
                            croppedy = compositor_rect.top() as f64;
                        }
                    }
                }

                meta.set_pos_x(x.floor());
                meta.set_pos_y(y.floor());
                meta.set_width(width.ceil());
                meta.set_height(height.ceil());
            }

            if crop_optimization_enabled {
                if let Ok(mut meta) = gst::meta::CustomMeta::from_mut_buffer(
                    mut_outbuf,
                    "OriginalFrameCompositionMeta",
                ) {
                    if let Ok(rects) = rects {
                        let s = meta.mut_structure();

                        s.set("croppedx", croppedx);
                        s.set("croppedy", croppedy);

                        // When content is cropped from negative positions, we need to adjust
                        // for the fractional pixels that were lost in the cropping
                        let pos_x = if croppedx < 0.0 {
                            // croppedx is negative, representing content cropped from the left
                            // We need to add back the fractional part that was lost
                            rects.original_dst_rect.left() as f64 - croppedx.fract()
                        } else {
                            rects.original_dst_rect.left() as f64
                        };

                        let pos_y = if croppedy < 0.0 {
                            // croppedy is negative, representing content cropped from the top
                            // We need to add back the fractional part that was lost
                            rects.original_dst_rect.top() as f64 - croppedy.fract()
                        } else {
                            rects.original_dst_rect.top() as f64
                        };

                        s.set("posx", pos_x);
                        s.set("posy", pos_y);
                        s.set("height", rects.original_dst_rect.height() as f64);
                        s.set("width", rects.original_dst_rect.width() as f64);
                    }
                } else {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Failed to get OriginalFrameCompositionMeta"
                    );
                }
            }

            #[cfg(not(feature = "ges"))]
            {
                let _ = compositor_rect;
            }
        }

        Ok(())
    }

    fn reshape_transform_caps(
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
                    let settings = self.settings();
                    caps.get_mut()
                        .unwrap()
                        .map_in_place(move |_features, structure| {
                            if let Ok(width) = structure.get::<i32>("width") {
                                structure.set(
                                    "width",
                                    width - settings.crop_left - settings.crop_right
                                        + 2 * settings.padding_px,
                                );
                            }

                            if let Ok(height) = structure.get::<i32>("height") {
                                structure.set(
                                    "height",
                                    height - settings.crop_top - settings.crop_top
                                        + 2 * settings.padding_px,
                                );
                            }

                            std::ops::ControlFlow::Continue(())
                        });
                    gst::debug!(CAT, imp = self, "Transformed caps: {caps:?}");
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

                gst::debug!(CAT, imp = self, "Transformed caps: {caps:?}");

                self.parent_transform_caps(direction, &caps, filter)
            }
            _ => unreachable!(),
        }
    }

    fn frame_composition_info(&self, _buf: &gst::BufferRef) -> (Option<skia::Rect>, f64) {
        #[cfg(feature = "ges")]
        {
            if let Some(meta) = _buf.meta::<ges::prelude::FrameCompositionMeta>() {
                let padding = self.settings().padding_px as f32;
                (
                    Some(skia::Rect::from_xywh(
                        meta.pos_x() as f32,
                        meta.pos_y() as f32,
                        meta.width() as f32 + padding * 2.,
                        meta.height() as f32 + padding * 2.,
                    )),
                    meta.alpha(),
                )
            } else {
                (None, 1.0)
            }
        }
        #[cfg(not(feature = "ges"))]
        (None, 1.0)
    }

    #[cfg(not(feature = "ges"))]
    fn setup_compositor_size(&self) {}

    #[cfg(feature = "ges")]
    fn setup_compositor_size(&self) {
        let mut parent = Some(self.obj().clone().upcast::<gst::Object>());
        while let Some(p) = parent {
            if let Some(track) = p.downcast_ref::<ges::VideoTrack>() {
                let mut state = self.state();
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
                            gst::info!(CAT, "Failed to get width/height from restriction caps");
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
    }

    fn compute_output_size(&self, caps: &gst::Caps) -> (Option<i32>, Option<i32>) {
        if let Ok(rects) = self.compute_src_image_and_dest_rects(Some(caps)) {
            (
                if rects.final_position.width() <= 0. {
                    None
                } else {
                    Some(rects.final_position.width().ceil() as i32)
                },
                if rects.final_position.height() <= 0. {
                    None
                } else {
                    Some(rects.final_position.height().ceil() as i32)
                },
            )
        } else if let Some(rect) = self.state().compositor_position {
            (
                Some(rect.width().ceil() as i32),
                Some(rect.height().ceil() as i32),
            )
        } else {
            (None, None)
        }
    }

    fn compute_src_image_and_dest_rects(
        &self,
        incaps: Option<&gst::Caps>,
    ) -> Result<TranslationRects, gst::FlowError> {
        let state = self.state();
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
            gst::element_imp_error!(self, gst::CoreError::Negotiation, ["Have no state yet"]);
            return Err(gst::FlowError::NotNegotiated);
        };

        let compositor_size = state.compositor_size.unwrap_or(skia::Size::new(
            in_info.width() as f32,
            in_info.height() as f32,
        ));

        let out_frame_size = if let Some(out_info) = state.out_info.as_ref() {
            skia::Size::new(out_info.width() as f32, out_info.height() as f32)
        } else {
            compositor_size
        };
        let in_frame_size = if let Some(in_info) = state.in_info.as_ref() {
            skia::Size::new(in_info.width() as f32, in_info.height() as f32)
        } else {
            compositor_size
        };

        let (padding_px, mut src_crop_left, crop_right, mut src_crop_top, crop_bottom) = {
            let settings = self.settings();
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
        let (
            mut dst_left,
            mut dst_top,
            compositor_rect,
            img_compositor_rect,
            _has_compositor_position,
        ) = if let Some(ref position_in_compositor) = state.compositor_position {
            let x = position_in_compositor.left();
            let y = position_in_compositor.top();

            (
                x - x.floor(),
                y - y.floor(),
                *position_in_compositor,
                skia::Rect::from_xywh(
                    position_in_compositor.x() + padding_px,
                    position_in_compositor.y() + padding_px,
                    position_in_compositor.width() - 2. * padding_px,
                    position_in_compositor.height() - 2. * padding_px,
                ),
                true,
            )
        } else {
            let compositor_rect = skia::Rect::from_xywh(
                0.,
                0.,
                in_frame_size.width + 2. * padding_px as f32,
                in_frame_size.height + 2. * padding_px as f32,
            );
            gst::debug!(
                CAT,
                imp = self,
                "No compositor position set, using compositor rect: {compositor_rect:?}",
            );
            (
                0.0,
                0.0,
                compositor_rect,
                skia::Rect::from_xywh(0., 0., in_frame_size.width, in_frame_size.height),
                false,
            )
        };
        drop(state);

        let mut src_width = in_info.width() as f32 - crop_right;
        let mut src_height = in_info.height() as f32 - crop_bottom;

        let mut dst_width = if img_compositor_rect.width() > 0. {
            img_compositor_rect.width()
        } else {
            out_frame_size.width - 2. * padding_px
        };
        let mut dst_height = if img_compositor_rect.height() > 0. {
            img_compositor_rect.height()
        } else {
            out_frame_size.height - 2. * padding_px
        };

        let (mut original_dst_left, mut original_dst_top) = (dst_left, dst_top);
        let (original_dst_width, original_dst_height) = (dst_width, dst_height);

        let (out_x, out_y) = (compositor_rect.x(), compositor_rect.y());
        let (mut out_width, mut out_height) = (compositor_rect.width(), compositor_rect.height());

        let crop_optimization_enabled = !self.settings().disable_crop_optimization;

        if crop_optimization_enabled {
            let width_factor = (in_info.width() as f32) / dst_width;
            let height_factor = in_info.height() as f32 / dst_height;

            if img_compositor_rect.left() < 0. {
                let extra_crop_left_src = img_compositor_rect.left() * width_factor;

                src_crop_left -= extra_crop_left_src;
                dst_width += img_compositor_rect.left();

                original_dst_left += img_compositor_rect.left();
                out_width += compositor_rect.left();
            } else if compositor_rect.left() < 0. {
                dst_left += img_compositor_rect.left();
                original_dst_left += img_compositor_rect.left();

                out_width += compositor_rect.left();
            } else {
                dst_left += padding_px;
                original_dst_left += padding_px;
            }

            if img_compositor_rect.right() > compositor_size.width {
                let cropped = compositor_size.width - img_compositor_rect.right();
                let extra_crop_right = cropped * width_factor;

                src_width += extra_crop_right;
                dst_width += cropped;
            }

            if compositor_rect.right() > compositor_size.width {
                out_width += compositor_size.width - compositor_rect.right();
            }

            if img_compositor_rect.top() < 0. {
                let extra_crop_top = img_compositor_rect.top() * height_factor;

                src_crop_top -= extra_crop_top;
                dst_height += img_compositor_rect.top();
                original_dst_top += img_compositor_rect.top();
                out_height += compositor_rect.top();
            } else if compositor_rect.top() < 0. {
                dst_top += compositor_rect.top() + padding_px;
                original_dst_top += compositor_rect.top() + padding_px;
                out_height += compositor_rect.top();
            } else {
                dst_top += padding_px;
                original_dst_top += padding_px;
            }

            if img_compositor_rect.bottom() > compositor_size.height {
                let cropped = compositor_size.height - img_compositor_rect.bottom();
                let extra_crop_bottom = cropped * height_factor;

                src_height += extra_crop_bottom;
                dst_height += cropped;
            }

            if compositor_rect.bottom() > compositor_size.height {
                out_height += compositor_size.height - compositor_rect.bottom();
            }
        } else {
            dst_left += padding_px;
            dst_top += padding_px;

            original_dst_left += padding_px;
            original_dst_top += padding_px;
        }

        let src_with_cropping_applied =
            skia::Rect::from_ltrb(src_crop_left, src_crop_top, src_width, src_height);

        if dst_top < 1.0 {
            dst_top = 0.0;
        }
        if dst_left < 1.0 {
            dst_left = 0.0;
        }
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
            final_position: skia::Rect::from_xywh(out_x, out_y, out_width, out_height),
        })
    }
}

pub fn add_custom_meta(outbuf: &mut gst::BufferRef, settings: std::sync::MutexGuard<'_, Settings>) {
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

pub fn add_original_frame_meta(outbuf: &mut gst::BufferRef, inbuf: &gst::BufferRef) {
    #[cfg(feature = "ges")]
    {
        if let Some(meta) = inbuf.meta::<ges::prelude::FrameCompositionMeta>() {
            let mut new_meta = if let Ok(new_meta) =
                gst::meta::CustomMeta::add(outbuf, "OriginalFrameCompositionMeta")
            {
                new_meta
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
            gst::trace!(CAT, "OriginalFrameCompositionMeta: {:#?}", s);
        }
    }
    #[cfg(not(feature = "ges"))]
    {
        let _ = (outbuf, inbuf);
    }
}
