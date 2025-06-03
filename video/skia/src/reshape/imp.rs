use gst::{
    glib::{self},
    subclass::prelude::*,
};
use gst_video::{prelude::*, subclass::prelude::*, VideoFormat};

use std::sync::{LazyLock, Mutex};
use tracing::*;

use crate::reshape_common::{ReshapeCommon, Settings, State};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "skiareshape",
        gst::DebugColorFlags::empty(),
        Some("Reshape video with skia"),
    )
});

#[derive(Default, Debug)]
pub struct SkiaReshape {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl ReshapeCommon for SkiaReshape {
    fn settings(&self) -> std::sync::MutexGuard<'_, Settings> {
        self.settings.lock().unwrap()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

impl SkiaReshape {}

#[glib::object_subclass]
impl ObjectSubclass for SkiaReshape {
    const NAME: &'static str = "GstSkiaReshape";
    type Type = super::SkiaReshape;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for SkiaReshape {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> =
            LazyLock::new(|| crate::reshape_common::reshape_properties());
        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        self.reshape_property(_id, pspec)
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        self.reshape_set_property(_id, value, pspec)
    }
}

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
            let caps = gst_video::VideoCapsBuilder::new()
                .format(VideoFormat::Rgba)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
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

impl VideoFilterImpl for SkiaReshape {
    #[instrument(skip(self, frame))]
    fn transform_frame(
        &self,
        frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
        outframe: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (_, alpha) = self.frame_composition_info(outframe.buffer());

        if alpha == 0.0 {
            // Skip drawing when alpha is 0
            return Ok(gst::FlowSuccess::Ok);
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
                gst::error!(CAT, imp = self, "Failed to get plane data: {:?}", e);
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

        self.reshape(out_surface.canvas(), &image)
    }
}
