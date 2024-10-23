use parking_lot::ReentrantMutex;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Mutex, OnceLock,
};

use gst::{
    glib::{self, Properties},
    prelude::*,
    subclass::prelude::*,
};
use once_cell::sync::Lazy;

use crate::uridecodepool::seek_handler;

use super::{pool::CAT, seek_handler::SeekHandler};

#[derive(Debug)]
struct State {
    stream: Option<gst::Stream>,
    stream_id: Option<String>,
    caps: gst::Caps,
    unused_since: Option<std::time::Instant>,
    bus_message_sigid: Option<glib::SignalHandlerId>,
    stream_selection_seqnum: gst::Seqnum,

    target_src: Option<super::UriDecodePoolSrc>,
    pending_seek: Option<gst::Event>,

    // Seek to be sent to apply inpoint/duration values
    initial_seek: Option<gst::Event>,
    last_seek_seqnum: gst::Seqnum,

    pool: Option<super::UriDecodePool>,
}

#[derive(Properties, Debug)]
#[properties(wrapper_type = super::DecoderPipeline)]
pub struct DecoderPipeline {
    pub pipeline: OnceLock<gst::Pipeline>,
    pub uridecodebin: OnceLock<gst::Element>,
    pub sink: OnceLock<gst_app::AppSink>,

    #[property(name="pool", set, get, type = super::UriDecodePool, construct_only, member = pool)]
    #[property(name="initial-seek", set, get, type = gst::Event, construct_only, member = initial_seek)]
    state: Mutex<State>,

    // Working around https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/150 by
    // ensuring we do not send `SELECT_STREAM` while tearing down
    state_lock: ReentrantMutex<bool>,
    tearing_down: AtomicBool,

    #[property(name = "name", construct_only)]
    pub(crate) name: OnceLock<String>,

    pub(crate) seek_handler: OnceLock<SeekHandler>,
}

impl Default for DecoderPipeline {
    fn default() -> Self {
        Self {
            pipeline: Default::default(),
            sink: Default::default(),
            uridecodebin: Default::default(),
            state: Mutex::new(State {
                unused_since: None,
                stream: None,
                stream_id: None,
                caps: gst::Caps::builder_full()
                    .structure_with_any_features(gst::Structure::new_empty("video/x-raw"))
                    .build(),
                stream_selection_seqnum: gst::Seqnum::next(),
                last_seek_seqnum: gst::Seqnum::next(),
                bus_message_sigid: None,
                target_src: None,
                pool: None,
                pending_seek: None,
                initial_seek: None,
            }),
            state_lock: ReentrantMutex::new(false),
            tearing_down: AtomicBool::new(false),
            seek_handler: Default::default(),
            name: Default::default(),
        }
    }
}

impl PartialEq for DecoderPipeline {
    fn eq(&self, other: &Self) -> bool {
        self.pipeline == other.pipeline
    }
}

impl DecoderPipeline {
    fn pad_removed(&self, _pad: &gst::Pad) {
        let sinkpad = self.sink().static_pad("sink").unwrap();

        if let Some(peer) = sinkpad.peer() {
            if let Err(err) = peer.unlink(&sinkpad) {
                gst::error!(CAT, imp: self, "Could not unlink {}:{} from {:?}:{}: {err:?}",
                    peer.parent().unwrap().name(), peer.name(),
                    sinkpad.parent().map(|e| e.name()), sinkpad.name());
            }

            let pipeline = self.pipeline();
            if let Some(parent) = peer.parent() {
                if parent.parent().as_ref() == Some(pipeline.upcast_ref()) {
                    let element = parent.downcast::<gst::Element>().unwrap();

                    gst::log!(CAT, imp: self, "Removing {} from {}", element.name(), pipeline.name());
                    if let Err(e) = element.set_state(gst::State::Null) {
                        gst::error!(CAT, imp: self, "Could not set {} state to Null: {e:?}", element.name());
                    }
                    if let Err(e) = pipeline.remove(element.downcast_ref::<gst::Element>().unwrap())
                    {
                        gst::error!(CAT, imp: self, "Could not remove {} from pipeline: {e:?}", element.name());
                    }
                }
            }
        }
    }

    fn pad_added(&self, pad: &gst::Pad) {
        gst::debug!(CAT, imp: self, "Pad added: {:?}", pad);
        let sinkpad = self.sink().static_pad("sink").unwrap();
        if sinkpad.is_linked() {
            let peer = sinkpad.peer().unwrap();

            let pipeline = self.pipeline();
            pipeline.debug_to_dot_file_with_ts(
                gst::DebugGraphDetails::all(),
                format!("{}-already-linked-pad", pipeline.name()),
            );

            gst::error!(CAT, imp: self, "Got pad {}:{} while {}:{} already linked to {}:{}",
                pad.parent().unwrap().name(),
                pad.name(),
                sinkpad.parent().unwrap().name(),
                sinkpad.name(),
                peer.parent().unwrap().name(), peer.name());
            return;
        }

        if let Some(Some(filter)) = self
            .target_src()
            .map(|s| s.create_filter(&*self.obj(), pad))
        {
            gst::error!(CAT, imp: self, "Got filter: {filter:?}");
            if let Err(err) = self.pipeline().add(&filter) {
                gst::error!(CAT, imp: self, "Failed to add filter: {:?}", err);
                return;
            }

            filter.sync_state_with_parent().unwrap();

            let filter_sinkpad = filter.sink_pads().first().unwrap().clone();
            if let Err(err) = pad.link(&filter_sinkpad) {
                gst::error!(CAT, imp: self, "Failed to link pads: {:?}", err);
                gst::error!(CAT, imp: self, "Failed link pads {:?}:{:?}: {:#?}\n -> {:?}:{}: {:#?} \n: {:?}",
                    pad.parent().map(|p| p.name()), pad.name(),
                    pad.query_caps(None),
                    sinkpad.parent().map(|p| p.name()), sinkpad.name(),
                    sinkpad.query_caps(None),
                    err);
            }

            let pad = filter.src_pads().first().unwrap().clone();
            if let Err(err) = pad.link(&sinkpad) {
                gst::error!(CAT, imp: self, "Failed to link pads: {:?}", err);
                gst::error!(CAT, imp: self, "Failed link pads {:?}:{:?}: {:#?}\n -> {:?}:{}: {:#?} \n: {:?}",
                    pad.parent().map(|p| p.name()), pad.name(),
                    pad.query_caps(None),
                    sinkpad.parent().map(|p| p.name()), sinkpad.name(),
                    sinkpad.query_caps(None),
                    err);
            }

            return;
        }

        if let Err(err) = pad.link(&sinkpad) {
            if self.state.lock().unwrap().bus_message_sigid.is_none() {
                gst::debug!(CAT, imp: self, "Pad added but no stream selected anymore");

                return;
            }
            gst::error!(
                CAT,
                imp: self,
                "Failed link pads {:?}:{:?}: {:#?}\n -> {:?}:{}: {:#?} \n: {:?}",
                pad.parent().map(|p| p.name()),
                pad.name(),
                pad.query_caps(None),
                sinkpad.parent().map(|p| p.name()),
                sinkpad.name(),
                sinkpad.query_caps(None),
                err
            );
        }
    }

    fn caps(&self) -> gst::Caps {
        self.state.lock().unwrap().caps.clone()
    }

    pub(crate) fn stream_type(&self) -> gst::StreamType {
        match self
            .state
            .lock()
            .unwrap()
            .caps
            .structure(0)
            .map(|s| s.name().as_str())
        {
            Some("video/x-raw") => gst::StreamType::VIDEO,
            Some("audio/x-raw") => gst::StreamType::AUDIO,
            Some("text/x-raw") => gst::StreamType::TEXT,
            _ => gst::StreamType::UNKNOWN,
        }
    }

    pub(crate) fn requested_stream_id(&self) -> Option<String> {
        self.state.lock().unwrap().stream_id.clone()
    }

    pub(crate) fn sink(&self) -> gst_app::AppSink {
        self.sink.get().unwrap().clone()
    }

    pub(crate) fn seek_handler(&self) -> &SeekHandler {
        self.seek_handler.get().unwrap()
    }

    pub(crate) fn pipeline(&self) -> gst::Pipeline {
        self.pipeline.get().unwrap().clone()
    }

    pub(crate) fn pipeline_ref(&self) -> &gst::Pipeline {
        self.pipeline.get().unwrap()
    }

    pub(crate) fn name(&self) -> &str {
        &self.name.get().unwrap()
    }

    pub(crate) fn initial_seek(&self) -> Option<gst::Event> {
        self.state.lock().unwrap().initial_seek.clone()
    }

    pub(crate) fn reset(&self, uri: &str, caps: &gst::Caps, stream_id: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        state.unused_since = None;
        state.stream = None;
        state.caps = caps.clone();
        state.stream_id = stream_id.map(|s| s.to_string());
        state.stream_selection_seqnum = gst::Seqnum::next();
        self.uridecodebin().set_property("caps", caps);
        self.sink().set_property("caps", caps);

        if state.bus_message_sigid.is_none() {
            let bus = self.pipeline_ref().bus().unwrap();
            bus.enable_sync_message_emission();
            state.bus_message_sigid = Some(bus.connect_sync_message(
                None,
                glib::clone!(@weak self as this => move |_, msg|
                    this.handle_bus_message(msg)
                ),
            ));
        }
        self.set_uri(uri);
    }

    pub fn seek(&self, seek_event: gst::Event) -> bool {
        let pipeline = {
            let mut state = self.state.lock().unwrap();

            if state.last_seek_seqnum == seek_event.seqnum() {
                gst::info!(CAT, obj: self.pipeline_ref(), "Ignoring duplicated seek event: {:?}", seek_event);

                return true;
            }

            let (res, current_state, pending_state) =
                self.pipeline_ref().state(Some(gst::ClockTime::ZERO));
            gst::error!(CAT, obj: self.pipeline_ref(), "{:?} Current state: {:?}", state.target_src, state);
            match res {
                Ok(_) => {
                    if current_state != gst::State::Playing {
                        gst::info!(
                            CAT,
                            obj: self.pipeline_ref(),
                            "Waiting for pipeline to be ready seeking it {seek_event:?}"
                        );

                        state.pending_seek = Some(seek_event);
                        return true;
                    } else {
                        let _ = state.pending_seek.take();
                    }
                }
                Err(e) => {
                    gst::error!(CAT, obj: self.pipeline_ref(), "Failed to get state: {e:?}");

                    return false;
                }
            }

            state.last_seek_seqnum = seek_event.seqnum();
            self.pipeline()
        };

        if !self
            .seek_handler()
            .should_send_seek(&seek_event, &*self.obj())
        {
            gst::error!(CAT, obj: pipeline, "asked not to discard seek {seek_event:?}");
            return false;
        }

        gst::error!(CAT, obj: pipeline, "--> Sending seek {:?}", seek_event);
        if !pipeline.send_event(seek_event) {
            gst::error!(CAT, obj: pipeline, "Failed to seek");
            return false;
        }

        true
    }

    fn handle_bus_message(&self, message: &gst::Message) {
        match message.view() {
            gst::MessageView::StreamCollection(s) => {
                let collection = s.stream_collection();

                let stream = if let Some(ref wanted_stream_id) = self.requested_stream_id() {
                    if let Some(stream) = collection.iter().find(|stream| {
                        let stream_id = stream.stream_id();
                        stream_id.map_or(false, |s| wanted_stream_id.as_str() == s.as_str())
                    }) {
                        gst::debug!(
                            CAT,
                            obj: self.pipeline_ref(),
                            "{:?} Selecting specified stream: {:?}",
                            self.name(),
                            wanted_stream_id
                        );

                        Some(stream)
                    } else {
                        gst::warning!(
                            CAT,
                            obj: self.pipeline_ref(),
                            "{:?} requested stream {} not found in {} - available: {:?}",
                            self.name(),
                            wanted_stream_id,
                            self.uridecodebin().property::<String>("uri"),
                            collection
                                .iter()
                                .map(|s| s.stream_id().to_owned())
                                .collect::<Vec<Option<glib::GString>>>()
                        );

                        None
                    }
                } else {
                    None
                };

                let stream = if let Some(stream) = stream {
                    stream
                } else if let Some(stream) = collection.iter().find(|stream| {
                    stream.stream_type() == self.stream_type() && stream.stream_id().is_some()
                }) {
                    gst::debug!(
                        CAT,
                        obj: self.pipeline_ref(),
                        "{:?} Selecting stream: {:?}",
                        self.name(),
                        stream.stream_id()
                    );
                    stream
                } else {
                    /* FIXME --- Post an error on the bus! */
                    gst::error!(
                        CAT,
                        obj: self.pipeline_ref(),
                        "{:?} No stream found for caps: {:?}",
                        self.name(),
                        self.caps()
                    );

                    return;
                };

                let seqnum = {
                    let mut state = self.state.lock().unwrap();
                    let _ = state.stream.insert(stream.clone());

                    state.stream_selection_seqnum
                };
                let uridecodebin = self.uridecodebin();

                loop {
                    gst::debug!(CAT, obj: self.pipeline_ref(), "Trying to get state lock");
                    if let Some(_state_lock) = self.state_lock.try_lock() {
                        message
                            .src()
                            .unwrap_or_else(|| uridecodebin.upcast_ref::<gst::Object>())
                            .downcast_ref::<gst::Element>()
                            .unwrap()
                            .send_event(
                                gst::event::SelectStreams::builder(&[stream
                                    .stream_id()
                                    .unwrap()
                                    .as_str()])
                                .seqnum(seqnum)
                                .build(),
                            );
                        break;
                    } else if self.tearing_down.load(Ordering::SeqCst) {
                        gst::error!(
                            CAT,
                            "Failed to get state lock, can not send stream selection??!!!"
                        );
                        break;
                    }
                }

                // if self.state.lock().unwrap().pending_seek.as_ref().is_some() {
                //     // We got a StreamCollection, we should be ready to seek now!
                //     self.seek_in_thread();
                // }
            }
            gst::MessageView::NeedContext(..)
            | gst::MessageView::HaveContext(..)
            | gst::MessageView::Element(..) => {
                if let Some(bus) = self.obj().pool().bus() {
                    gst::debug!(CAT, obj: self.pipeline_ref(), "Posting context message to the pool bus");
                    if let Err(e) = bus.post(message.to_owned()) {
                        gst::warning!(CAT, obj: self.pipeline_ref(), "Could not post message {message:?}: {e:?}");
                    }
                } else if let Some(target) = self.target_src() {
                    if let Err(e) = target.post_message(message.to_owned()) {
                        gst::warning!(CAT, obj: self.pipeline_ref(), "Could not post message {message:?}: {e:?}");
                    }
                }
            }
            gst::MessageView::Error(s) => {
                gst::error!(CAT, imp: self, "Got error message: {s}");
                self.pipeline_ref().debug_to_dot_file_with_ts(
                    gst::DebugGraphDetails::all(),
                    format!("error-{}", self.name()),
                );
            }
            gst::MessageView::StateChanged(s) => {
                if s.src().map_or(false, |s| {
                    s == self.pipeline_ref().upcast_ref::<gst::Object>()
                }) && s.current() == gst::State::Playing
                    && self.state.lock().unwrap().pending_seek.as_ref().is_some()
                {
                    gst::debug!(CAT, obj: self.pipeline_ref(), "Pipeline ready to seek");

                    self.seek_in_thread();
                }
            }
            _ => (),
        }
    }

    fn seek_in_thread(&self) {
        let pipeline = self.pipeline();

        // Send seek from some other thread to avoid deadlocks
        pipeline.call_async(glib::clone!(@weak self as this => move |pipeline| {
            let mut state = this.state.lock().unwrap();

            let seek_event = if let Some(seek_event) = state.pending_seek.take() {
                if this.seek_handler().should_send_seek(&seek_event, &*this.obj()) {
                    seek_event
                } else {
                    gst::error!(CAT, obj: pipeline, "asked not to discard seek {seek_event:?}");
                    return;
                }
            } else {
                gst::error!(CAT, obj: pipeline, "--> No pending seek");
                return;
            };

            gst::error!(CAT, obj: pipeline, "--> Sending pending seek {:?}", seek_event.seqnum());
            drop(state);

            if !pipeline.send_event(seek_event) {
                if let Err(e) = pipeline.post_message(gst::message::Error::new(
                    gst::CoreError::Failed,
                    "Failed to seek",
                )) {
                    gst::error!(CAT, obj: pipeline, "Failed to post error message: {e:?}");
                }
            }
        }));
    }

    pub(crate) fn unused_since(&self) -> Option<std::time::Instant> {
        self.state.lock().unwrap().unused_since
    }

    pub(crate) fn stream(&self) -> Option<gst::Stream> {
        self.state.lock().unwrap().stream.clone()
    }

    pub(crate) fn uridecodebin(&self) -> gst::Element {
        self.uridecodebin.get().unwrap().clone()
    }

    fn set_uri(&self, uri: &str) {
        self.uridecodebin().set_property("uri", uri);
    }

    pub(crate) fn play(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if self
            .state
            .lock()
            .unwrap()
            .pool
            .as_ref()
            .unwrap()
            .imp()
            .deinitialized()
        {
            gst::error!(CAT, "Pool is already deinitialized");
            return Err(gst::StateChangeError);
        }

        gst::debug!(CAT, obj: self.pipeline_ref(), "Starting pipeline");

        self.tearing_down.store(false, Ordering::SeqCst);
        let (res, state, pending) = self.pipeline_ref().state(gst::ClockTime::ZERO);
        res?;
        if state < gst::State::Paused {
            if let Some(seek_event) = self.initial_seek() {
                gst::debug!(CAT, obj: self.pipeline_ref(), "Using initial seek as pending_seek: {:?}", seek_event);
                self.state.lock().unwrap().pending_seek = Some(seek_event);
            }
        }
        gst::debug!(
            CAT,
            "Pipeline state is {state:?} and pending is {pending:?}"
        );
        self.pipeline_ref().set_state(gst::State::Playing)
    }

    pub(crate) fn target_src(&self) -> Option<super::UriDecodePoolSrc> {
        self.state.lock().unwrap().target_src.clone()
    }

    pub(crate) fn set_target_src(&self, target_src: Option<super::UriDecodePoolSrc>) {
        let mut state = self.state.lock().unwrap();

        if target_src.is_some() {
            state.unused_since = None;
        }
        state.target_src = target_src;
    }

    pub(crate) fn release(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, obj: self.pipeline_ref(), "Releasing pipeline {}", self.name());
        self.set_target_src(None);

        let obj = self.obj().clone();
        self.pipeline_ref().call_async(move |pipeline| {
            let this = obj.imp();
            gst::debug!(CAT, obj: pipeline, "Tearing pipeline down now");
            this.tearing_down.store(true, Ordering::SeqCst);
            let state_lock = this.state_lock.lock();
            this.state.lock().unwrap().stream_selection_seqnum = gst::Seqnum::next();
            if let Err(err) = pipeline.set_state(gst::State::Null) {
                gst::error!(CAT, obj: pipeline, "Could not teardown pipeline {err:?}");
            } else {
                drop(state_lock);

                this.state.lock().unwrap().unused_since = Some(std::time::Instant::now());
                // Pipeline ready to be reused, bring it back to the pool.
                obj.emit_by_name::<()>("released", &[]);
            }
        });
        let mut state = self.state.lock().unwrap();
        state.stream = None;
        drop(state);

        Ok(gst::StateChangeSuccess::Success)
    }

    pub(crate) fn stop(&self) {
        gst::debug!(CAT, obj: self.pipeline_ref(), "Stopping");
        if let Some(sigid) = self.state.lock().unwrap().bus_message_sigid.take() {
            self.pipeline_ref().bus().unwrap().disconnect(sigid);
        }

        let obj = self.obj().clone();
        self.pipeline_ref().call_async(move |pipeline| {
            let this = obj.imp();

            gst::debug!(CAT, obj: pipeline, "Tearing pipeline down now");

            this.tearing_down.store(true, Ordering::SeqCst);
            let _state_lock = this.state_lock.lock();
            if let Err(err) = pipeline.set_state(gst::State::Null) {
                gst::error!(CAT, obj: pipeline, "Could not teardown pipeline {err:?}");
            }

            this.seek_handler().reset(this.pipeline().upcast_ref());
            obj.emit_by_name::<()>("stopped", &[]);
        });
    }
}

#[glib::derived_properties]
impl ObjectImpl for DecoderPipeline {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder("released").build(),
                glib::subclass::Signal::builder("stopped").build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        let pipeline = gst::Pipeline::with_name(&format!("pipeline-{}", self.name()));

        let uridecodebin = gst::ElementFactory::make("uridecodebin3")
            .property("instant-uri", true)
            .property("name", format!("dbin-{}", self.name()))
            .build()
            .expect("Failed to create uridecodebin")
            .downcast::<gst::Bin>()
            .unwrap();

        // FIXME - in multiqueue fix the logic for pushing on unlinked pads
        // so it doesn't wait forever in many of our case.
        // We do not care about unlinked pads streams ourselves!
        {
            for element in uridecodebin
                .iterate_all_by_element_factory_name("multiqueue")
                .into_iter()
                .flatten()
            {
                element.set_property("unlinked-cache-time", gst::ClockTime::from_seconds(86400));
            }

            uridecodebin.connect_deep_element_added(|_, _, element| {
                if element.factory() == gst::ElementFactory::find("multiqueue") {
                    element
                        .set_property("unlinked-cache-time", gst::ClockTime::from_seconds(86400));
                }
            });
        }

        self.seek_handler
            .set(SeekHandler::new(self.name()))
            .unwrap();
        let sink = gst_app::AppSink::builder()
            .name(format!("sink-{}", self.name()))
            .sync(false)
            .enable_last_sample(false)
            .max_buffers(1)
            .buffer_list(false)
            .build();
        pipeline.add(&uridecodebin).unwrap();
        pipeline.add(&sink).unwrap();

        uridecodebin.connect_pad_added(glib::clone!(@weak self as this => move |_, pad| {
                this.pad_added(pad);
        }));
        uridecodebin.connect_pad_removed(glib::clone!(@weak self as this => move |_, pad| {
                this.pad_removed(pad);
        }));

        self.pipeline.set(pipeline).unwrap();
        self.sink.set(sink).unwrap();
        self.uridecodebin.set(uridecodebin.upcast()).unwrap();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for DecoderPipeline {
    const NAME: &'static str = "GstDecoderPipeline";
    type Type = super::DecoderPipeline;
}
