//
// Copyright (C) 2023 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    mem,
    sync::{Arc, Mutex},
};

use gst::prelude::*;

/// Expected packet produced by the payloader
pub struct ExpectedPacket {
    /// All packets are expected to have a known and fixed PTS.
    pub pts: gst::ClockTime,
    /// If not set the size will not be checked.
    pub size: Option<usize>,
    pub flags: gst::BufferFlags,
    pub pt: u8,
    pub rtp_time: u32,
    pub marker: bool,
}

impl ExpectedPacket {
    /// Creates a builder for an `ExpectedPacket`.
    ///
    /// Assigns the following packet default values:
    ///
    /// * pts: gst::ClockTime::ZERO
    /// * size: None => not checked
    /// * flags: gst::BufferFlags::empty()
    /// * pt: 96
    /// * rtp_time: 0
    /// * marker: true
    pub fn builder() -> ExpectedPacketBuilder {
        ExpectedPacketBuilder(ExpectedPacket {
            pts: gst::ClockTime::ZERO,
            size: None,
            flags: gst::BufferFlags::empty(),
            pt: 96,
            rtp_time: 0,
            marker: true,
        })
    }
}

pub struct ExpectedPacketBuilder(ExpectedPacket);
impl ExpectedPacketBuilder {
    pub fn pts(mut self, pts: gst::ClockTime) -> Self {
        self.0.pts = pts;
        self
    }

    pub fn size(mut self, size: usize) -> Self {
        self.0.size = Some(size);
        self
    }

    pub fn flags(mut self, flags: gst::BufferFlags) -> Self {
        self.0.flags = flags;
        self
    }

    pub fn pt(mut self, pt: u8) -> Self {
        self.0.pt = pt;
        self
    }

    pub fn rtp_time(mut self, rtp_time: u32) -> Self {
        self.0.rtp_time = rtp_time;
        self
    }

    pub fn marker_bit(mut self, marker: bool) -> Self {
        self.0.marker = marker;
        self
    }

    pub fn build(self) -> ExpectedPacket {
        self.0
    }
}

/// Expected buffer produced by the depayloader
#[derive(Debug)]
pub struct ExpectedBuffer {
    /// If not set then it is asserted that the depayloaded buffer also has no PTS.
    pub pts: Option<gst::ClockTime>,
    /// If not set then it is asserted that the depayloaded buffer also has no DTS.
    pub dts: Option<gst::ClockTime>,
    /// If not set the size will not be checked.
    pub size: Option<usize>,
    pub flags: gst::BufferFlags,
}

impl ExpectedBuffer {
    /// Creates a builder for an `ExpectedBuffer`.
    ///
    /// Assigns the following buffer default values:
    ///
    /// * pts: None
    /// * dts: None
    /// * size: None => not checked
    /// * flags: gst::BufferFlags::empty()
    pub fn builder() -> ExpectedBufferBuilder {
        ExpectedBufferBuilder(ExpectedBuffer {
            pts: None,
            dts: None,
            size: None,
            flags: gst::BufferFlags::empty(),
        })
    }
}

pub struct ExpectedBufferBuilder(ExpectedBuffer);
#[allow(dead_code)]
impl ExpectedBufferBuilder {
    pub fn pts(mut self, pts: gst::ClockTime) -> Self {
        self.0.pts = Some(pts);
        self
    }

    pub fn maybe_pts(mut self, pts: Option<gst::ClockTime>) -> Self {
        self.0.pts = pts;
        self
    }

    pub fn dts(mut self, dts: gst::ClockTime) -> Self {
        self.0.dts = Some(dts);
        self
    }

    pub fn size(mut self, size: usize) -> Self {
        self.0.size = Some(size);
        self
    }

    pub fn maybe_size(mut self, size: Option<usize>) -> Self {
        self.0.size = size;
        self
    }

    pub fn flags(mut self, flags: gst::BufferFlags) -> Self {
        self.0.flags = flags;
        self
    }

    pub fn build(self) -> ExpectedBuffer {
        self.0
    }
}

/// Source of the test
pub enum Source<'a> {
    #[allow(dead_code)]
    Buffers(gst::Caps, Vec<gst::Buffer>),
    Bin(&'a str),
}

/// Pipeline wrapper to automatically set state to `Null` on drop
///
/// Useful to not get critical warnings on panics from unwinding.
struct Pipeline(gst::Pipeline);

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

impl std::ops::Deref for Pipeline {
    type Target = gst::Pipeline;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn run_test_pipeline(
    src: Source,
    pay: &str,
    depay: &str,
    expected_pay: Vec<Vec<ExpectedPacket>>,
    expected_depay: Vec<Vec<ExpectedBuffer>>,
) {
    let pipeline = Pipeline(gst::Pipeline::new());

    // Return if the pipelines can't be built: this likely means that encoders are missing

    let src = match src {
        Source::Bin(src) => {
            let Ok(src) = gst::parse::bin_from_description_with_name(src, true, "rtptestsrc")
            else {
                return;
            };

            src.upcast::<gst::Element>()
        }
        Source::Buffers(caps, buffers) => {
            let mut buffers = buffers.into_iter();
            let appsrc = gst_app::AppSrc::builder()
                .format(gst::Format::Time)
                .caps(&caps)
                .callbacks(
                    gst_app::AppSrcCallbacks::builder()
                        .need_data(move |appsrc, _offset| {
                            let Some(buffer) = buffers.next() else {
                                let _ = appsrc.end_of_stream();
                                return;
                            };

                            // appsrc already handles the error for us
                            let _ = appsrc.push_buffer(buffer);
                        })
                        .build(),
                )
                .build();

            appsrc.upcast::<gst::Element>()
        }
    };

    let Ok(pay) = gst::parse::bin_from_description_with_name(pay, true, "rtptestpay") else {
        return;
    };
    let pay = pay.upcast::<gst::Element>();

    // Collect samples from after the payloader
    let pay_samples = Arc::new(Mutex::new(Vec::new()));
    pay.static_pad("src")
        .unwrap()
        .add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            {
                let pay_samples = pay_samples.clone();
                move |pad, info| {
                    let segment_event = pad.sticky_event::<gst::event::Segment>(0).unwrap();
                    let segment = segment_event.segment().clone();

                    let caps = pad.current_caps().unwrap();

                    let mut sample_builder = gst::Sample::builder().segment(&segment).caps(&caps);
                    if let Some(buffer) = info.buffer() {
                        sample_builder = sample_builder.buffer(buffer);
                    } else if let Some(list) = info.buffer_list() {
                        sample_builder = sample_builder.buffer_list(list);
                    } else {
                        unreachable!();
                    }

                    pay_samples.lock().unwrap().push(sample_builder.build());

                    gst::PadProbeReturn::Ok
                }
            },
        )
        .unwrap();

    let Ok(depay) = gst::parse::bin_from_description_with_name(depay, true, "rtptestdepay") else {
        return;
    };
    let depay = depay.upcast::<gst::Element>();

    let depay_samples = Arc::new(Mutex::new(Vec::new()));
    let appsink = gst_app::AppSink::builder()
        .sync(false)
        .buffer_list(true)
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let depay_samples = depay_samples.clone();
                    move |appsink| {
                        let Ok(sample) = appsink.pull_sample() else {
                            return Err(gst::FlowError::Flushing);
                        };

                        depay_samples.lock().unwrap().push(sample);

                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        )
        .build();

    pipeline
        .add_many([&src, &pay, &depay, appsink.as_ref()])
        .unwrap();
    gst::Element::link_many([&src, &pay, &depay, appsink.as_ref()]).unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set pipeline to Playing");

    let msg = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::NONE,
            &[gst::MessageType::Error, gst::MessageType::Eos],
        )
        .expect("Didn't receive ERROR or EOS message");

    assert_ne!(
        msg.type_(),
        gst::MessageType::Error,
        "Received error message {msg:?}"
    );

    pipeline
        .set_state(gst::State::Null)
        .expect("Failed to set pipeline to Null");

    drop(msg);
    drop(src);
    drop(pay);
    drop(depay);
    drop(appsink);
    drop(pipeline);

    let pay_samples = mem::take(&mut *pay_samples.lock().unwrap());
    let depay_samples = mem::take(&mut *depay_samples.lock().unwrap());

    // Now check against the expected values

    assert_eq!(
        pay_samples.len(),
        expected_pay.len(),
        "Expected {} payload packets but got {}",
        expected_pay.len(),
        pay_samples.len()
    );

    let mut initial_timestamp = None;

    for (i, (expected_list, sample)) in
        Iterator::zip(expected_pay.into_iter(), pay_samples.into_iter()).enumerate()
    {
        let mut iter_a;
        let mut iter_b;

        let buffer_iter: &mut dyn Iterator<Item = &gst::BufferRef> =
            if let Some(list) = sample.buffer_list() {
                assert_eq!(
                    list.len(),
                    expected_list.len(),
                    "Expected {} buffers in {}-th payload list but got {}",
                    expected_list.len(),
                    i,
                    list.len()
                );
                iter_a = list.iter();
                &mut iter_a
            } else {
                let buffer = sample.buffer().unwrap();
                assert_eq!(
                    expected_list.len(),
                    1,
                    "Expected {} buffers in {}-th payload list but got 1",
                    expected_list.len(),
                    i,
                );
                iter_b = Some(buffer).into_iter();
                &mut iter_b
            };

        for (j, (expected_buffer, buffer)) in
            Iterator::zip(expected_list.into_iter(), buffer_iter).enumerate()
        {
            let buffer_pts = buffer.pts().expect("Buffer without PTS");
            assert_eq!(
                buffer_pts, expected_buffer.pts,
                "Buffer {} of payload buffer list {} has unexpected PTS {} instead of {}",
                j, i, buffer_pts, expected_buffer.pts,
            );

            if let Some(expected_size) = expected_buffer.size {
                assert_eq!(
                    buffer.size(),
                    expected_size,
                    "Buffer {} of payload buffer list {} has unexpected size {} instead of {}",
                    j,
                    i,
                    buffer.size(),
                    expected_size,
                );
            }

            let buffer_flags = buffer.flags() - gst::BufferFlags::TAG_MEMORY;
            assert_eq!(
                buffer_flags, expected_buffer.flags,
                "Buffer {} of payload buffer list {} has unexpected flags {:?} instead of {:?}",
                j, i, buffer_flags, expected_buffer.flags,
            );

            let map = buffer.map_readable().unwrap();
            let rtp_packet = rtp_types::RtpPacket::parse(&map).expect("Invalid RTP packet");
            assert_eq!(
                rtp_packet.payload_type(),
                expected_buffer.pt,
                "Buffer {} of payload buffer list {} has unexpected payload type {:?} instead of {:?}",
                j,
                i,
                rtp_packet.payload_type(),
                expected_buffer.pt,
            );

            assert_eq!(
                rtp_packet.marker_bit(),
                expected_buffer.marker,
                "Buffer {} of payload buffer list {} has unexpected marker {:?} instead of {:?}",
                j,
                i,
                rtp_packet.marker_bit(),
                expected_buffer.marker,
            );

            if initial_timestamp.is_none() {
                initial_timestamp = Some(rtp_packet.timestamp());
            }
            let initial_timestamp = initial_timestamp.unwrap();

            let expected_timestamp = expected_buffer.rtp_time.wrapping_add(initial_timestamp);

            assert_eq!(
                rtp_packet.timestamp(),
                expected_timestamp,
                "Buffer {} of payload buffer list {} has unexpected RTP timestamp {:?} instead of {:?}",
                j,
                i,
                rtp_packet.timestamp(),
                expected_timestamp,
            );
        }
    }

    assert_eq!(
        depay_samples.len(),
        expected_depay.len(),
        "Expected {} depayload samples but got {}",
        expected_depay.len(),
        depay_samples.len()
    );

    for (i, (expected_list, sample)) in
        Iterator::zip(expected_depay.into_iter(), depay_samples.into_iter()).enumerate()
    {
        let mut iter_a;
        let mut iter_b;

        let buffer_iter: &mut dyn Iterator<Item = &gst::BufferRef> =
            if let Some(list) = sample.buffer_list() {
                assert_eq!(
                    list.len(),
                    expected_list.len(),
                    "Expected {} depayload buffers in {}-th list but got {}",
                    expected_list.len(),
                    i,
                    list.len()
                );
                iter_a = list.iter();
                &mut iter_a
            } else {
                let buffer = sample.buffer().unwrap();
                assert_eq!(
                    expected_list.len(),
                    1,
                    "Expected {} depayload buffers in {}-th list but got 1",
                    expected_list.len(),
                    i,
                );
                iter_b = Some(buffer).into_iter();
                &mut iter_b
            };

        for (j, (expected_buffer, buffer)) in
            Iterator::zip(expected_list.into_iter(), buffer_iter).enumerate()
        {
            let buffer_pts = buffer.pts();
            assert_eq!(
                buffer_pts,
                expected_buffer.pts,
                "Buffer {} of depayload buffer list {} has unexpected PTS {} instead of {}",
                j,
                i,
                buffer_pts.display(),
                expected_buffer.pts.display(),
            );

            if let Some(expected_size) = expected_buffer.size {
                assert_eq!(
                    buffer.size(),
                    expected_size,
                    "Buffer {} of depayload buffer list {} has unexpected size {} instead of {}",
                    j,
                    i,
                    buffer.size(),
                    expected_size,
                );
            }

            let buffer_flags = buffer.flags() - gst::BufferFlags::TAG_MEMORY;
            assert_eq!(
                buffer_flags, expected_buffer.flags,
                "Buffer {} of depayload buffer list {} has unexpected flags {:?} instead of {:?}",
                j, i, buffer_flags, expected_buffer.flags,
            );
        }
    }
}
