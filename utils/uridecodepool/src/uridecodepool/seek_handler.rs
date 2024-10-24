// SPDX-License-Identifier: MPL-2.0
use crate::uridecodepool::{imp::CAT, DecoderPipeline};
use gst::subclass::prelude::*;
use gst::{glib, prelude::*};
use gst_base::prelude::*;
use std::sync::Mutex;

#[derive(Debug)]
pub(crate) struct SeekHandler {
    state: Mutex<State>,

    name: String,
}

#[derive(Debug)]
struct RemappedSegment {
    sample_segment: gst::FormattedSegment<gst::ClockTime>,
    remapped_segment: gst::Segment,
}

#[derive(Debug, Default)]
struct State {
    stream_time: Option<gst::ClockTime>,
    seek_info: SeekInfo,
    last_remapped_segment: Option<RemappedSegment>,
    pushed_buffer_in_segment: bool,

    // Seek event received from NLE while relinking stack
    nle_seek: Option<gst::Event>,

    // The actual seek event has been handled
    handled_composition_seek: bool,
    probe_id: Option<gst::PadProbeId>,
    pad_probe: glib::WeakRef<gst::Pad>,
}

impl State {
    fn set_seek_info(&mut self, seek_info: SeekInfo) {
        self.seek_info = seek_info;
        self.last_remapped_segment = None;
        self.pushed_buffer_in_segment = false;
    }

    fn reset(&mut self, obj: &glib::Object) {
        gst::debug!(CAT, obj: obj, "Resetting seek state");
        self.stream_time = None;
        self.set_seek_info(SeekInfo::None);
        self.handled_composition_seek = false;
        self.nle_seek = None;
        if let (Some(pad), Some(probe_id)) = (self.pad_probe.upgrade(), self.probe_id.take()) {
            pad.remove_probe(probe_id);
        }
    }
}

#[derive(Debug)]
pub(crate) enum NleCompositionSeekResult {
    /// The seek is unexpected or invalid - handle it as a regular seek
    Unexpected,
    /// The seek matches expectations - use it as our seek
    Expected,
    /// The seek is expected but we haven't received an initialization seek
    /// Assume our underlying pipeline is a nested timeline and the unrelying
    /// timeline will be the one that seeks
    UseSeqnum(gst::Seqnum),
}

#[derive(Clone, Debug, Default)]
pub(crate) enum SeekInfo {
    #[default]
    None,
    SeekSegment(gst::Seqnum, gst::Segment),
    PreviousSeekDone(gst::Sample, Option<gst::Segment>),
}

impl SeekHandler {
    pub fn new(name: &str) -> Self {
        Self {
            state: Mutex::new(State::default()),
            name: name.to_string(),
        }
    }

    pub(crate) fn reset(&self, obj: &glib::Object) {
        let mut state = self.state.lock().unwrap();

        state.reset(obj);
        gst::debug!(CAT, obj: obj, "Setting {} seek_info: {:?}", self.name, state.seek_info);
    }

    pub(crate) fn has_eos_sample(&self) -> bool {
        let res = matches!(
            self.state.lock().unwrap().seek_info,
            SeekInfo::PreviousSeekDone(_, _)
        );

        if res {
            gst::debug!(CAT, "{} has eos sample", self.name);
        }

        res
    }

    fn get_sample_start_end_stream_time(
        &self,
        sample: &gst::Sample,
        obj: &super::UriDecodePoolSrc,
    ) -> Result<(bool, gst::ClockTime, gst::ClockTime), gst::FlowError> {
        if let (Some(buffer), Some(Ok(segment))) = (
            sample.buffer(),
            sample
                .segment()
                .map(|s| s.clone().downcast::<gst::format::Time>()),
        ) {
            let framerate = sample.caps().and_then(|caps| {
                caps.structure(0)
                    .and_then(|s| s.get::<gst::Fraction>("framerate").ok())
            });

            let start = if let Some(pts) = buffer.pts() {
                pts
            } else {
                gst::error!(CAT, obj: obj, "Decoded buffer without timestamp");

                return Err(gst::FlowError::Error);
            };

            let stop = start
                + buffer.duration().unwrap_or_else(|| {
                    framerate.map_or(gst::ClockTime::ZERO, |framerate| {
                        if framerate.numer() > 0 {
                            gst::ClockTime::from_nseconds(
                                gst::ClockTime::SECOND.nseconds() * framerate.denom() as u64
                                    / framerate.numer() as u64,
                            )
                        } else {
                            gst::ClockTime::ZERO
                        }
                    })
                });

            let (start, stop) = if let Some((start, stop)) = segment.clip(start, stop) {
                (start, stop)
            } else {
                return Ok((false, gst::ClockTime::ZERO, gst::ClockTime::ZERO));
            };

            Ok((
                true,
                segment.to_stream_time(start).expect(
                    "Start has been clipped to the segment, it should have a valid stream time",
                ),
                segment.to_stream_time(stop).expect(
                    "Stop has been clipped to the segment, it should have a valid stream time",
                ),
            ))
        } else {
            gst::error!(CAT, obj:  obj, "Sample without buffer or segment");

            Err(gst::FlowError::Error)
        }
    }

    fn check_eos(
        &self,
        obj: &super::UriDecodePoolSrc,
        sample: &gst::Sample,
    ) -> Result<(), gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let seek_segment = if let SeekInfo::SeekSegment(_, ref seek_segment) = state.seek_info {
            seek_segment.downcast_ref::<gst::format::Time>().unwrap()
        } else {
            gst::log!(CAT, obj: obj, "No seek segment {:?}", state.seek_info);
            return Ok(());
        };

        let (clipping_succeeded, start, stop) =
            self.get_sample_start_end_stream_time(sample, obj)?;
        if !clipping_succeeded {
            return Ok(());
        }
        // This logic follows the implementation of gst::Segment::clip
        // Buffer has a duration != 0 and its stop is right at the beginning of the segment
        let buffer_ends_at_start_of_segment = seek_segment.start().is_some()
            && start != stop
            && stop == seek_segment.start().unwrap();
        // Segment has a duratrion != 0 and the buffer starts at the end of the segment
        let buffer_starts_at_end_of_segment = seek_segment.stop().is_some()
            && seek_segment.start() != seek_segment.stop()
            && Some(start) == seek_segment.stop();

        let check_needs_at_least_one_buffer = || {
            if state.pushed_buffer_in_segment {
                false
            } else if let Some(pipeline) = obj.pipeline() {
                if pipeline
                    .iterate_all_by_element_factory_name("segmentclipper")
                    .next()
                    .is_ok()
                {
                    gst::info!(CAT, obj: obj, "Got a segmentclipper in underlying pipeline and \
                        got no buffer before EOS. Use the same logic as segmentclipper in that case \
                        to handle input streams with 'random/big gaps'");

                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if seek_segment.rate() > 0.0 {
            // Forward playback
            if Some(stop) < seek_segment.start() || buffer_ends_at_start_of_segment {
                return Ok(());
            } else if seek_segment.stop().is_some() && Some(start) >= seek_segment.stop()
                || buffer_starts_at_end_of_segment
            {
                if check_needs_at_least_one_buffer() {
                    return Ok(());
                }

                gst::info!(CAT, obj: obj, "Buffer reached end of segment \n{seek_segment:#?} \n {sample:#?} \n");
                let seek_segment = seek_segment.clone().upcast();
                state.set_seek_info(SeekInfo::PreviousSeekDone(
                    sample.clone(),
                    Some(seek_segment),
                ));
                gst::debug!(CAT, obj: obj, "[inpoint={:?} - duration={:?}] - Setting seek_info: {:#?}",
                    obj.inpoint(), obj.duration(), state.seek_info,);
                state.handled_composition_seek = false;
                state.nle_seek = None;
                gst::info!(CAT, obj: obj, "Faking EOS");
                return Err(gst::FlowError::Eos);
            }
        } else {
            // Reverse playback
            if start
                > seek_segment
                    .stop()
                    .expect("Can't have a NONE segment.stop in reverse playback")
                || buffer_starts_at_end_of_segment
            {
                return Ok(());
            } else if stop
                <= seek_segment
                    .start()
                    .expect("Can't have a NONE segment.start in reverse playback")
                || buffer_starts_at_end_of_segment
            {
                if check_needs_at_least_one_buffer() {
                    return Ok(());
                }

                gst::info!(CAT, obj: obj, "Buffer reached end of segment {seek_segment:?}");
                let seek_segment = seek_segment.clone().upcast();
                state.set_seek_info(SeekInfo::PreviousSeekDone(
                    sample.clone(),
                    Some(seek_segment),
                ));
                gst::debug!(CAT, obj: obj, "[inpoint={:?} - duration={:?}] - Setting seek_info: {:#?}",
                    obj.inpoint(), obj.duration(), state.seek_info,);
                state.handled_composition_seek = false;
                state.nle_seek = None;
                gst::info!(CAT, obj: obj, "Faking EOS");
                return Err(gst::FlowError::Eos);
            }
        }

        Ok(())
    }

    pub(crate) fn get_eos_sample(
        &self,
        obj: &super::UriDecodePoolSrc,
    ) -> Result<Option<gst::Sample>, gst::FlowError> {
        let state = self.state.lock().unwrap();
        if state.nle_seek.as_ref().is_some() && !state.handled_composition_seek {
            gst::debug!(CAT, obj: obj, "Not checking state because waiting for nleseek to be handled {} seek_info: {:?} - returning EOS!", self.name, state.seek_info);

            return Err(gst::FlowError::Eos);
        }

        if let SeekInfo::PreviousSeekDone(ref sample, _) = state.seek_info {
            gst::info!(CAT, obj: obj, "Got EOS sample: {:?}", sample);
            Ok(Some(sample.clone()))
        } else {
            gst::log!(CAT, obj: obj, "No EOS sample");
            Ok(None)
        }
    }

    fn remap_segment(
        &self,
        obj: &super::UriDecodePoolSrc,
        seqnum: &gst::Seqnum,
        seek_segment: &gst::Segment,
        sample_segment: Option<gst::Segment>,
    ) -> Result<SeekInfo, (gst::FlowError, Option<gst::Seqnum>)> {
        let sample_segment = if let Some(sample_segment) = sample_segment {
            sample_segment.downcast::<gst::ClockTime>().map_err(|err| {
                gst::error!(CAT, obj: obj, "Invalid sample segment format: ({err:?}");
                (gst::FlowError::Error, None)
            })?
        } else {
            gst::warning!(CAT, obj: obj, "No segment on the sample??");
            return Ok(SeekInfo::SeekSegment(*seqnum, seek_segment.clone()));
        };

        if let Some(last_remapped_segment) =
            self.state.lock().unwrap().last_remapped_segment.as_ref()
        {
            if last_remapped_segment.sample_segment == sample_segment {
                return Ok(SeekInfo::SeekSegment(
                    *seqnum,
                    last_remapped_segment.remapped_segment.clone(),
                ));
            }
        }

        let seek_segment_time = seek_segment
            .downcast_ref::<gst::ClockTime>()
            .expect("Seek segment should have been checked already!");

        if seek_segment_time == &sample_segment {
            return Ok(SeekInfo::SeekSegment(*seqnum, seek_segment.clone()));
        }

        assert_eq!(
            seek_segment_time.rate(),
            sample_segment.rate(),
            "Underlying pipelines should produce the same rate as expected seek!"
        );

        let start_target = gst::Signed::Positive(obj.inpoint().expect("We can't have None duration here as we are in the case where we have a valid nle seek segment!"));
        let duration_target = gst::Signed::Positive(obj.duration().expect("We can't have None duration here as we are in the case where we have a valid nle seek segment!"));
        let seek_segment_start = gst::Signed::Positive(seek_segment_time.start().unwrap());
        let seek_segment_stop = gst::Signed::Positive(seek_segment_time.stop().unwrap());

        let start_diff = start_target - seek_segment_start;
        let stop_diff = start_target + duration_target - seek_segment_stop;

        let sample_start = gst::Signed::Positive(sample_segment.start().unwrap());
        let sample_stop = gst::Signed::Positive(sample_segment.stop().unwrap());

        let new_start = match sample_start - start_diff {
            gst::Signed::Positive(new_start) => new_start,
            gst::Signed::Negative(_) => sample_start.positive().unwrap(),
        };
        let new_stop = match sample_stop - stop_diff {
            gst::Signed::Positive(new_stop) => new_stop,
            gst::Signed::Negative(_) => sample_stop.positive().unwrap(),
        };

        let mut segment = seek_segment.clone();
        segment.set_start(new_start);
        segment.set_stop(new_stop);

        gst::info!(CAT, obj: obj, "sample segment: {sample_segment:#?} - seek_segment {seek_segment:#?} -> remapped_segment: {segment:#?}");
        self.state.lock().unwrap().last_remapped_segment = Some(RemappedSegment {
            sample_segment,
            remapped_segment: segment.clone(),
        });

        Ok(SeekInfo::SeekSegment(*seqnum, segment))
    }

    pub(crate) fn process(
        &self,
        obj: &super::UriDecodePoolSrc,
        sample: &gst::Sample,
    ) -> Result<SeekInfo, (gst::FlowError, Option<gst::Seqnum>)> {
        let mut state = self.state.lock().unwrap();

        gst::log!(CAT, obj: obj, "nle_seek: {:?} -- handled? {:?}", state.nle_seek, state.handled_composition_seek);
        if state.nle_seek.is_some() && !state.handled_composition_seek {
            gst::error!(CAT, obj: obj, "This should no happen anymore.");
            state.set_seek_info(SeekInfo::PreviousSeekDone(
                sample.clone(),
                sample.segment().cloned(),
            ));
            drop(state);

            gst::info!(CAT, obj: obj, "Force unblocking the nlecompositon by sending EOS, keeping sample around");
            if let Some(caps) = sample.caps() {
                gst::log!(CAT, obj: obj, "Pushing caps {:?}", caps);
                if let Err(e) = obj.imp().set_caps(caps.to_owned()) {
                    gst::error!(CAT, obj: obj, "Failed to push caps: {:?}", e);
                }
            }

            gst::info!(CAT, obj: obj, "Faking EOS before starting");
            return Err((gst::FlowError::Eos, None));
        }

        if matches!(state.seek_info, SeekInfo::PreviousSeekDone(_, _)) {
            let seek_info = if let Some(nle_seek) = state.nle_seek.as_ref() {
                let (rate, flags, start_type, start, stop_type, stop) =
                    if let gst::EventView::Seek(s) = nle_seek.view() {
                        s.get()
                    } else {
                        unreachable!();
                    };
                let mut segment = gst::FormattedSegment::<gst::ClockTime>::new().upcast();
                segment.do_seek(rate, flags, start_type, start, stop_type, stop);

                SeekInfo::SeekSegment(nle_seek.seqnum(), segment)
            } else {
                SeekInfo::None
            };
            state.set_seek_info(seek_info);
        }

        let mut res = state.seek_info.clone();
        let seqnum = state.nle_seek.as_ref().map(|s| s.seqnum());
        drop(state);

        if let Err(gst::FlowError::Eos) = self.check_eos(obj, sample) {
            return Err((gst::FlowError::Eos, seqnum));
        }

        if let SeekInfo::SeekSegment(seqnum, segment) = &res {
            res = self.remap_segment(obj, seqnum, segment, sample.segment().cloned())?;
            self.state.lock().unwrap().pushed_buffer_in_segment = true;
        }
        Ok(res)
    }

    pub(crate) fn handle_nlecomposition_seek(
        &self,
        obj: &super::UriDecodePoolSrc,
        seek: &gst::Event,
    ) -> NleCompositionSeekResult {
        let mut state = self.state.lock().unwrap();
        state.nle_seek = None;
        gst::debug!(CAT, obj: obj, "nlecomposition-seek: {:?}", seek);
        let (rate, _flags, start_type, start, stop_type, stop) =
            if let gst::EventView::Seek(s) = seek.view() {
                s.get()
            } else {
                unreachable!();
            };

        let (seek_start, seek_stop) = match (start, stop) {
            (gst::GenericFormattedValue::Time(start), gst::GenericFormattedValue::Time(stop)) => {
                (start, stop)
            }
            _ => {
                gst::error!(CAT, obj: obj, "Seeked with wrong format {seek:?}");
                return NleCompositionSeekResult::Unexpected;
            }
        };

        if rate.abs() != 1.0 {
            gst::info!(CAT, obj: obj, "Seeked with abs(rate) != 1.0, not using default segment");

            return NleCompositionSeekResult::Unexpected;
        }

        if start_type != gst::SeekType::Set || stop_type != gst::SeekType::Set {
            gst::info!(CAT, obj: obj, "Seek type not supported, start type:{start_type:?} stop type:{stop_type:?}");

            return NleCompositionSeekResult::Unexpected;
        }

        if obj.reverse() {
            if rate > 0.0 {
                gst::info!(CAT, obj: obj, "Reverse playack but got a forward seek, not using default segment");
                return NleCompositionSeekResult::Unexpected;
            }
        } else if rate < 0.0 {
            gst::info!(CAT, obj: obj, "Forward playback but got a reverse seek, not using default segment");
            return NleCompositionSeekResult::Unexpected;
        }

        let duration = obj.duration();
        if duration.is_none() {
            if matches!(state.seek_info, SeekInfo::None) {
                gst::fixme!(CAT, obj: obj, "This assume NLE is used **through** GES \
                            and GES is responsible for sending the 'intial-seek' and \
                            does not send it in that case because our underlying \
                            pipeline is a nested timeline");
                let seqnum = seek.seqnum();

                gst::info!(CAT, obj: obj, "Force using seqnum {seqnum:?}");
                return NleCompositionSeekResult::UseSeqnum(seqnum);
            }

            gst::info!(CAT, obj: obj, "We had no initial seek and an unexpected seek");
            return NleCompositionSeekResult::Unexpected;
        }

        if let SeekInfo::PreviousSeekDone(_, ref segment) = state.seek_info {
            let segment = segment
                .as_ref()
                .expect("Only EOS to unblock composition can have an None segment")
                .downcast_ref::<gst::format::Time>()
                .unwrap();
            if obj.reverse() {
                if seek_stop != segment.start() {
                    gst::info!(CAT, obj: obj, "Reverse playback but start != previous start, not using default segment");
                    state.set_seek_info(SeekInfo::None);
                    return NleCompositionSeekResult::Unexpected;
                }
            } else if seek_start != segment.stop() {
                gst::info!(CAT, obj: obj, "Forward playback but start != previous stop, not using default segment");
                state.set_seek_info(SeekInfo::None);
                return NleCompositionSeekResult::Unexpected;
            }
        } else {
            let (inpoint, outpoint) = (
                obj.inpoint().unwrap_or(gst::ClockTime::ZERO),
                obj.inpoint().unwrap_or(gst::ClockTime::ZERO) + duration.expect("Checked before"),
            );
            if obj.reverse() {
                if seek_stop != Some(outpoint) {
                    gst::info!(CAT, obj: obj, "Reverse playback but stop != inpoint + duration, not using default segment");
                    return NleCompositionSeekResult::Unexpected;
                }

                if seek_stop > Some(inpoint) {
                    gst::info!(CAT, obj: obj, "Reverse playback but start > inpoint, not using default segment");
                    return NleCompositionSeekResult::Unexpected;
                }
            } else if seek_start != Some(inpoint) {
                gst::info!(CAT, obj: obj, "seek_start({seek_start:?}) != inpoint({inpoint:?}), not using default segment");
                return NleCompositionSeekResult::Unexpected;
            }

            gst::info!(CAT, obj: obj, "{} seek_start({seek_start:?}) = inpoint({inpoint:?}), USING default segment",
                obj.imp().decoderpipe().unwrap().name());
        }
        state.nle_seek = Some(seek.clone());

        return NleCompositionSeekResult::Expected;
    }

    pub(crate) fn handle_seek(
        &self,
        obj: &super::UriDecodePoolSrc,
        seek: &gst::event::Seek,
    ) -> bool {
        let seek_event = seek.event().to_owned();
        let mut state = self.state.lock().unwrap();
        let nle_seek = state.nle_seek.clone();

        if nle_seek.is_none() || state.handled_composition_seek {
            gst::info!(CAT, obj: obj, "Not expecting any NLE seek, forward: {:?}", seek);
            state.set_seek_info(SeekInfo::None);
            return false;
        }

        if seek_event.seqnum() != nle_seek.as_ref().unwrap().seqnum() {
            gst::info!(CAT, obj: obj, "Not the expected NLE seek??");
            gst::info!(CAT, obj: obj, "expected: {:?} != {:?}",
                nle_seek.as_ref().map(|s| s.seqnum()),
                seek_event.seqnum());
            state.nle_seek = None;
            state.set_seek_info(SeekInfo::None);
            state.handled_composition_seek = false;
            return false;
        }

        state.handled_composition_seek = true;
        let (rate, flags, start_type, start, stop_type, stop) =
            if let gst::EventView::Seek(s) = seek_event.view() {
                s.get()
            } else {
                unreachable!();
            };
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new().upcast();
        segment.do_seek(rate, flags, start_type, start, stop_type, stop);
        if !matches!(state.seek_info, SeekInfo::PreviousSeekDone(_, _)) {
            state.set_seek_info(SeekInfo::SeekSegment(seek_event.seqnum(), segment));
        }
        gst::debug!(CAT, obj: obj, "Setting {} seek_info: {:?}", self.name, state.seek_info);
        drop(state);

        true
    }

    fn handle_flush_event_probe(
        &self,
        obj: &super::UriDecodePoolSrc,
        probe_info: &gst::PadProbeInfo,
        seek_event: &gst::Event,
    ) -> gst::PadProbeReturn {
        if let Some(gst::PadProbeData::Event(ref event)) = probe_info.data {
            if let gst::EventView::FlushStop(flush) = event.view() {
                let mut state = self.state.lock().unwrap();
                if flush.seqnum() == seek_event.seqnum() {
                    gst::log!(CAT, obj: obj, "forwarded {} seek {:?}", self.name, state.seek_info);
                    state.handled_composition_seek = true;

                    let (rate, flags, start_type, start, stop_type, stop) =
                        if let gst::EventView::Seek(s) = seek_event.view() {
                            s.get()
                        } else {
                            unreachable!();
                        };
                    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new().upcast();
                    segment.do_seek(rate, flags, start_type, start, stop_type, stop);

                    if !matches!(state.seek_info, SeekInfo::PreviousSeekDone(_, _)) {
                        state.set_seek_info(SeekInfo::SeekSegment(seek_event.seqnum(), segment));
                    }
                    gst::debug!(CAT, obj: obj, "Setting {} seek_info: {:?}", self.name, state.seek_info);
                } else {
                    gst::info!(
                        CAT,
                        obj: obj,
                        "Dropping NLE seek info after flushing - expected {:?}, got {:?}",
                        seek_event.seqnum(),
                        flush.seqnum()
                    );
                    state.handled_composition_seek = false;
                    state.nle_seek = None;
                    state.set_seek_info(SeekInfo::None);
                }
            }
        }

        gst::PadProbeReturn::Ok
    }

    pub(crate) fn handle_sample(&self, sample: &gst::Sample) -> Option<gst::Buffer> {
        if let Some(buffer) = sample.buffer_owned() {
            let mut state = self.state.lock().unwrap();
            if let Some(segment) = sample.segment() {
                state.stream_time = if let gst::GenericFormattedValue::Time(stream_time) =
                    segment.to_stream_time(buffer.pts())
                {
                    stream_time
                } else {
                    unreachable!()
                };
            }

            Some(buffer)
        } else {
            None
        }
    }

    pub fn should_send_seek(&self, event: &gst::Event, decoderpipeline: &DecoderPipeline) -> bool {
        if let Some(structure) = event.structure() {
            if !structure
                .get::<bool>("nlecomposition-seek")
                .map_or(false, |v| v)
            {
                gst::debug!(CAT, obj: decoderpipeline, "Not a composition seek event");
                return true;
            }
        } else {
            return true;
        }

        let mut i = 0;
        let pad = decoderpipeline
            .imp()
            .sink()
            .sink_pads()
            .first()
            .unwrap()
            .clone();
        while let Some(event) = pad.sticky_event::<gst::event::Tag>(i) {
            if let gst::EventView::Tag(tag) = event.view() {
                // Do not send initialization seek to sub timelines!
                if let Some(is_ges_timeline) = tag.tag().generic("is-ges-timeline") {
                    if is_ges_timeline.get::<bool>().unwrap() {
                        gst::info!(CAT, obj: decoderpipeline, "Tag with is-ges-timeline, not sending init seek!");
                        return false;
                    }
                }
            }
            i += 1;
        }

        gst::info!(CAT, obj: decoderpipeline, "Sending initialization seek");

        true
    }
}
