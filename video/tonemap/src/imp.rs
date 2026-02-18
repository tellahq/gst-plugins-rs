// Copyright (C) 2026, Tella
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! GStreamer `rstonemap` element — HDR→SDR tonemapping as a BaseTransform.
//!
//! Replicates the FFmpeg preprocessing chain from `ffmpeg_utils/src/lib.rs:513-527`:
//!   `zscale=t=linear:npl=100 → gbrpf32le → zscale=p=bt709 → tonemap=hable:desat=0
//!    → zscale=t=bt709:m=bt709:r=tv → yuv420p`
//!
//! Per-pixel pipeline (see `math.rs` for formulas and standard references):
//!   1. PQ/HLG EOTF → linear light (normalized to 100 nits)
//!   2. BT.2020 → BT.709 gamut mapping (3×3 matrix)
//!   3. Hable filmic tonemapping (W=11.2, desat=0)
//!   4. BT.709 OETF (gamma encoding)
//!   5. Clamp + quantize to 8-bit
//!
//! SDR content: passthrough (zero-cost, `transform_ip` never called).
//! Element pattern follows the existing videofx elements (border, colordetect).

use gst::{glib, subclass::prelude::*};
use gst_base::prelude::*;
use gst_video::{subclass::prelude::*, VideoFormat};
use std::sync::LazyLock;
use std::sync::Mutex;

use super::math;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rstonemap",
        gst::DebugColorFlags::empty(),
        Some("HDR to SDR tonemapping"),
    )
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Pq,
    Hlg,
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    force_active: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            force_active: false,
        }
    }
}

struct State {
    active: bool,
    width: usize,
    height: usize,
    stride: usize,
    bytes_per_pixel: usize,
    rgb_offsets: (usize, usize, usize),
    transfer: Transfer,
}

#[derive(Default)]
pub struct RsTonemap {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

fn detect_hdr_transfer(caps: &gst::Caps) -> Option<Transfer> {
    let caps_str = caps.to_string();

    if caps_str.contains("bt2100-pq") || caps_str.contains("smpte-st-2084") {
        Some(Transfer::Pq)
    } else if caps_str.contains("bt2100-hlg") || caps_str.contains("arib-std-b67") {
        Some(Transfer::Hlg)
    } else {
        None
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RsTonemap {
    const NAME: &'static str = "GstRsTonemap";
    type Type = super::RsTonemap;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for RsTonemap {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![glib::ParamSpecBoolean::builder("force-active")
                .nick("Force active")
                .blurb("Force tonemapping active even if colorimetry is not HDR")
                .default_value(false)
                .mutable_playing()
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "force-active" => {
                let mut settings = self.settings.lock().unwrap();
                let force_active = value.get().expect("type checked upstream");
                if settings.force_active != force_active {
                    gst::info!(
                        CAT,
                        imp = self,
                        "Changing force-active from {} to {}",
                        settings.force_active,
                        force_active
                    );
                    settings.force_active = force_active;
                    self.obj().reconfigure_src();
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "force-active" => {
                let settings = self.settings.lock().unwrap();
                settings.force_active.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RsTonemap {}

impl ElementImpl for RsTonemap {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "HDR Tonemapper",
                "Filter/Effect/Converter/Video",
                "Tonemaps HDR (PQ/HLG BT.2020) video to SDR (BT.709)",
                "Tella <dev@tella.tv>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    VideoFormat::Rgba,
                    VideoFormat::Bgra,
                    VideoFormat::Rgb,
                    VideoFormat::Bgr,
                ])
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

impl BaseTransformImpl for RsTonemap {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut result = gst::Caps::new_empty();
        {
            let result_ref = result.make_mut();
            for i in 0..caps.size() {
                let mut s = caps.structure(i).unwrap().to_owned();

                match direction {
                    gst::PadDirection::Sink => {
                        // Querying what src can produce: HDR input → BT.709 output
                        if let Ok(c) = s.get::<String>("colorimetry") {
                            if c.contains("bt2100-hlg")
                                || c.contains("arib-std-b67")
                                || c.contains("bt2100-pq")
                                || c.contains("smpte-st-2084")
                            {
                                s.set("colorimetry", "bt709");
                            }
                        }
                    }
                    gst::PadDirection::Src => {
                        // Querying what sink can accept: remove colorimetry to accept any
                        s.remove_field("colorimetry");
                    }
                    _ => {}
                }

                result_ref.append_structure_full(
                    s,
                    caps.features(i).map(|f| f.to_owned()),
                );
            }
        }

        if let Some(f) = filter {
            Some(result.intersect(f))
        } else {
            Some(result)
        }
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;
        gst::info!(CAT, imp = self, "Stopped");
        Ok(())
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let in_info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse input caps"))?;

        gst::debug!(
            CAT,
            imp = self,
            "Configured for caps {} to {}",
            incaps,
            outcaps
        );

        let force_active = self.settings.lock().unwrap().force_active;
        let hdr_transfer = detect_hdr_transfer(incaps);
        let active = hdr_transfer.is_some() || force_active;
        let transfer = hdr_transfer.unwrap_or(Transfer::Pq);

        let format = in_info.format();
        let (bpp, rgb_offsets) = match format {
            VideoFormat::Rgba => (4, (0usize, 1usize, 2usize)),
            VideoFormat::Bgra => (4, (2, 1, 0)),
            VideoFormat::Rgb => (3, (0, 1, 2)),
            VideoFormat::Bgr => (3, (2, 1, 0)),
            _ => return Err(gst::loggable_error!(CAT, "Unsupported format {:?}", format)),
        };

        if active {
            gst::info!(
                CAT,
                imp = self,
                "HDR tonemapping active: transfer={:?}, format={:?}, force={}",
                transfer,
                format,
                force_active
            );
        } else {
            gst::debug!(CAT, imp = self, "SDR content, passthrough mode");
        }

        *self.state.lock().unwrap() = Some(State {
            active,
            width: in_info.width() as usize,
            height: in_info.height() as usize,
            stride: in_info.stride()[0] as usize,
            bytes_per_pixel: bpp,
            rgb_offsets,
            transfer,
        });

        self.obj().set_passthrough(!active);

        Ok(())
    }

    fn transform_ip(&self, buf: &mut gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (width, height, stride, bpp, (ri, gi, bi), transfer) = {
            let state_guard = self.state.lock().unwrap();
            let state = match state_guard.as_ref() {
                Some(s) if s.active => s,
                _ => return Ok(gst::FlowSuccess::Ok),
            };
            (
                state.width,
                state.height,
                state.stride,
                state.bytes_per_pixel,
                state.rgb_offsets,
                state.transfer,
            )
        };

        let mut map = buf.map_writable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::CoreError::Failed,
                ["Failed to map buffer writable"]
            );
            gst::FlowError::Error
        })?;
        let data = map.as_mut_slice();

        // Per-pixel tonemapping: PQ/HLG EOTF → BT.2020→709 → Hable → BT.709 OETF
        // TODO: For Phase 2/3, add a 256-entry LUT to avoid per-pixel powf calls.
        for y in 0..height {
            let row = y * stride;
            for x in 0..width {
                let p = row + x * bpp;

                let r_in = data[p + ri] as f32 / 255.0;
                let g_in = data[p + gi] as f32 / 255.0;
                let b_in = data[p + bi] as f32 / 255.0;

                let (lr, lg, lb) = match transfer {
                    Transfer::Pq => (
                        math::pq_eotf(r_in),
                        math::pq_eotf(g_in),
                        math::pq_eotf(b_in),
                    ),
                    Transfer::Hlg => (
                        math::hlg_eotf(r_in),
                        math::hlg_eotf(g_in),
                        math::hlg_eotf(b_in),
                    ),
                };

                let (r709, g709, b709) = math::bt2020_to_bt709(lr, lg, lb);
                let (rg, gg, bg) = math::soft_gamut_map(r709, g709, b709);

                let peak = match transfer {
                    Transfer::Pq => 11.2,
                    Transfer::Hlg => 10.0,
                };
                let (rt, gt, bt) = math::hable_tonemap(rg, gg, bg, peak);

                let ro = math::bt709_oetf(rt);
                let go = math::bt709_oetf(gt);
                let bo = math::bt709_oetf(bt);

                data[p + ri] = (ro.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                data[p + gi] = (go.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                data[p + bi] = (bo.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
