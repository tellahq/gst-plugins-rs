// Copyright (C) 2024 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * element-segmentclipper:
 * @short_description: Clip buffers based on segment
 *
 * This element clips buffers based on the segment event it receives. It will drop buffers
 * that are outside the segment and will clip buffers that are partially outside the segment.
 * It expects all input buffer to have increasing PTS and will push EOS when the segment done.
 *
 * When receiving an EOS event, it will push the last buffer it received if no buffer inside
 * the segment was received. This is useful in combination with the
 * #GstVideoDecoder:drop-out-of-segment property, this element can be
 * used to handlle special cases where the input stream has big gaps (in screen recording for example)
 * and seeking might lead to EOS without any buffer inside the segment, while what use would
 * expect is to have the frame before the gap to be displayed.
 *
 * Since: plugins-rs 0.14.0
 */
use gst::subclass::prelude::*;
use gst::{glib, prelude::*};
use gst_base::subclass::base_transform::GenerateOutputSuccess;
use gst_base::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "segment-clipper",
        gst::DebugColorFlags::empty(),
        Some("Segment Clipper"),
    )
});

#[derive(Debug, Default)]
struct State {
    segment: gst::FormattedSegment<gst::ClockTime>,
    framerate: Option<gst::Fraction>,
    last_dropped_buffer: Option<gst::Buffer>,
}

#[derive(Default)]
pub(crate) struct SegmentClipper {
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for SegmentClipper {
    const NAME: &'static str = "GstSegmentClipper";
    type Type = super::SegmentClipper;
    type ParentType = gst_base::BaseTransform;
}

impl BaseTransformImpl for SegmentClipper {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;

    fn submit_input_buffer(
        &self,
        is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let segment = &state.segment;

        let start = if let Some(pts) = inbuf.pts() {
            pts
        } else {
            gst::warning!(CAT, imp: self, "Dropping buffer without PTS");

            return Ok(gst_base::BASE_TRANSFORM_FLOW_DROPPED);
        };

        let stop = start
            + inbuf.duration().unwrap_or_else(|| {
                state.framerate.map_or(gst::ClockTime::ZERO, |framerate| {
                    gst::ClockTime::from_nseconds(
                        gst::ClockTime::SECOND.nseconds() * framerate.denom() as u64
                            / framerate.numer() as u64,
                    )
                })
            });

        let drop_buffer = |state: &mut State| {
            state.last_dropped_buffer = Some(inbuf.clone());
            gst::debug!(CAT, imp: self, "Dropping buffer outside segment");

            Ok(gst_base::BASE_TRANSFORM_FLOW_DROPPED)
        };

        if segment.rate() > 0.0 {
            // Forward playback
            if stop
                < segment
                    .start()
                    .expect("Can't have a NONE segment.start in forward playback")
            {
                return drop_buffer(&mut state);
            } else if segment.stop().is_some() && Some(start) >= segment.stop() {
                gst::debug!(CAT, imp: self, "Buffer reached end of segment {segment:?}");
                return Err(gst::FlowError::Eos);
            }
        } else {
            // Reverse playback
            if stop
                <= segment
                    .start()
                    .expect("Can't have a NONE segment.start in reverse playback")
            {
                gst::debug!(CAT, imp: self, "Buffer reached end of segment");
                return Err(gst::FlowError::Eos);
            } else if start
                > segment
                    .stop()
                    .expect("Can't have a NONE segment.stop in reverse playback")
            {
                return drop_buffer(&mut state);
            }
        }

        self.parent_submit_input_buffer(is_discont, inbuf)
    }

    fn transform_ip(&self, _buf: &mut gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        Ok(gst::FlowSuccess::Ok)
    }

    fn generate_output(
        &self,
    ) -> Result<gst_base::subclass::base_transform::GenerateOutputSuccess, gst::FlowError> {
        let res = self.parent_generate_output()?;
        let mut buffer = if let GenerateOutputSuccess::Buffer(buffer) = res {
            buffer
        } else {
            return Ok(res);
        };

        let mut state = self.state.lock().unwrap();
        let start = buffer.pts().expect("Checked in submit_input_buffer");

        let stop = start
            + buffer.duration().unwrap_or_else(|| {
                state.framerate.map_or(gst::ClockTime::ZERO, |framerate| {
                    gst::ClockTime::from_nseconds(
                        gst::ClockTime::SECOND.nseconds() * framerate.denom() as u64
                            / framerate.numer() as u64,
                    )
                })
            });

        state.last_dropped_buffer = None;
        let (pts, end) = state
            .segment
            .clip(start, stop)
            .expect("Manually checked in submit_input_buffer");
        let buffer_mut = buffer.make_mut();
        if let Some(pts) = pts {
            buffer_mut.set_pts(pts);

            if let Some(end) = end {
                buffer_mut.set_duration(end - pts);
            }
        }

        Ok(GenerateOutputSuccess::Buffer(buffer))
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        if incaps != outcaps {
            return Err(gst::loggable_error!(
                CAT,
                "Input and output caps are not the same"
            ));
        }

        let caps_struct = incaps.structure(0).unwrap();
        self.state.lock().unwrap().framerate = if caps_struct.has_name("video/x-raw") {
            caps_struct.get::<gst::Fraction>("framerate").map_or_else(
                |_| None,
                |f| {
                    if f.numer() == 0 {
                        gst::info!(CAT, "Ignoring variable framerate");
                        None
                    } else {
                        Some(f)
                    }
                },
            )
        } else {
            None
        };

        gst::debug!(CAT, imp: self, "Received caps: {:?}", caps_struct);

        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if let gst::EventView::Segment(segment_event) = event.view() {
            match segment_event
                .segment()
                .clone()
                .downcast::<gst::format::Time>()
            {
                Ok(s) => {
                    let mut state = self.state.lock().unwrap();
                    state.segment = s;
                    state.last_dropped_buffer = None;
                }
                Err(err) => {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ["Time segment needed: {:?}", err]
                    );
                    return false;
                }
            };
        } else if let gst::EventView::FlushStop(..) = event.view() {
            let mut state = self.state.lock().unwrap();
            state.last_dropped_buffer = None;
        } else if let gst::EventView::Eos(..) = event.view() {
            let mut state = self.state.lock().unwrap();
            if let Some(mut last_buffer) = state.last_dropped_buffer.take() {
                let segment = state.segment.clone();
                drop(state);

                let buf = last_buffer.make_mut();
                if segment.rate() > 0.0 {
                    buf.set_pts(segment.start().unwrap());
                } else {
                    buf.set_pts(segment.stop().unwrap());
                }

                gst::info!(CAT, imp: self,
                    "Got EOS event but did not push any yet although we have received buffers \
                     push the last buffer we received clipping it to the segment");
                if let Err(e) = self.obj().src_pads().first().unwrap().push(last_buffer) {
                    gst::error!(CAT, imp: self, "Failed to push last dropped buffer: {:?}", e);
                }
            }
        }

        self.parent_sink_event(event)
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = Default::default();

        Ok(())
    }
}

impl ObjectImpl for SegmentClipper {}
impl GstObjectImpl for SegmentClipper {}
impl ElementImpl for SegmentClipper {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segment Clipper",
                "Filter/Generic",
                "Clip buffers based on segment",
                "Thibault Saunier <tsaunier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
