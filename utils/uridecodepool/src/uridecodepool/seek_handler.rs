use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use std::sync::{Condvar, Mutex, MutexGuard};

use crate::uridecodepool::imp::CAT;

#[derive(Debug, Default)]
struct NleCompositionSeekData {
    // Wether the seek handler is started
    started: bool,
    expect_nle_seek: bool,
    // Wether the seek handler is waiting for a seek to be sent by a parent nlecomposition
    waiting: bool,
    seek: Option<gst::event::Seek<gst::Event>>,
}

#[derive(Debug)]
pub(crate) struct SeekHandler {
    // Lock order is state -> nlecomposition_seek_data
    state: Mutex<State>,

    // nlecomposition_seek_data is used to signal that we are waiting for a seek to be sent by
    // a parent nlecomposition element.
    // Cases:
    // - We expected `nlecomposition` to send a seek_
    //   - The seek doesn't match our expectations, ignore it
    //   - The seek matches our expectations, use it
    // - We didn't expect a seek
    // - The associated source is stopped
    nlecomposition_seek_data: Mutex<NleCompositionSeekData>,
    waiting_nlecomposition_seek_cond: Condvar,
    consumed_nlecomposition_seek_cond: Condvar,
}

#[derive(Debug, Default)]
struct State {
    stream_time: Option<gst::ClockTime>,
    seek_info: SeekInfo,
}

impl State {
    fn reset(&mut self) {
        self.stream_time = None;
        self.seek_info = SeekInfo::None;
    }
}

impl Default for SeekHandler {
    fn default() -> Self {
        Self {
            nlecomposition_seek_data: Mutex::new(NleCompositionSeekData::default()),
            consumed_nlecomposition_seek_cond: Condvar::new(),
            waiting_nlecomposition_seek_cond: Condvar::new(),

            state: Mutex::new(State::default()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) enum SeekInfo {
    #[default]
    None,
    SeekSegment(gst::Seqnum, gst::Segment),
    PreviousSeekDone(gst::Sample, gst::Segment),
}

impl SeekHandler {
    // Full means that we should also reset all ibform
    fn flush_locked(&self, state: &MutexGuard<State>, stop: bool) {
        let mut nlecomposition_seek_data = self.nlecomposition_seek_data.lock().unwrap();

        if stop {
            nlecomposition_seek_data.started = false;
        }
        let _ = nlecomposition_seek_data.seek.take();
        gst::debug!(CAT, "Flushing, notifying_all");
        self.waiting_nlecomposition_seek_cond.notify_all();
        self.consumed_nlecomposition_seek_cond.notify_all();

        nlecomposition_seek_data.expect_nle_seek =
            matches!(state.seek_info, SeekInfo::PreviousSeekDone(_, _));
        gst::debug!(
            CAT,
            "Reseting seek handler - waiting seek? {:?}",
            state.seek_info
        );
    }

    pub(crate) fn start(&self) {
        self.nlecomposition_seek_data.lock().unwrap().started = true;
    }

    pub(crate) fn stop(&self, obj: &super::UriDecodePoolSrc) {
        gst::debug!(CAT, obj: obj, "STOPPING SEEK HANDLER");
        let state = self.state.lock().unwrap();
        self.flush_locked(&state, true);
    }

    pub(crate) fn reset(&self) {
        let mut state = self.state.lock().unwrap();
        self.flush_locked(&state, false);

        state.reset();
    }

    pub(crate) fn has_eos_sample(&self) -> bool {
        let res = matches!(
            self.state.lock().unwrap().seek_info,
            SeekInfo::PreviousSeekDone(_, _)
        );

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
    ) -> Result<bool, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let seek_segment = if let SeekInfo::SeekSegment(_, ref seek_segment) = state.seek_info {
            seek_segment.downcast_ref::<gst::format::Time>().unwrap()
        } else {
            gst::log!(CAT, obj: obj, "No seek segment {:?}", state.seek_info);
            return Ok(false);
        };

        let (clipping_succeeded, start, stop) =
            self.get_sample_start_end_stream_time(sample, obj)?;
        if !clipping_succeeded {
            return Ok(false);
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

        if seek_segment.rate() > 0.0 {
            // Forward playback
            if Some(stop) < seek_segment.start() || buffer_ends_at_start_of_segment {
                return Ok(false);
            } else if seek_segment.stop().is_some() && Some(start) >= seek_segment.stop()
                || buffer_starts_at_end_of_segment
            {
                self.nlecomposition_seek_data
                    .lock()
                    .unwrap()
                    .expect_nle_seek = true;
                gst::error!(CAT, obj: obj, "||||||||||||<<<<<<<<<<<<<<<<<<<<<<<<<<<<< Buffer reached end of segment {seek_segment:?} -> {:?}", *self.nlecomposition_seek_data.lock().unwrap());
                gst::error!(CAT, "WAITING SEEK IS TRUE");
                state.seek_info =
                    SeekInfo::PreviousSeekDone(sample.clone(), seek_segment.clone().upcast());
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
                return Ok(false);
            } else if stop
                <= seek_segment
                    .start()
                    .expect("Can't have a NONE segment.start in reverse playback")
                || buffer_starts_at_end_of_segment
            {
                self.nlecomposition_seek_data
                    .lock()
                    .unwrap()
                    .expect_nle_seek = true;
                gst::error!(CAT, obj: obj, "+++++++++++++++++>>>>>>>>>>>> Buffer reached end of segment");
                state.seek_info =
                    SeekInfo::PreviousSeekDone(sample.clone(), seek_segment.clone().upcast());
                return Err(gst::FlowError::Eos);
            }
        }

        Ok(false)
    }

    pub(crate) fn get_eos_sample(&self) -> Option<gst::Sample> {
        let state = self.state.lock().unwrap();
        if let SeekInfo::PreviousSeekDone(ref sample, _) = state.seek_info {
            Some(sample.clone())
        } else {
            None
        }
    }

    pub(crate) fn process(
        &self,
        obj: &super::UriDecodePoolSrc,
        sample: &gst::Sample,
    ) -> Result<SeekInfo, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let expecting_new_stack = matches!(state.seek_info, SeekInfo::PreviousSeekDone(_, _));
        let duration = obj.duration();
        if state.stream_time.is_some() && !expecting_new_stack || duration.is_none() {
            let res = state.seek_info.clone();
            drop(state);

            self.check_eos(obj, sample)?;

            return Ok(res);
        }
        drop(state);

        let return_seek_info = |state: Option<MutexGuard<State>>, info: SeekInfo| {
            state.map_or_else(
                || self.state.lock().unwrap().seek_info = info.clone(),
                |mut state| state.seek_info = info.clone(),
            );

            gst::error!(CAT, obj: obj, "WAITING SEEK IS FALSE");
            self.consumed_nlecomposition_seek_cond.notify_all();

            gst::error!(CAT, obj: obj, "====> Returning seek info: {info:?}");

            Ok(info)
        };

        let mut custom_query = gst::query::Custom::new(
            gst::Structure::builder("nlecomposition-initialization-seek")
                .field("nle-initialization-seek", None::<gst::Event>)
                .build(),
        );
        gst::error!(CAT, obj: obj, "Checking for nle seek event||");
        obj.src_pad().peer_query(custom_query.query_mut());

        let mut nlecomposition_seek_data = self.nlecomposition_seek_data.lock().unwrap();
        nlecomposition_seek_data.expect_nle_seek = true;
        if !expecting_new_stack
            && custom_query
                .structure()
                .unwrap()
                .get::<Option<gst::Event>>("nlecomposition-initialization-seek")
                .unwrap_or(None::<gst::Event>)
                .is_none()
        {
            nlecomposition_seek_data.expect_nle_seek = false;
            drop(nlecomposition_seek_data);
            gst::error!(CAT, obj: obj, "Nle will not seek, not using default segment");

            return return_seek_info(None, SeekInfo::None);
        }

        gst::error!(CAT, obj: obj, "Nle will seek, waiting for seek event");

        nlecomposition_seek_data.waiting = true;
        while nlecomposition_seek_data.expect_nle_seek
            && nlecomposition_seek_data.started
            && nlecomposition_seek_data.seek.is_none()
        {
            gst::error!(CAT, obj: obj, "Waiting for nle seek event");
            nlecomposition_seek_data = self
                .waiting_nlecomposition_seek_cond
                .wait(nlecomposition_seek_data)
                .unwrap();
            gst::error!(CAT, obj: obj, "Done waiting? expected_nle_seek={:?} - started={:?}", nlecomposition_seek_data.expect_nle_seek, nlecomposition_seek_data.started);
        }
        nlecomposition_seek_data.waiting = false;

        let seek = if let Some(seek) = nlecomposition_seek_data.seek.take() {
            seek
        } else {
            drop(nlecomposition_seek_data);

            gst::error!(CAT, obj: obj, "No seek event received, flushing?");
            return return_seek_info(None, SeekInfo::None);
        };
        drop(nlecomposition_seek_data);

        let (rate, flags, start_type, start, stop_type, stop) = seek.get();
        let (seek_start, seek_stop) = match (start, stop) {
            (gst::GenericFormattedValue::Time(start), gst::GenericFormattedValue::Time(stop)) => {
                (start, stop)
            }
            _ => {
                gst::error!(CAT, obj: obj, "Seeked with wrong format {seek:?}");
                return Err(gst::FlowError::Error);
            }
        };

        if rate.abs() != 1.0 {
            gst::error!(CAT, obj: obj, "Seeked with abs(rate) != 1.0, not using default segment");

            return return_seek_info(None, SeekInfo::None);
        }

        if start_type != gst::SeekType::Set || stop_type != gst::SeekType::Set {
            gst::error!(CAT, obj: obj, "Seek type not supported, start type:{start_type:?} stop type:{stop_type:?}");

            return return_seek_info(None, SeekInfo::None);
        }

        if obj.reverse() {
            if rate > 0.0 {
                gst::error!(CAT, obj: obj, "Reverse playack but got a forward seek, not using default segment");
                return return_seek_info(None, SeekInfo::None);
            }
        } else if rate < 0.0 {
            gst::error!(CAT, obj: obj, "Forward playback but got a reverse seek, not using default segment");
            return return_seek_info(None, SeekInfo::None);
        }

        let state = self.state.lock().unwrap();
        if let SeekInfo::PreviousSeekDone(_, ref segment) = state.seek_info {
            let segment = segment.downcast_ref::<gst::format::Time>().unwrap();
            if obj.reverse() {
                if seek_stop != segment.start() {
                    gst::error!(CAT, obj: obj, "Reverse playback but start != previous start, not using default segment");
                    return return_seek_info(Some(state), SeekInfo::None);
                }
            } else if seek_start != segment.stop() {
                gst::error!(CAT, obj: obj, "Forward playback but start != previous stop, not using default segment");
                return return_seek_info(Some(state), SeekInfo::None);
            }
        } else {
            let (current_start, current_stop) = (
                obj.inpoint().unwrap_or(gst::ClockTime::ZERO),
                obj.inpoint().unwrap_or(gst::ClockTime::ZERO) + duration.expect("Checked before"),
            );
            if obj.reverse() {
                if seek_stop != Some(current_stop) {
                    gst::error!(CAT, obj: obj, "Reverse playback but stop != inpoint + duration, not using default segment");
                    return return_seek_info(Some(state), SeekInfo::None);
                }

                if seek_stop > Some(current_start) {
                    gst::error!(CAT, obj: obj, "Reverse playback but start > inpoint, not using default segment");
                    return return_seek_info(Some(state), SeekInfo::None);
                }
            } else if seek_start != Some(current_start) {
                gst::error!(CAT, obj: obj, "seek_start({seek_start:?}) != current_start({current_start:?}), not using default segment");
                return return_seek_info(Some(state), SeekInfo::None);
            }
        }
        drop(state);

        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new().upcast();
        segment.do_seek(rate, flags, start_type, seek_start, stop_type, seek_stop);

        gst::error!(CAT, obj: obj, "Sending seek to baseclass {:?}", seek.event());
        obj.imp().send_seek(seek.event().to_owned());
        gst::error!(CAT, obj: obj, "====> Sending segment {segment:?}");

        return_seek_info(None, SeekInfo::SeekSegment(seek.seqnum(), segment))
    }

    pub(crate) fn handle_seek(
        &self,
        obj: &super::UriDecodePoolSrc,
        seek: &gst::event::Seek,
    ) -> bool {
        gst::error!(CAT, obj: obj, "I HAVE BEEN BUILT!! ====> Handling seek {seek:#?}");

        let (rate, flags, start_type, start, stop_type, stop) = seek.get();
        let mut nlecomposition_seek_data = self.nlecomposition_seek_data.lock().unwrap();
        if nlecomposition_seek_data.expect_nle_seek {
            gst::error!(CAT, "Was maybe expecting a seek");
            if let SeekInfo::PreviousSeekDone(_, ref segment) = self.state.lock().unwrap().seek_info
            {
                gst::error!(CAT, "Previous segment {:#?} - seek {:#?}", segment, seek);
                if rate != if obj.reverse() { -1.0 } else { 1.0 } {
                    nlecomposition_seek_data.expect_nle_seek = false;
                }

                if start_type != gst::SeekType::Set || stop_type != gst::SeekType::Set {
                    nlecomposition_seek_data.expect_nle_seek = false;
                }

                if rate >= 1.0 && start != segment.stop() || rate <= -1.0 && stop != segment.start()
                {
                    nlecomposition_seek_data.expect_nle_seek = false;
                }
            }

            if !nlecomposition_seek_data.expect_nle_seek {
                gst::error!(CAT, obj: obj, "Expected seek doesn't match received one: expected_nle_seek={:?} - resetting seek_info", nlecomposition_seek_data.expect_nle_seek);
                self.state.lock().unwrap().seek_info = SeekInfo::None;
                self.waiting_nlecomposition_seek_cond.notify_all();
            }
        }

        if !nlecomposition_seek_data.expect_nle_seek {
            // In case of invalid seeks
            drop(nlecomposition_seek_data);

            gst::error!(CAT, obj: obj, "====> Not expecting nlecomposition seek, let source handle the seek  'normally' and reset");
            if flags.contains(gst::SeekFlags::FLUSH) {
                gst::error!(CAT, obj: obj, "====> Flushing seek, reseting");
                self.reset();
            }
            return false;
        }
        gst::error!(CAT, "====> Waiting for seek, sending seek to the waiter.");

        nlecomposition_seek_data.seek = Some(seek.to_owned());
        nlecomposition_seek_data.expect_nle_seek = false;
        self.waiting_nlecomposition_seek_cond.notify_all();

        gst::log!(CAT, obj: obj, "Waiting seek to be consumed");
        while nlecomposition_seek_data.seek.is_some() && nlecomposition_seek_data.started {
            nlecomposition_seek_data = self
                .consumed_nlecomposition_seek_cond
                .wait(nlecomposition_seek_data)
                .unwrap();
        }
        drop(nlecomposition_seek_data);

        let state = self.state.lock().unwrap();
        gst::error!(CAT, obj: obj, "Done waiting for seek to be consumed, current seek_info={:?}", state.seek_info);

        !matches!(&state.seek_info, SeekInfo::None)
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
}
