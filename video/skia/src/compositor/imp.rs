// SPDX-License-Identifier: MPL-2.0
use gst::glib::Properties;
use gst_base::subclass::prelude::*;
use gst_video::{prelude::*, subclass::prelude::*};
use once_cell::sync::Lazy;

use super::*;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "skiacompositor",
        gst::DebugColorFlags::FG_BLUE,
        Some("Skia compositor"),
    )
});

mod video_format {
    static MAPPINGS: &[(skia::ColorType, gst_video::VideoFormat)] = &[
        (skia::ColorType::RGBA8888, gst_video::VideoFormat::Rgba),
        (skia::ColorType::BGRA8888, gst_video::VideoFormat::Bgra),
        (skia::ColorType::RGB888x, gst_video::VideoFormat::Rgbx),
        (skia::ColorType::RGB565, gst_video::VideoFormat::Rgb16),
        (skia::ColorType::Gray8, gst_video::VideoFormat::Gray8),
    ];

    pub fn gst_to_skia(video_format: gst_video::VideoFormat) -> Option<skia::ColorType> {
        MAPPINGS
            .iter()
            .find_map(|&(ct, vf)| (vf == video_format).then_some(ct))
    }

    pub fn gst_formats() -> Vec<gst_video::VideoFormat> {
        MAPPINGS.iter().map(|&(_, vf)| vf).collect()
    }
}

#[derive(glib::Enum, Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
#[enum_type(name = "GstSkiaCompositorBackground")]
#[repr(u32)]
pub enum SkiaCompositorBackground {
    Checker = 0,
    Black = 1,
    White = 2,
    Transparent = 3,
}

impl Default for SkiaCompositorBackground {
    fn default() -> Self {
        Self::Black
    }
}

#[derive(Default, Properties, Debug)]
#[properties(wrapper_type = super::SkiaCompositor)]
pub struct SkiaCompositor {
    #[property(
        name = "background",
        get,
        set,
        builder(SkiaCompositorBackground::Checker)
    )]
    background: Mutex<SkiaCompositorBackground>,
}

impl SkiaCompositor {
    fn draw_background(&self, canvas: &skia::Canvas, info: &gst_video::VideoInfo) {
        let mut paint = skia::Paint::default();
        match *self.background.lock().unwrap() {
            SkiaCompositorBackground::Black => paint.set_color(skia::Color::BLACK),
            SkiaCompositorBackground::White => paint.set_color(skia::Color::WHITE),
            SkiaCompositorBackground::Transparent => paint.set_color(skia::Color::TRANSPARENT),
            SkiaCompositorBackground::Checker => {
                let square_size: f32 = 10.;
                let size = canvas.base_layer_size();

                for i in 0..(size.width / square_size as i32) {
                    for j in 0..(size.height / square_size as i32) {
                        let is_even = (i + j) % 2 == 0;
                        paint.set_color(if is_even {
                            skia::Color::DARK_GRAY
                        } else {
                            skia::Color::GRAY
                        });

                        let x = i as f32 * square_size;
                        let y = j as f32 * square_size;

                        let rect = skia::Rect::from_xywh(x, y, square_size, square_size);
                        canvas.draw_rect(rect, &paint);
                    }
                }

                return;
            }
        };
        paint.set_style(skia::paint::Style::Fill);
        paint.set_anti_alias(true);

        canvas.draw_rect(
            skia::Rect::from_xywh(0., 0., info.width() as f32, info.height() as f32),
            &paint,
        );
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SkiaCompositor {
    const NAME: &'static str = "SkiaCompositor";
    type Type = super::SkiaCompositor;
    type ParentType = gst_video::VideoAggregator;
    type Interfaces = (gst::ChildProxy,);
}

#[glib::derived_properties]
impl ObjectImpl for SkiaCompositor {}
impl GstObjectImpl for SkiaCompositor {}

impl ElementImpl for SkiaCompositor {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
            std::sync::OnceLock::new();

        Some(ELEMENT_METADATA.get_or_init(|| {
            gst::subclass::ElementMetadata::new(
                "Skia Compositor",
                "Compositor/Video",
                "Skia based compositor",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        }))
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> =
            std::sync::OnceLock::new();

        PAD_TEMPLATES.get_or_init(|| {
            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    // Support formats supported by Skia and GStreamer on the src side
                    &gst_video::VideoCapsBuilder::new()
                        .format_list(video_format::gst_formats())
                        .build();
                )
                .unwrap(),
                gst::PadTemplate::with_gtype(
                    "sink_%u",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    // Support all formats as inputs will be converted to the output format
                    // automatically by the VideoAggregatorConvertPad base class
                    &gst_video::VideoCapsBuilder::new().build(),
                    SkiaCompositorPad::static_type(),
                )
                .unwrap(),
            ]
        })
    }

    // Notify via the child proxy interface whenever a new pad is added or removed.
    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let element = self.obj();
        let pad = self.parent_request_new_pad(templ, name, caps)?;
        element.child_added(&pad, &pad.name());
        Some(pad)
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let element = self.obj();
        element.child_removed(pad, &pad.name());
        self.parent_release_pad(pad);
    }
}

// Implementation of gst_base::Aggregator virtual methods.
impl AggregatorImpl for SkiaCompositor {
    // Called whenever a query arrives at the given sink pad of the compositor.
    fn sink_query(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        // We can accept any input caps that match the pad template. By default
        // videoaggregator only allows caps that have the same format as the output.
        match query.view_mut() {
            QueryViewMut::Caps(q) => {
                let caps = aggregator_pad.pad_template_caps();
                let filter = q.filter();

                let caps = if let Some(filter) = filter {
                    filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First)
                } else {
                    caps
                };

                q.set_result(&caps);

                true
            }
            QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps();
                let template_caps = aggregator_pad.pad_template_caps();
                let res = caps.is_subset(&template_caps);
                q.set_result(res);

                true
            }
            _ => self.parent_sink_query(aggregator_pad, query),
        }
    }
}

impl VideoAggregatorImpl for SkiaCompositor {
    fn find_best_format(
        &self,
        downstream_caps: &gst::Caps,
    ) -> Option<(gst_video::VideoInfo, bool)> {
        gst::info!(CAT, imp: self, "Downstream caps: {}", downstream_caps);

        None
    }

    // Called whenever a new output frame should be produced. At this point, each pad has
    // either no frame queued up at all or the frame that should be used for this output
    // time.
    fn aggregate_frames(
        &self,
        token: &gst_video::subclass::AggregateFramesToken,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let obj = self.obj();

        // Map the output frame writable.
        let out_info = obj.video_info().unwrap();

        let mut mapped_mem = outbuf.map_writable().map_err(|_| gst::FlowError::Error)?;

        let width = out_info.width() as i32;
        let height = out_info.height() as i32;
        let out_img_info = skia::ImageInfo::new(
            skia::ISize { width, height },
            video_format::gst_to_skia(out_info.format()).unwrap(),
            skia::AlphaType::Unpremul,
            None,
        );
        let mut surface =
            skia::surface::surfaces::wrap_pixels(&out_img_info, &mut mapped_mem, None, None)
                .ok_or(gst::FlowError::Error)?;

        let canvas = surface.canvas();

        self.draw_background(canvas, &out_info);

        obj.foreach_sink_pad(|_obj, pad| {
            let pad = pad.downcast_ref::<SkiaCompositorPad>().unwrap();

            let mut paint = skia::Paint::default();

            paint.set_anti_alias(pad.anti_alias());
            paint.set_blend_mode(pad.operator().into());

            let frame = match pad.prepared_frame(token) {
                Some(frame) => frame,
                None => return true,
            };

            paint.set_alpha_f(pad.alpha() as f32);
            let img_info = skia::ImageInfo::new(
                skia::ISize {
                    width: frame.width() as i32,
                    height: frame.height() as i32,
                },
                skia::ColorType::RGBA8888,
                skia::AlphaType::Unpremul,
                None,
            );

            let image = unsafe {
                skia::image::images::raster_from_data(
                    &img_info,
                    &skia::Data::new_bytes(frame.plane_data(0).unwrap()),
                    frame.info().stride()[0] as usize,
                )
            }
            .expect("Wrong image parameters to raster from data.");

            let mut desired_width = pad.width();
            if desired_width <= 0. {
                desired_width = frame.width() as f32;
            }
            let mut desired_height = pad.height();
            if desired_height <= 0. {
                desired_height = frame.height() as f32;
            }
            let src_rect = skia::Rect::from_wh(frame.width() as f32, frame.height() as f32); // Source rectangle
            let dst_rect =
                skia::Rect::from_xywh(pad.xpos(), pad.ypos(), desired_width, desired_height);
            gst::log!(
                CAT,
                imp: self,
                "Drawing frame from pad {} at {:?} to {:?}",
                pad.name(),
                src_rect,
                dst_rect
            );
            canvas.draw_image_rect(
                image,
                Some((&src_rect, skia::canvas::SrcRectConstraint::Strict)),
                dst_rect,
                &paint,
            );

            true
        });

        Ok(gst::FlowSuccess::Ok)
    }
}

impl ChildProxyImpl for SkiaCompositor {
    fn children_count(&self) -> u32 {
        let object = self.obj();
        object.num_pads() as u32
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        let object = self.obj();
        object
            .pads()
            .into_iter()
            .find(|p| p.name() == name)
            .map(|p| p.upcast())
    }

    fn child_by_index(&self, index: u32) -> Option<glib::Object> {
        let object = self.obj();
        object
            .pads()
            .into_iter()
            .nth(index as usize)
            .map(|p| p.upcast())
    }
}
