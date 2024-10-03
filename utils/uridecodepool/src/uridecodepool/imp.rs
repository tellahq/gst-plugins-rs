// SPDX-License-Identifier: MPL-2.0

use crate::uridecodepool::seek_handler::{self, SeekInfo};
use futures::prelude::*;
use gst::glib::translate::ToGlibPtr;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

use gst::glib;
use gst::glib::Properties;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::{prelude::*, subclass::prelude::*};
use once_cell::sync::Lazy;

use super::{
    pool::{self, RUNTIME},
    DecoderPipeline,
};

#[derive(Debug)]
struct Settings {
    uri: Option<String>,
    caps: gst::Caps,
    stream_id: Option<String>,

    inpoint: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
    reverse: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            uri: None,
            caps: gst::Caps::builder_full()
                .structure_with_any_features(gst::Structure::new_empty("video/x-raw"))
                .build(),
            stream_id: None,
            inpoint: None,
            duration: None,
            reverse: false,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    decoderpipe: Option<DecoderPipeline>,
    bus_message_sigid: Option<glib::SignalHandlerId>,
    source_setup_sigid: Option<glib::SignalHandlerId>,

    current_segment: Option<gst::FormattedSegment<gst::ClockTime>>,

    // Seek event being processed
    seek_event: Option<gst::Event>,
    // Seek to be processed as soon as underlying pipeline is ready
    pending_seek: Option<gst::Event>,
    flushing: bool,
    seek_seqnum: Option<gst::Seqnum>,
    segment_seqnum: Option<gst::Seqnum>,
    needs_segment: bool,
    seek_segment: Option<gst::Segment>,
    ignore_seek: bool,
}

#[derive(Properties, Debug)]
#[properties(wrapper_type = super::UriDecodePoolSrc)]
pub struct UriDecodePoolSrc {
    #[property(name="uri", get, set, type = Option<String>, member = uri, blurb = "The URI to play")]
    #[property(name = "caps", get, set, type = gst::Caps, member = caps,
        blurb = "The caps of the stream to target"
    )]
    #[property(name = "stream-id", get, set, type = Option<String>, member = stream_id,
        flags = glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
        blurb = "The stream-id of the stream to be used"
    )]
    #[property(name="inpoint", get, set, type = Option<gst::ClockTime>, member = inpoint, blurb = "The inpoint to seek to")]
    #[property(name="duration", get, set, type = Option<gst::ClockTime>, member = duration, blurb = "The duration to play")]
    #[property(name="reverse", get, set, type = bool, member = reverse, blurb = "Play in reverse direction")]
    settings: Mutex<Settings>,

    /// This is a hack to be able to generate a dot file of the underlying pipeline
    /// when this element is being dumped. It respects the GST_DEBUG_DUMP_DOT_DIR
    /// environment variable and the file will be generated in that directory.
    #[property(name="pipeline-dot",
        get = Self::dot_pipeline,
        type = Option<String>,
        blurb = "Generate a dot file of the underlying pipeline and return its file path")
    ]
    #[property(name="pipeline", get = Self::pipeline, type = Option<gst::Pipeline>, blurb = "The underlying pipeline in use")]
    state: Mutex<State>,
    start_completed: Mutex<bool>,

    #[property(name="pool", get, type = super::UriDecodePool, blurb = "The pool used")]
    pool: super::UriDecodePool,
}

impl Default for UriDecodePoolSrc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            start_completed: Default::default(),
            pool: pool::PIPELINE_POOL_POOL.lock().unwrap().clone(),
        }
    }
}

static START_TIME: Lazy<gst::ClockTime> = Lazy::new(gst::get_timestamp);

static DUMPDOT_DIR: Lazy<Option<Box<PathBuf>>> = Lazy::new(|| {
    if let Ok(dotdir) = std::env::var("GST_DEBUG_DUMP_DOT_DIR") {
        let path = Path::new(&dotdir);
        if path.exists() && path.is_dir() {
            return Some(Box::new(path.to_owned()));
        }
    }

    None
});

pub(crate) static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "uridecodepoolsrc",
        gst::DebugColorFlags::FG_YELLOW,
        Some("Playbin Pool Src"),
    )
});

// Same gst::bus::BusStream but not dropping messages so other handlers
// can be set
#[derive(Debug)]
struct CustomBusStream {
    bus: glib::WeakRef<gst::Bus>,
    receiver: futures::channel::mpsc::UnboundedReceiver<gst::Message>,
}

impl CustomBusStream {
    fn new(bus: &gst::Bus) -> Self {
        let (sender, receiver) = futures::channel::mpsc::unbounded();

        bus.connect_sync_message(None, move |_, msg| {
            let _ = sender.unbounded_send(msg.to_owned());
        });

        Self {
            bus: bus.downgrade(),
            receiver,
        }
    }
}

impl Drop for CustomBusStream {
    fn drop(&mut self) {
        if let Some(bus) = self.bus.upgrade() {
            bus.unset_sync_handler();
        }
    }
}

impl futures::Stream for CustomBusStream {
    type Item = gst::Message;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.receiver.poll_next_unpin(context)
    }
}

impl futures::stream::FusedStream for CustomBusStream {
    fn is_terminated(&self) -> bool {
        self.receiver.is_terminated()
    }
}

impl UriDecodePoolSrc {
    pub(crate) fn send_seek(&self, seek: gst::Event) {
        gst::debug!(CAT, imp: self, "Sending seek event {:?}", seek);

        self.state.lock().unwrap().ignore_seek = true;
        self.obj().send_event(seek);
        self.state.lock().unwrap().ignore_seek = false;
    }

    fn pipeline(&self) -> Option<gst::Pipeline> {
        self.state
            .lock()
            .unwrap()
            .decoderpipe
            .as_ref()
            .map(|pipe| pipe.pipeline())
    }

    fn dot_pipeline(&self) -> Option<String> {
        if DUMPDOT_DIR.is_none() {
            return None;
        }

        let decoderpipe = match self.state.lock().unwrap().decoderpipe.as_ref() {
            Some(decoderpipe) => decoderpipe.clone(),
            None => {
                gst::info!(CAT, imp: self, "No decoderpipe to dump");
                return None;
            }
        };

        let pipeline = decoderpipe.pipeline();
        let fname = format!(
            "{}-{}-{}.dot",
            gst::get_timestamp() - *START_TIME,
            self.obj().name(),
            pipeline.name()
        );

        let dot_file = DUMPDOT_DIR.as_ref().unwrap().join(&fname);
        let mut file = std::fs::File::create(dot_file)
            .map_err(|e| {
                gst::warning!(CAT, "Could not create dot file: {e:?}");
                e
            })
            .ok()?;

        file.write_all(
            pipeline
                .debug_to_dot_data(gst::DebugGraphDetails::all())
                .as_bytes(),
        )
        .map_err(|e| {
            gst::warning!(CAT, imp: self, "Failed to write dot file: {e:?}");

            e
        })
        .ok()?;

        Some(fname)
    }

    pub(crate) fn initial_seek_event(&self) -> Option<gst::Event> {
        if let Some(seek_event) = self
            .obj()
            .emit_by_name::<Option<gst::Event>>("get-initial-seek", &[])
        {
            if let gst::EventView::Seek(s) = seek_event.view() {
                let mut settings = self.settings.lock().unwrap();
                let (rate, flags, start_type, start, stop_type, stop) = s.get();

                if let gst::GenericFormattedValue::Time(start) = start {
                    settings.inpoint = Some(start.expect("Start value is mandatory in the seek"));
                    if let gst::GenericFormattedValue::Time(stop) = stop {
                        if let Some(stop) = stop {
                            settings.duration = Some(stop - start.unwrap());
                        } else {
                            settings.duration = None;
                        }
                    }
                } else {
                    unreachable!("Unsupported start type: {:?}", start_type);
                }
                settings.reverse = if rate < 0.0 { true } else { false };
            } else {
                unreachable!();
            }

            return Some(seek_event);
        }

        let settings = self.settings.lock().unwrap();

        if let Some(inpoint) = settings.inpoint {
            gst::debug!(CAT, imp: self, "inpoint: {:?} - duration: {:?}", inpoint, settings.duration);
            let stop = if let Some(duration) = settings.duration {
                Some(inpoint + duration)
            } else {
                gst::ClockTime::NONE
            };

            let seek = gst::event::Seek::new(
                if settings.reverse { -1.0 } else { 1.0 },
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                inpoint,
                gst::SeekType::Set,
                stop,
            );

            Some(seek)
        } else {
            None
        }
    }

    fn handle_bus_messages(&self, bus: gst::Bus, decoderpipe: &DecoderPipeline) {
        let obj = self.obj().clone();
        let mut bus_stream = CustomBusStream::new(&bus);
        let weak_decoderpipe = decoderpipe.downgrade();

        RUNTIME.spawn(async move {
            while let Some(message) = bus_stream.next().await {
                let view = message.view();

                let this = obj.imp();
                let decoderpipe = {
                    let state = this.state.lock().unwrap();

                    if state.decoderpipe == weak_decoderpipe.upgrade() && state.decoderpipe.is_some() {
                        state.decoderpipe.as_ref().unwrap().clone()
                    } else {
                        gst::debug!(CAT, imp: this, "Got message {:?} without decoderpipe", message);
                        // We have been disconnected while we already entered the
                        // callback it seems
                        return;
                    }
                };

                match view {
                    gst::MessageView::StateChanged(s) => {

                        let mut start_completed = this.start_completed.lock().unwrap();

                        if !*start_completed
                            && s.src() == Some(decoderpipe.pipeline().upcast_ref())
                            && s.pending() == gst::State::VoidPending
                            // Can't call `start_complete` if the srcpad has not been activated yet
                            // (we will call start_complete in the `start()` vmethod in that case)
                            && this.obj().src_pad().mode() == gst::PadMode::Push
                        {
                            *start_completed = true;
                            gst::error!(CAT, obj: obj, "Calling start_complete");
                            obj.start_complete(gst::FlowReturn::Ok);
                        }
                    }
                    gst::MessageView::Error(s) => {
                        if let Some(p) = obj.imp().decoderpipe() { p.pipeline().debug_to_dot_file_with_ts(
                                gst::DebugGraphDetails::all(),
                                format!("{}-error", obj.name()),
                            ) }
                        gst::error!(CAT, obj: obj, "Got error message: {s} from {decoderpipe:?} (uri: {:?})", this.settings.lock().unwrap().uri);
                        if let Err(e) = obj.post_message(s.message().to_owned()) {
                            gst::error!(CAT, "Could not post error message: {e:?}");
                        }
                    }
                    _ => (),
                }
            }
        });
    }

    // Avoid sending the seek to the baseclass until we have called `start_complete()` as
    // seeking in the baseclass starts the srcpad tasks, and then we can end up calling `start_complete`,
    // which needs the STREAM_LOCK, while it is taken by the streaming thread.
    //
    // Returns `true`` if the event should be postponed or `false` if it should be sent to the base
    // class
    fn handle_seek_event(&self, event: &gst::Event) -> bool {
        gst::error!(CAT, imp: self, "Got event: {event:?}");
        let start_completed = self.start_completed.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        state.seek_event = Some(event.clone());

        if !*start_completed {
            // Ensure that the right seek will be used when the pipeline reaches
            // playing state
            if let Some(p) = state.decoderpipe.as_ref() {
                p.imp().seek(event.clone());
            }

            gst::info!(CAT, imp: self, "Waiting for start to complete before sending seek");
            // And force the base class to handle the seek for us when we call `start_complete`
            unsafe {
                let obj = self.obj();
                let _obj_lock = obj.object_lock();
                let base_src: *mut gst_base::ffi::GstBaseSrc =
                    obj.upcast_ref::<gst_base::BaseSrc>().to_glib_none().0;
                (*base_src).pending_seek = event.to_glib_full();
            }

            true
        } else {
            false
        }
    }

    /// Ensures that a `stream-start` event with the right ID has been received
    /// already
    fn requested_stream_started(&self, decoderpipe: &DecoderPipeline) -> bool {
        let selected_stream = match decoderpipe.stream() {
            Some(stream) => stream.stream_id(),
            None => {
                gst::info!(CAT, imp: self, "No stream selected yet");
                return false;
            }
        }
        .unwrap();

        decoderpipe
            .sink()
            .sink_pads()
            .first()
            .unwrap()
            .stream_id()
            .map_or(false, |id| {
                if id.as_str() != selected_stream.as_str() {
                    gst::info!(
                        CAT,
                        imp: self,
                        "(Still?) Using wrong stream {} instead of selected: {} - dropping",
                        id,
                        selected_stream
                    );
                    false
                } else {
                    true
                }
            })
    }

    fn process_objects(
        &self,
        event_type: Option<gst::EventType>,
    ) -> Result<gst::MiniObject, gst::FlowError> {
        gst::log!(CAT, imp: self, "Processing objects: {event_type:?}");
        assert!(
            event_type.is_none()
                || event_type == Some(gst::EventType::Caps)
                || event_type == Some(gst::EventType::Segment)
        );

        let decoderpipe = self
            .state
            .lock()
            .unwrap()
            .decoderpipe
            .as_ref()
            .unwrap()
            .clone();
        let sink = decoderpipe.sink();
        let sink_sinkpad = decoderpipe.sink().sink_pads().first().unwrap().clone();

        let return_func =
            |this: &Self, obj: gst::MiniObject| -> Result<gst::MiniObject, gst::FlowError> {
                if this.state.lock().unwrap().flushing {
                    return Err(gst::FlowError::Flushing);
                }

                Ok(obj)
            };

        loop {
            if self.state.lock().unwrap().flushing {
                gst::debug!(CAT, "Flushing...");
                return Err(gst::FlowError::Flushing);
            }

            if self.requested_stream_started(&decoderpipe)
                && self.state.lock().unwrap().seek_seqnum.is_none()
            {
                match event_type {
                    Some(gst::EventType::Caps) => {
                        if let Some(caps) = sink_sinkpad.current_caps() {
                            return return_func(self, caps.upcast());
                        }
                    }
                    Some(gst::EventType::Segment) => {
                        if let Some(segment) = sink_sinkpad.sticky_event::<gst::event::Segment>(0) {
                            return return_func(
                                self,
                                gst::event::Segment::new(segment.segment()).upcast(),
                            );
                        }
                    }
                    Some(t) => todo!("Implement support for {t:?}"),
                    _ => (),
                }
            }

            let is_eos = sink.is_eos();
            // Avoid blocking forever if for some reason the underlying pipeline is stuck, allowing
            // the element to be flushed/stopped
            let obj = match sink.try_pull_object(gst::ClockTime::from_seconds(10)) {
                Some(obj) => Ok(obj),
                None => {
                    // Handle the case where the sink changed its EOS state
                    // between the pull and now
                    if is_eos || sink.is_eos() {
                        let mut state = self.state.lock().unwrap();
                        if state.seek_seqnum.is_some() {
                            let pipeline = state.decoderpipe.as_ref().unwrap().clone();

                            gst::info!(CAT, imp: self, "Got EOS while waiting for FLUSH_STOP on {}", pipeline.name());
                            drop(state);

                            self.dot_pipeline();

                            // Assert if we have been waiting for flush for more than 10s
                            // to avoid infinite loop
                            gst::fixme!(CAT, imp: self, "find a way to avoid that ugly sleep");
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            continue;
                        }

                        if state.needs_segment {
                            gst::debug!(CAT, imp: self, "Needs segment!");
                            let segment =
                                sink
                                    .sink_pads().first()
                                    .unwrap()
                                    .sticky_event::<gst::event::Segment>(0)
                                    .map_or_else(|| {
                                        gst::info!(CAT, imp: self, "Sticky segment not found, pushing original seek segment");

                                        state.seek_segment.clone()
                                    }, |segment| {
                                        let segment = segment.segment().clone();

                                        gst::info!(CAT,
                                            imp: self,
                                            "Pushing segment {segment:#?} before returning EOS so downstream has the right seqnum");

                                        Some(segment)
                                    });

                            if let Some(segment) = segment {
                                match segment.downcast_ref::<gst::format::Time>() {
                                    Some(segment) => state.current_segment = Some(segment.clone()),
                                    None => {
                                        gst::element_imp_error!(
                                            self,
                                            gst::StreamError::Failed,
                                            ["uridecodepoolsrc can only handle time segments"]
                                        );

                                        return Err(gst::FlowError::Error);
                                    }
                                }

                                drop(state);

                                self.obj().push_segment(&segment);
                            } else {
                                gst::warning!(CAT, imp: self, "No segment to push before EOS");
                            }
                        }

                        return Err(gst::FlowError::Eos);
                    }

                    if self.state.lock().unwrap().flushing {
                        gst::debug!(CAT, imp: self, "Flushing");
                        return Err(gst::FlowError::Flushing);
                    }

                    continue;
                }
            }?;

            let event = if obj.type_().is_a(gst::Event::static_type()) {
                let event = obj.downcast_ref::<gst::Event>().unwrap();
                gst::log!(CAT, imp: self, "Got event: {:?}", event);
                match event.view() {
                    gst::EventView::Tag(_) => {
                        gst::debug!(CAT, imp: self, "Got tag event, forwarding");

                        self.obj().src_pad().push_event(event.to_owned());
                        None
                    }
                    gst::EventView::FlushStop(f) => {
                        gst::debug!(CAT, imp: self, "Got FLUSH_STOP {f:?}");
                        let mut state = self.state.lock().unwrap();
                        if let Some(seq) = state.seek_seqnum.as_ref() {
                            if &event.seqnum() == seq {
                                gst::info!(
                                    CAT,
                                    imp: self,
                                    "Got FLUSH_STOP with right seqnum {seq:?}, restarting pushing buffers"
                                );
                                let _ = state.seek_seqnum.take();
                            } else {
                                gst::info!(CAT, imp: self, "Got FLUSH_STOP with seqnum {:?} while expecting {:?}", event.seqnum(), seq);
                            }
                        } else {
                            gst::info!(CAT, imp: self, "Got FLUSH_STOP without a seek seqnum, ignoring");
                        }

                        continue;
                    }
                    _ => Some(event),
                }
            } else {
                None
            };

            if !self.requested_stream_started(&decoderpipe) {
                gst::info!(CAT, imp: self, "Got {obj:?} from wrong stream, dropping");
                continue;
            }

            if let Some(event) = event {
                if let gst::EventView::Caps(c) = event.view() {
                    gst::debug!(CAT, imp: self, "Got caps: {:?}", c.caps());
                    if matches!(event_type, Some(gst::EventType::Caps)) {
                        return return_func(self, c.caps().to_owned().upcast());
                    } else {
                        gst::debug!(CAT, imp: self, "Pushing new caps downstream");
                        self.set_caps(c.caps().to_owned())?;
                    }
                }
            } else if obj.type_().is_a(gst::Sample::static_type()) {
                if event_type.is_some() {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "Got sample while waiting for {event_type:?}, dropping"
                    );
                    continue;
                }

                if self.state.lock().unwrap().seek_seqnum.is_some() {
                    gst::info!(
                        CAT,
                        imp: self,
                        "Got sample while waiting for FLUSH_STOP, dropping"
                    );

                    continue;
                }

                return return_func(self, obj);
            }
        }
    }

    fn set_decoderpipe(&self, decoderpipe: &DecoderPipeline) {
        let pipeline = decoderpipe.pipeline();
        let bus = pipeline.bus().unwrap();
        bus.enable_sync_message_emission();
        self.handle_bus_messages(bus, decoderpipe);

        let obj = self.obj();
        let mut state = self.state.lock().unwrap();
        state.source_setup_sigid = Some(decoderpipe.uridecodebin().connect_closure(
            "source-setup",
            false,
            glib::closure!(
                @watch obj => move |_decoderpipe: gst::Element, source: gst::Element| {
                    obj.emit_by_name::<()>("source-setup", &[&source]);
                }
            ),
        ));

        state.needs_segment = true;
        state.decoderpipe = Some(decoderpipe.clone());
        gst::error!(CAT, imp: self, "Setting decoderpipe to {decoderpipe:?}");
        drop(state);

        self.obj().notify("pipeline");
    }

    fn get_appsink_caps(&self) -> Option<gst::Caps> {
        let decoderpipe = self.decoderpipe().unwrap();
        let sink = decoderpipe.sink();
        let sink_pad = sink.static_pad("sink").unwrap();
        sink_pad
            .sticky_event::<gst::event::Caps>(0)
            .map(|caps| caps.caps_owned())
    }

    pub fn decoderpipe(&self) -> Option<DecoderPipeline> {
        self.state.lock().unwrap().decoderpipe.clone()
    }

    fn stream_start_probe(
        &self,
        probe_info: &mut gst::PadProbeInfo,
        stream_start: &gst::event::StreamStart,
    ) -> gst::PadProbeReturn {
        let decoderpipe = self.decoderpipe().unwrap();
        let stream = if let Some(stream) = decoderpipe.stream() {
            stream
        } else {
            gst::info!(CAT, imp: self, "StreamStart event without stream");
            return gst::PadProbeReturn::Ok;
        };

        let stream_id = stream.stream_id().unwrap();
        gst::debug!(CAT, imp: self, "{:?} Got stream: {:?} {}", decoderpipe, stream.stream_type(), stream_id);

        let settings = self.settings.lock().unwrap();
        let mut event_builder = gst::event::StreamStart::builder(
                                settings.stream_id.as_ref().map_or_else(|| stream_id.as_str(), |id| {
                                    let pipeline = decoderpipe.pipeline();
                                    if id.as_str() != stream_id.as_str() {
                                        pipeline.debug_to_dot_file_with_ts(
                                            gst::DebugGraphDetails::all(),
                                            format!("{}-wrong-stream-id", self.obj().name())
                                        );
                                        gst::info!(CAT, imp: self, "Selected wrong stream ID {}, {} could probably not be found \
                                            FAKING selected stream ID", stream_id, id)
                                    }

                                    id.as_str()
                                })
                            )
                            .flags(stream.stream_flags())
                            .stream(stream);

        if let Some(group_id) = stream_start.group_id() {
            event_builder = event_builder.group_id(group_id);
        }

        probe_info.data = Some(gst::PadProbeData::Event(event_builder.build()));

        gst::PadProbeReturn::Ok
    }

    fn segment_probe(
        &self,
        probe_info: &mut gst::PadProbeInfo,
        new_segment: &gst::event::Segment,
    ) -> gst::PadProbeReturn {
        let mut state = self.state.lock().unwrap();
        if state.seek_seqnum.is_some() {
            gst::info!(CAT, imp: self, "Dropping segment while waiting flushing seek {:?} to be executed", state.seek_seqnum);

            return gst::PadProbeReturn::Drop;
        }

        gst::debug!(CAT, imp: self, "Got segment {new_segment:#?}");
        if let Some(segment) = state.current_segment.clone() {
            let mut builder = gst::event::Segment::builder(&segment)
                .running_time_offset(new_segment.running_time_offset())
                .seqnum(new_segment.seqnum());

            if let Some(seqnum) = state.segment_seqnum.as_ref() {
                builder = builder.seqnum(*seqnum);
                gst::log!(CAT, imp: self, "Setting segment seqnum: {seqnum:?}");
            }

            state.needs_segment = false;
            probe_info.data = Some(gst::PadProbeData::Event(builder.build()));

            gst::PadProbeReturn::Ok
        } else {
            gst::debug!(CAT, imp: self, "Trying to push a segment before we received one, dropping it.");

            gst::PadProbeReturn::Drop
        }
    }

    pub fn set_caps(&self, caps: gst::Caps) -> Result<(), gst::FlowError> {
        self.obj()
            .upcast_ref::<gst_base::BaseSrc>()
            .set_caps(&caps)
            .map_err(|e| {
                if self
                    .obj()
                    .src_pad()
                    .pad_flags()
                    .contains(gst::PadFlags::FLUSHING)
                {
                    gst::FlowError::Flushing
                } else {
                    gst::error!(CAT, "Could not set caps: {e:?}");
                    gst::FlowError::NotNegotiated
                }
            })
    }

    fn push_segment(
        &self,
        segment: Option<gst::FormattedSegment<gst::GenericFormattedValue>>,
    ) -> Result<(), gst::FlowError> {
        if let Some(segment) = segment {
            let seg = match segment.downcast_ref::<gst::format::Time>() {
                Some(seg) => seg.clone(),
                None => {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ["Time segment needed"]
                    );
                    return Err(gst::FlowError::Error);
                }
            };

            let mut state = self.state.lock().unwrap();
            if Some(seg.clone()).as_ref() != state.current_segment.as_ref() {
                state.current_segment = Some(seg);
                drop(state);

                gst::debug!(CAT, imp: self, "Pushing segment: {segment:?}");
                self.obj().push_segment(segment.upcast_ref());
            }
        }

        Ok(())
    }

    pub(crate) fn create_filter(
        &self,
        pipeline: &DecoderPipeline,
        pad: &gst::Pad,
    ) -> Option<gst::Element> {
        self.obj()
            .emit_by_name::<Option<gst::Element>>("create-filter", &[&pipeline.pipeline(), &pad])
    }
}

#[glib::object_subclass]
impl ObjectSubclass for UriDecodePoolSrc {
    const NAME: &'static str = "GstUriDecodePoolSrc";
    type Type = super::UriDecodePoolSrc;
    type ParentType = gst_base::BaseSrc;

    type Interfaces = (gst::ChildProxy,);
}

#[glib::derived_properties]
impl ObjectImpl for UriDecodePoolSrc {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                /**
                 * uridecodepoolsrc::source-setup:
                 * @source: The source element to setup
                 *
                 * This signal is emitted after the source element has been created,
                 * so it can be configured by setting additional properties (e.g. set
                 * a proxy server for an http source, or set the device and read
                 * speed for an audio cd source). This is functionally equivalent
                 * to connecting to the notify::source signal, but more convenient.
                 *
                 * This signal is usually emitted from the context of a GStreamer
                 * streaming thread.
                 */
                glib::subclass::Signal::builder("source-setup")
                    .param_types([gst::Element::static_type()])
                    .build(),
                /**
                 * uridecodepoolsrc::get-initial-seek:
                 *
                 * This signal is emited when its underlying pipeline has been created
                 * and is ready to be seeked.
                 *
                 * This signal overrides the #uridecodepoolsrc::inpoint and
                 * #uridecodepoolsrc::duration properties as it will be used instead of those
                 * values for the initial seek.
                 *
                 * Returns: a #GstEvent or None if a seek should be sent as soon as possible to the
                 * underlying pipeline.                  */
                glib::subclass::Signal::builder("get-initial-seek")
                    .return_type::<Option<gst::Event>>()
                    .build(),
                /**
                 * uridecodepoolsrc::create-filter:
                 * @pipeline: The pipeline to add the filter to
                 * @pad: The pad after which the filter will be added
                 *
                 * Returns: a #GstElement which as exactly 1 sinkpad and 1 srcpad
                 * to add as as a filter right before the sink of the underlying pipeline.
                 */
                glib::subclass::Signal::builder("create-filter")
                    .param_types([gst::Pipeline::static_type(), gst::Pad::static_type()])
                    .return_type::<Option<gst::Element>>()
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        let _ = START_TIME.as_ref();
        self.parent_constructed();
        self.obj().set_format(gst::Format::Time);
        self.obj().set_async(true);
        self.obj().set_automatic_eos(false);

        self.obj().src_pad().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH | gst::PadProbeType::QUERY_DOWNSTREAM,
            glib::clone!(@weak self as this => @default-return gst::PadProbeReturn::Ok, move |_, probe_info| {
                let event = match &probe_info.data {
                    Some(gst::PadProbeData::Event(event)) => event.clone(),
                    Some(gst::PadProbeData::Query(q)) => {
                        if let gst::QueryView::Allocation(_) = q.view() {
                            return gst::PadProbeReturn::Drop;
                        }

                        return gst::PadProbeReturn::Ok
                    }
                    _ => unreachable!(),
                };

                match event.view() {
                    gst::EventView::FlushStart(_) | gst::EventView::FlushStop(_) => {
                        gst::info!(CAT, imp: this, "Got flush {event:?}");
                        return gst::PadProbeReturn::Ok
                    }
                    gst::EventView::StreamStart(s) => this.stream_start_probe(probe_info, s),
                    gst::EventView::Segment(s) => this.segment_probe(probe_info, s),
                    _ => gst::PadProbeReturn::Ok
                }

            }));
    }
}

impl GstObjectImpl for UriDecodePoolSrc {}

impl ElementImpl for UriDecodePoolSrc {
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let res = self.parent_change_state(transition);

        // Handle the case where nlecomposition sent a 'fake' seek event right before being tore
        // down
        if transition == gst::StateChange::ReadyToNull {
            let mut state = self.state.lock().unwrap();
            if let Some(pipeline) = state.decoderpipe.take() {
                self.pool.release(pipeline);
            }
        }

        res
    }

    fn send_event(&self, event: gst::Event) -> bool {
        gst::error!(CAT, imp: self, "Got event {event:?}");
        if let gst::EventView::Seek(s) = event.view() {
            gst::info!(CAT, imp: self, "Got {s:?}");

            if event.structure().map_or(false, |s| {
                gst::error!(CAT, "Got struct {s:?}");
                s.has_field("nlecomposition-seek")
            }) {
                gst::error!(CAT, "---> HERE I AM");
                let decoderpipe = if let Some(decoderpipe) = self.decoderpipe() {
                    decoderpipe
                } else {
                    let has_uri = self.settings.lock().unwrap().uri.is_some();
                    let decoderpipe = if has_uri {
                        let seek_event = event.clone();
                        // let get_initial_seek_sigid =
                        //     self.obj()
                        //         .connect("get-initial-seek", false, move |args| {
                        //             let obj = args[0].get::<gst::Element>().unwrap();
                        //             gst::error!(CAT, obj: obj, "FIXME, be smart and rescale the seek to handle the  whole object taking into account the NLE seek!!");
                        //             Some(seek_event.clone().into())
                        //         }
                        //     );
                        //
                        let decoderpipe = self.pool.get_decoderpipe(&self.obj());
                        if !decoderpipe.seek_handler().has_eos_sample() {
                            self.state.lock().unwrap().seek_seqnum =
                                decoderpipe.imp().initial_seek().map(|s| s.seqnum());
                        }
                        // self.obj().disconnect(get_initial_seek_sigid);

                        decoderpipe
                    } else {
                        gst::error!(CAT, imp: self, "No URI set!!!!!!");
                        return false;
                    };

                    self.set_decoderpipe(&decoderpipe);

                    decoderpipe
                };
                gst::error!(CAT, imp: self, "Setting decoderpipe to {decoderpipe:?}");
                if decoderpipe
                    .seek_handler()
                    .handle_nlecomposition_seek(&self.obj(), &event)
                {
                    self.state.lock().unwrap().seek_event = Some(event.clone());
                    gst::error!(CAT, imp: self, "NleComposition initialization seek handled");
                    return true;
                }
            } else {
                gst::error!(CAT, "---> NOOOO NLE SEEK!! HERE I AM");
            }

            // Avoid base class to handle seek event when it has been started
            // but the underlying pipeline is not ready yet.
            if self.handle_seek_event(&event) {
                return true;
            }
            // } else if let gst::EventView::CustomUpstream(e) = event.view() {
            //     if event
            //         .structure()
            //         .map_or(false, |s| s.has_name("nlecomposition-seek"))
            //     {
            //         let has_uri = self.settings.lock().unwrap().uri.is_some();
            //         let decoderpipe = if has_uri {
            //             self.pool.get_decoderpipe(&self.obj())
            //         } else {
            //             return false;
            //         };
            //
            //         self.state.lock().unwrap().decoderpipe = Some(decoderpipe.clone());
            //         if decoderpipe
            //             .seek_handler()
            //             .handle_nlecomposition_seek(&self.obj(), e)
            //         {
            //             gst::error!(CAT, imp: self, "NleComposition FAKE seek handled");
            //             return true;
            //         }
            //     }
        }

        self.parent_send_event(event)
    }

    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Source",
                "Source",
                "Playback source which runs a pool of decoderpipe instances",
                "Thibault Saunier <tsaunier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSrcImpl for UriDecodePoolSrc {
    fn is_seekable(&self) -> bool {
        static NOTIFIED: Once = Once::new();

        NOTIFIED.call_once(|| {
            gst::fixme!(CAT, "Handle not seekable underlying pipelines");
        });

        true
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Start flushing!");
        self.state.lock().unwrap().flushing = true;

        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Stop flushing!");
        self.state.lock().unwrap().flushing = false;

        Ok(())
    }

    fn do_seek(&self, segment: &mut gst::Segment) -> bool {
        let mut state = self.state.lock().unwrap();
        let seek_event = if let Some(seek_event) = state.seek_event.take() {
            seek_event
        } else {
            gst::info!(CAT, imp: self, "Ignoring initial seek");

            return true;
        };

        gst::debug!(CAT, imp: self, "Seeking with saved {seek_event:?}");
        let decoderpipe = state.decoderpipe.clone();
        drop(state);

        if let Some(decoderpipe) = decoderpipe {
            if let gst::EventView::Seek(s) = seek_event.view() {
                let values = s.get();
                if values.1.contains(gst::SeekFlags::FLUSH) {
                    let mut state = self.state.lock().unwrap();
                    state.segment_seqnum = Some(seek_event.seqnum());
                    state.needs_segment = true;
                    state.seek_segment = Some(segment.clone());

                    drop(state);
                    if decoderpipe.seek_handler().handle_seek(&self.obj(), s) {
                        gst::error!(CAT, imp: self, "Handling seek ourself, not forwarding to underlying pipeline");
                        return true;
                    }

                    self.state.lock().unwrap().seek_seqnum = Some(seek_event.seqnum());

                    gst::info!(CAT, imp: self, "Flushing seek... waiting for flush-stop with right seqnum ({:?}) before restarting pushing buffers", seek_event.seqnum());
                }

                values
            } else {
                unreachable!();
            };

            gst::debug!(
                CAT,
                imp: self,
                "Sending {seek_event:?} to {}",
                decoderpipe.imp().name()
            );

            if !decoderpipe.imp().seek(seek_event) {
                self.state.lock().unwrap().seek_seqnum = None;
            }
            true
        } else {
            gst::info!(CAT, imp: self, "No pipeline to seek");

            false
        }
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Starting");

        let has_uri = self.settings.lock().unwrap().uri.is_some();
        let decoderpipe = if let Some(decoderpipe) = self.decoderpipe() {
            decoderpipe
        } else if has_uri {
            self.pool.get_decoderpipe(&self.obj())
        } else {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                ("No URI provided")
            ));
        };

        let mut start_completed = self.start_completed.lock().unwrap();
        self.set_decoderpipe(&decoderpipe);
        let res = decoderpipe.imp().play().map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ("Failed to set underlying pipeline to PLAYING: {err:?}")
            )
        })?;
        if res == gst::StateChangeSuccess::Success {
            gst::debug!(CAT, imp: self, "Already ready");
            if !*start_completed {
                gst::error!(CAT, imp: self, "Calling complete");
                self.obj().start_complete(gst::FlowReturn::Ok);
                *start_completed = true;
                drop(start_completed);

                if let Some(pending_seek) = self.state.lock().unwrap().pending_seek.take() {
                    gst::info!(CAT, imp: self, "Sending pending seek {pending_seek:?} after start complete");
                    self.send_event(pending_seek);
                }
            } else {
                gst::info!(CAT, imp: self, "start_complete() already called");
            }
        } else {
            let settings = self.settings.lock().unwrap();
            gst::debug!(
                CAT,
                imp: self,
                "{:?} - {:?} Waiting {} state PLAYING to be reached after {res:?}",
                settings.stream_id,
                settings.caps,
                decoderpipe.imp().name()
            );
        }

        Ok(())
    }

    fn negotiate(&self) -> Result<(), gst::LoggableError> {
        if self
            .decoderpipe()
            .map_or(false, |p| p.seek_handler().has_eos_sample())
        {
            // we need to use the caps from the sample that triggered the fake EOS, so we
            // have to wait for it
            gst::error!(CAT, imp: self, "Changing stack, waiting for previous sample before renegotiating");

            return Ok(());
        }

        gst::info!(CAT, imp: self, "Caps changed, renegotiating");

        let caps = match self.process_objects(Some(gst::EventType::Caps)) {
            Ok(caps) => caps.downcast::<gst::Caps>().unwrap(),
            Err(e) => {
                if e == gst::FlowError::Eos {
                    if let Some(caps) = self.get_appsink_caps() {
                        caps
                    } else {
                        gst::info!(CAT, imp: self, "No sticky caps event on sink. Faking success.");
                        return Ok(());
                    }
                } else {
                    return Err(gst::loggable_error!(
                        CAT,
                        "No caps on appsink after prerolling {e:?}"
                    ));
                }
            }
        };

        gst::debug!(CAT, imp: self, "Negotiated caps: {:?}", caps);

        self.set_caps(caps)
            .map_err(|e| gst::loggable_error!(CAT, "Failed to set caps: {e:?}"))
    }

    fn event(&self, event: &gst::Event) -> bool {
        if let gst::EventView::Seek(seek) = event.view() {
            if self
                .decoderpipe()
                .map_or(false, |p| p.seek_handler().handle_seek(&self.obj(), seek))
            {
                gst::error!(CAT, imp: self, "Seek handled");
                return true;
            }

            // Avoid base class to handle seek event when it has been started
            // but the underlying pipeline is not ready yet.
            if self.handle_seek_event(event) {
                return true;
            }

            gst::debug!(CAT, imp: self, "Forwarding seek to base class {:?}", self.obj().src_pad().mode());
        }

        self.parent_event(event)
    }

    fn create(
        &self,
        _offset: u64,
        _buffer: Option<&mut gst::BufferRef>,
        _length: u32,
    ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
        let pipeline = self.decoderpipe().unwrap();

        gst::log!(CAT, imp: self, "create with underlying pipeline {} state: {:?}", pipeline.pipeline().name(), pipeline.pipeline().state(gst::ClockTime::ZERO));

        // If we are inside nlecomposition, we need to use the sample that triggered the fake EOS
        // and set the caps from it.
        let (sample, eos_sample_caps_to_set) = if let Some(sample) =
            pipeline.seek_handler().get_eos_sample(&self.obj())?
        {
            if let Some(caps) = sample
                .caps()
                .map_or_else(|| self.get_appsink_caps(), |caps| Some(caps.to_owned()))
            {
                gst::info!(CAT, imp: self, "EOS sample: {sample:?} --> Forcing caps");
                (sample, Some(caps))
            } else {
                gst::error!(CAT, imp: self, "EOS sample: {sample:?} --> can't find any caps after EOS sample, not-negotiated");
                return Err(gst::FlowError::NotNegotiated);
            }
        } else {
            (
                self.process_objects(None)?
                    .downcast::<gst::Sample>()
                    .expect("Should always have a sample when not waiting for EOS"),
                None,
            )
        };
        gst::trace!(CAT, imp: self, "Got {sample:?}");

        let segment = match pipeline.seek_handler().process(&self.obj(), &sample) {
            Ok(SeekInfo::SeekSegment(seqnum, segment)) => {
                gst::log!(CAT, imp: self, "Got seek segment after process --> new seqnum: {seqnum:?} -- {:?}",  self.state.lock().unwrap().seek_seqnum);
                self.state.lock().unwrap().segment_seqnum = Some(seqnum);

                Some(segment)
            }
            Ok(SeekInfo::None) => {
                gst::log!(CAT, imp: self, "Using sample segment: {:?}", sample.segment());
                sample.segment().cloned()
            }
            Err((gst::FlowError::Eos, Some(seqnum))) => {
                self.state.lock().unwrap().segment_seqnum = Some(seqnum);
                if eos_sample_caps_to_set.is_none() {
                    return Err(gst::FlowError::Eos);
                }

                gst::info!(
                    CAT,
                    "We got an EOS sample right after getting an EOS sample from previous run
                     pushing segment to ensure seqnum is propagated"
                );
                self.state.lock().unwrap().segment_seqnum = Some(seqnum);

                if let Err(e) = self.set_caps(eos_sample_caps_to_set.unwrap()) {
                    gst::error!(CAT, imp: self, "Failed to push caps: {:?}", e);
                }

                if let Err(e) = self.push_segment(sample.segment().cloned()) {
                    gst::error!(CAT, imp: self, "Failed to push segment: {:?}", e);
                }
                return Err(gst::FlowError::Eos);
            }
            Err((e, _)) => {
                return Err(e);
            }
            _ => unreachable!(),
        };

        if let Some(caps) = eos_sample_caps_to_set {
            gst::debug!(CAT, imp: self, "Setting caps: {:?}", caps);
            self.set_caps(caps)?;
        }

        self.push_segment(segment)?;

        if let Some(buffer) = pipeline.seek_handler().handle_sample(&sample) {
            Ok(gst_base::subclass::base_src::CreateSuccess::NewBuffer(
                buffer,
            ))
        } else if sample.buffer_list_owned().is_some() {
            unreachable!("Buffer lists are not supported");
        } else {
            gst::error!(CAT, imp: self, "Got sample without buffer or buffer list");

            Err(gst::FlowError::Error)
        }
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Stopping");
        let pipeline = {
            let mut state = self.state.lock().unwrap();

            state.current_segment = None;
            state.seek_seqnum = None;
            state.segment_seqnum = None;
            state.seek_event = None;
            state.pending_seek = None;

            let decoderpipe = state.decoderpipe.as_ref().unwrap().clone();
            let pipeline = decoderpipe.pipeline();
            if let Some(sigid) = state.bus_message_sigid.take() {
                pipeline.bus().unwrap().disconnect(sigid);
            }
            if let Some(sigid) = state.source_setup_sigid.take() {
                decoderpipe.uridecodebin().disconnect(sigid);
            }
            pipeline.bus().unwrap().disable_sync_message_emission();

            state.decoderpipe.take().unwrap()
        };

        *self.start_completed.lock().unwrap() = false;
        gst::info!(CAT, imp: self, "Releasing {pipeline:?}");
        self.pool.release(pipeline);

        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Uri(q) => {
                q.set_uri(self.settings.lock().unwrap().uri.as_ref());
                return true;
            }
            gst::QueryViewMut::Duration(_) => {
                if let Some(decoderpipe) = self.decoderpipe() {
                    return decoderpipe.pipeline().query(query);
                }
            }
            gst::QueryViewMut::Custom(s) => {
                if let Some(decoderpipe) = self.decoderpipe() {
                    return decoderpipe.pipeline().query(query);
                }
            }
            _ => (),
        }

        BaseSrcImplExt::parent_query(self, query)
    }
}

impl ChildProxyImpl for UriDecodePoolSrc {
    fn child_by_index(&self, _index: u32) -> Option<glib::Object> {
        None
    }

    fn children_count(&self) -> u32 {
        0
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        match name {
            "pool" => Some(self.pool.clone().upcast()),
            _ => None,
        }
    }
}
