// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use gst_videoconverter::VideoConverter;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rsvideoconvert",
        gst::DebugColorFlags::empty(),
        Some("Rust Video Format Converter using yuv crate"),
    )
});

#[derive(Default)]
struct State {
    converter: Option<VideoConverter>,
}

#[derive(Default)]
pub struct RsVideoConvert {
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for RsVideoConvert {
    const NAME: &'static str = "GstRsVideoConvert";
    type Type = super::RsVideoConvert;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for RsVideoConvert {}

impl GstObjectImpl for RsVideoConvert {}

impl ElementImpl for RsVideoConvert {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Rust Video Format Converter",
                "Filter/Converter/Video",
                "Converts between RGB and YUV video formats using the yuv crate",
                "GStreamer Rust Plugin Developers",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            // Support all video formats - we use yuv crate when possible,
            // but fall back to GStreamer's VideoConverter for other formats
            let caps = gst::Caps::builder("video/x-raw")
                .field(
                    "format",
                    gst::List::new(gst_video::VideoFormat::iter_raw().map(|f| f.to_str())),
                )
                .field("width", gst::IntRange::new(1, i32::MAX))
                .field("height", gst::IntRange::new(1, i32::MAX))
                .field(
                    "framerate",
                    gst::FractionRange::new(
                        gst::Fraction::new(0, 1),
                        gst::Fraction::new(i32::MAX, 1),
                    ),
                )
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for RsVideoConvert {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut other_caps = gst::Caps::new_empty();

        {
            let other_caps_mut = other_caps.get_mut().unwrap();

            // For each input caps structure, create output structures for all supported formats
            // This preserves width, height, framerate, etc. but allows format conversion
            for s in caps.iter() {
                for format in gst_video::VideoFormat::iter_raw() {
                    let mut s_out = s.to_owned();
                    s_out.set("format", format.to_str());

                    // Remove YUV-specific fields when converting to RGB formats
                    let out_format = gst_video::VideoFormat::from_string(format.to_str());
                    if let Ok(out_info) = gst_video::VideoInfo::builder(out_format, 1, 1).build() {
                        if out_info.is_rgb() {
                            // RGB formats don't have chroma subsampling or YUV colorimetry
                            s_out.remove_field("chroma-site");
                            // Colorimetry will be re-added during fixate_caps if appropriate
                            s_out.remove_field("colorimetry");
                        }
                    }

                    other_caps_mut.append_structure(s_out);
                }
            }
        }

        gst::debug!(
            CAT,
            imp = self,
            "Transformed caps from {} to {} in direction {:?}",
            caps,
            other_caps,
            direction
        );

        if let Some(filter) = filter {
            Some(filter.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First))
        } else {
            Some(other_caps)
        }
    }

    fn fixate_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        mut othercaps: gst::Caps,
    ) -> gst::Caps {
        gst::debug!(
            CAT,
            imp = self,
            "Fixating othercaps {:?} based on caps {:?} in direction {:?}",
            othercaps,
            caps,
            direction
        );

        // Get the input video info
        let in_info = match gst_video::VideoInfo::from_caps(caps) {
            Ok(info) => info,
            Err(_) => {
                gst::warning!(CAT, imp = self, "Failed to parse input caps");
                return BaseTransformImplExt::parent_fixate_caps(self, direction, caps, othercaps);
            }
        };

        let othercaps = othercaps.make_mut();

        // Since we don't support scaling, fixate width, height, and framerate to match input
        for i in 0..othercaps.size() {
            let s = othercaps.structure_mut(i).unwrap();

            // Fixate width to input width
            s.fixate_field_nearest_int("width", in_info.width() as i32);

            // Fixate height to input height
            s.fixate_field_nearest_int("height", in_info.height() as i32);

            // Fixate framerate to input framerate if present
            if s.has_field_with_type("framerate", gst::Fraction::static_type()) {
                s.fixate_field_nearest_fraction(
                    "framerate",
                    gst::Fraction::new(in_info.fps().numer() as i32, in_info.fps().denom() as i32),
                );
            }

            // Fixate pixel-aspect-ratio to input PAR if present
            if s.has_field_with_type("pixel-aspect-ratio", gst::Fraction::static_type()) {
                s.fixate_field_nearest_fraction(
                    "pixel-aspect-ratio",
                    gst::Fraction::new(in_info.par().numer() as i32, in_info.par().denom() as i32),
                );
            }
        }

        // Now handle format fixation - find the best format match
        let othercaps = self.fixate_format(caps, othercaps.copy(), direction);

        // Let parent do final fixation
        let othercaps = self.parent_fixate_caps(direction, caps, othercaps);

        gst::debug!(CAT, imp = self, "Fixated othercaps to {:?}", othercaps);

        othercaps
    }
}

impl RsVideoConvert {
    /// Fixate the format field, trying to find the best match for the input format
    fn fixate_format(
        &self,
        caps: &gst::Caps,
        mut othercaps: gst::Caps,
        direction: gst::PadDirection,
    ) -> gst::Caps {
        // Try to intersect with input caps first to see if we can preserve the format
        let intersection = othercaps.intersect(caps);

        if !intersection.is_empty() {
            // If we can keep the same format, prefer that
            gst::debug!(
                CAT,
                imp = self,
                "Found intersection, preferring to preserve input format"
            );
            let mut result = intersection;
            result.fixate();
            return result;
        }

        // Get input format info
        let in_s = caps.structure(0).unwrap();
        let in_format_str = match in_s.get::<&str>("format") {
            Ok(f) => f,
            Err(_) => {
                gst::debug!(CAT, imp = self, "No input format, using default fixation");
                othercaps.fixate();
                return othercaps;
            }
        };

        let in_format = gst_video::VideoFormat::from_string(in_format_str);

        gst::debug!(
            CAT,
            imp = self,
            "Input format: {} ({:?})",
            in_format_str,
            in_format
        );

        // Score each possible output format and find the best match
        let mut best_score = i32::MAX;
        let mut best_format = None;

        if let Some(out_s) = othercaps.structure(0) {
            if let Ok(format_value) = out_s.get::<glib::Value>("format") {
                let formats: Vec<String> = if format_value.type_() == glib::Type::STRING {
                    vec![format_value.get::<String>().unwrap()]
                } else if let Ok(list) = format_value.get::<gst::List>() {
                    list.iter().filter_map(|v| v.get::<String>().ok()).collect()
                } else {
                    Vec::new()
                };

                for format_str in formats {
                    let format = gst_video::VideoFormat::from_string(&format_str);
                    let score = self.score_format_conversion(in_format, format);

                    gst::trace!(CAT, imp = self, "Format {} score: {}", format_str, score);

                    if score < best_score {
                        best_score = score;
                        best_format = Some(format_str);
                    }

                    if score == 0 {
                        break; // Perfect match
                    }
                }
            }
        }

        // Apply the best format if found
        if let Some(format) = best_format {
            gst::debug!(
                CAT,
                imp = self,
                "Selected best format: {} (score: {})",
                format,
                best_score
            );

            let othercaps_mut = othercaps.make_mut();
            if let Some(s) = othercaps_mut.structure_mut(0) {
                s.set("format", format);
            }
        }

        // For sink direction, try to preserve colorimetry and chroma-site from input
        if direction == gst::PadDirection::Sink {
            othercaps = self.transfer_colorimetry_from_input(caps, othercaps);
        }

        othercaps.fixate();
        othercaps
    }

    /// Score a format conversion based on information loss
    /// Lower score = better match
    fn score_format_conversion(
        &self,
        in_format: gst_video::VideoFormat,
        out_format: gst_video::VideoFormat,
    ) -> i32 {
        let mut score = 0i32;

        // Exact match is perfect
        if in_format == out_format {
            return 0;
        }

        // Create temporary VideoInfo objects to get format information
        let in_info = match gst_video::VideoInfo::builder(in_format, 320, 240).build() {
            Ok(info) => info,
            Err(_) => return i32::MAX, // Unknown format
        };
        let out_info = match gst_video::VideoInfo::builder(out_format, 320, 240).build() {
            Ok(info) => info,
            Err(_) => return i32::MAX, // Unknown format
        };

        let in_fmt_info = in_info.format_info();
        let out_fmt_info = out_info.format_info();

        // Penalize depth differences heavily
        let in_depth = in_fmt_info.depth()[0] as i32;
        let out_depth = out_fmt_info.depth()[0] as i32;
        let depth_diff = (in_depth - out_depth).abs();
        score += depth_diff * 10;

        // Penalize different number of components
        if in_fmt_info.n_components() != out_fmt_info.n_components() {
            score += 50;
        }

        // Penalize different color spaces (RGB vs YUV vs GRAY)
        let in_is_rgb = in_info.is_rgb();
        let in_is_yuv = in_info.is_yuv();
        let in_is_gray = in_info.is_gray();

        let out_is_rgb = out_info.is_rgb();
        let out_is_yuv = out_info.is_yuv();
        let out_is_gray = out_info.is_gray();

        if in_is_rgb != out_is_rgb || in_is_yuv != out_is_yuv || in_is_gray != out_is_gray {
            score += 30; // Color space conversion penalty
        }

        // Penalize alpha differences
        use gst_video::VideoFormatFlags;
        let in_has_alpha = in_fmt_info.flags().contains(VideoFormatFlags::ALPHA);
        let out_has_alpha = out_fmt_info.flags().contains(VideoFormatFlags::ALPHA);
        if in_has_alpha != out_has_alpha {
            score += 20;
        }

        // Prefer formats with same packing
        if in_fmt_info.flags().contains(VideoFormatFlags::TILED)
            != out_fmt_info.flags().contains(VideoFormatFlags::TILED)
        {
            score += 40;
        }

        score
    }

    /// Transfer colorimetry and chroma-site information from input to output when appropriate
    fn transfer_colorimetry_from_input(
        &self,
        caps: &gst::Caps,
        mut othercaps: gst::Caps,
    ) -> gst::Caps {
        let in_s = match caps.structure(0) {
            Some(s) => s,
            None => return othercaps,
        };

        let mut othercaps_owned = othercaps.make_mut().copy();
        let out_s = match othercaps_owned.make_mut().structure_mut(0) {
            Some(s) => s,
            None => return othercaps_owned,
        };

        // Get format information
        let in_format_str = in_s.get::<&str>("format").ok();
        let out_format_str = out_s.get::<&str>("format").ok();

        if let (Some(in_fmt), Some(out_fmt)) = (in_format_str, out_format_str) {
            let in_format = gst_video::VideoFormat::from_string(in_fmt);
            let out_format = gst_video::VideoFormat::from_string(out_fmt);

            // Create temporary VideoInfo objects to get format information
            let in_info = match gst_video::VideoInfo::builder(in_format, 320, 240).build() {
                Ok(info) => info,
                Err(_) => return othercaps_owned,
            };
            let out_info = match gst_video::VideoInfo::builder(out_format, 320, 240).build() {
                Ok(info) => info,
                Err(_) => return othercaps_owned,
            };

            let in_fmt_info = in_info.format_info();
            let out_fmt_info = out_info.format_info();

            let in_is_rgb = in_info.is_rgb();
            let in_is_yuv = in_info.is_yuv();
            let in_is_gray = in_info.is_gray();

            let out_is_rgb = out_info.is_rgb();
            let out_is_yuv = out_info.is_yuv();
            let out_is_gray = out_info.is_gray();

            // Transfer colorimetry if we're staying in the same color space
            if (in_is_rgb && out_is_rgb) || (in_is_yuv && out_is_yuv) || (in_is_gray && out_is_gray)
            {
                if let Ok(colorimetry) = in_s.get::<String>("colorimetry") {
                    gst::debug!(CAT, imp = self, "Preserving colorimetry: {}", colorimetry);
                    out_s.set("colorimetry", colorimetry);
                }
            }

            // Transfer chroma-site for YUV formats with same subsampling
            if in_is_yuv && out_is_yuv {
                // Check if subsampling is the same
                let same_subsampling = in_fmt_info.w_sub() == out_fmt_info.w_sub()
                    && in_fmt_info.h_sub() == out_fmt_info.h_sub();

                if same_subsampling {
                    if let Ok(chroma_site) = in_s.get::<String>("chroma-site") {
                        gst::debug!(CAT, imp = self, "Preserving chroma-site: {}", chroma_site);
                        out_s.set("chroma-site", chroma_site);
                    }
                }
            }
        }

        othercaps_owned
    }
}

impl VideoFilterImpl for RsVideoConvert {
    fn set_info(
        &self,
        _incaps: &gst::Caps,
        in_info: &gst_video::VideoInfo,
        _outcaps: &gst::Caps,
        out_info: &gst_video::VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        let in_format = in_info.format();
        let out_format = out_info.format();

        gst::debug!(
            CAT,
            imp = self,
            "Configuring conversion from {} to {}",
            in_format,
            out_format
        );

        // Get a reference to the element as BaseTransform to set passthrough
        let element = self.obj();
        let base_transform = element.upcast_ref::<gst_base::BaseTransform>();

        // Check if input and output are identical - if so, use passthrough
        if in_info == out_info {
            gst::info!(
                CAT,
                imp = self,
                "Input and output caps are identical, enabling passthrough"
            );
            base_transform.set_passthrough(true);

            // Clear any existing converter since we're in passthrough mode
            let mut state = self.state.lock().unwrap();
            state.converter = None;

            return Ok(());
        }

        // Not identical, disable passthrough and create converter
        base_transform.set_passthrough(false);

        // Create VideoConverter and verify the conversion is supported
        let mut config = gst_video::VideoConverterConfig::new();
        config.set_threads(0);
        let converter = VideoConverter::new(in_info, out_info, Some(config))
            .map_err(|e| gst::loggable_error!(CAT, "Failed to create converter: {}", e))?;

        let mut state = self.state.lock().unwrap();
        state.converter = Some(converter);

        gst::info!(
            CAT,
            imp = self,
            "Successfully configured converter for {} -> {}",
            in_format,
            out_format
        );

        Ok(())
    }

    fn transform_frame(
        &self,
        in_frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
        out_frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();

        let converter = state.converter.as_ref().ok_or_else(|| {
            gst::element_imp_error!(
                self,
                gst::CoreError::Negotiation,
                ["Converter not configured"]
            );
            gst::FlowError::NotNegotiated
        })?;

        converter.frame_ref(in_frame, out_frame).map_err(|e| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Conversion failed: {}", e]);
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}
