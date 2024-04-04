// SPDX-License-Identifier: MPL-2.0

use futures::prelude::*;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            uri: None,
            caps: gst::Caps::builder_full()
                .structure_with_any_features(gst::Structure::new_empty("video/x-raw"))
                .build(),
            stream_id: None,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    decoderpipe: Option<DecoderPipeline>,
    bus_message_sigid: Option<glib::SignalHandlerId>,
    source_setup_sigid: Option<glib::SignalHandlerId>,

    segment: Option<gst::Segment>,
    seek_event: Option<gst::Event>,
    flushing: bool,
    seek_seqnum: Option<gst::Seqnum>,
    segment_seqnum: Option<gst::Seqnum>,
    needs_segment: bool,
    seek_segment: Option<gst::Segment>,
}

#[derive(Properties, Debug)]
#[properties(wrapper_type = super::UriDecodePoolSrc)]
pub struct UriDecodePoolSrc {
    #[property(name="uri", get, set, type = String, member = uri, blurb = "The URI to play")]
    #[property(name = "caps", get, set, type = gst::Caps, member = caps,
        blurb = "The caps of the stream to target"
    )]
    #[property(name = "stream-id", get, set, type = Option<String>, member = stream_id,
        flags = glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
        blurb = "The stream-id of the stream to be used"
    )]
    settings: Mutex<Settings>,

    /// This is a hack to be able to generate a dot file of the underlying pipeline
    /// when this element is being dumped. It respects the GST_DEBUG_DUMP_DOT_DIR
    /// environment variable and the file will be generated in that directory.
    #[property(name="pipeline-dot",
        get = Self::dot_pipeline,
        type = Option<String>,
        blurb = "Generate a dot file of the underlying pipeline and return its file path")
    ]
    state: Mutex<State>,
    start_completed: Mutex<bool>,

    pool: super::PlaybinPool,
}

impl Default for UriDecodePoolSrc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            start_completed: Default::default(),
            pool: pool::PLAYBIN_POOL.lock().unwrap().clone(),
        }
    }
}

static START_TIME: Lazy<gst::ClockTime> = Lazy::new(|| gst::get_timestamp());

static DUMPDOT_DIR: Lazy<Option<Box<PathBuf>>> = Lazy::new(|| {
    if let Ok(dotdir) = std::env::var("GST_DEBUG_DUMP_DOT_DIR") {
        let path = Path::new(&dotdir);
        if path.exists() && path.is_dir() {
            return Some(Box::new(path.to_owned()));
        }
    }

    None
});

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "uridecodepoolsrc",
        gst::DebugColorFlags::empty(),
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
                        {
                            *start_completed = true;
                            obj.start_complete(gst::FlowReturn::Ok);
                        }
                    }
                    gst::MessageView::Error(s) => {
                        obj.imp().decoderpipe().map(|p| {
                            p.pipeline().debug_to_dot_file_with_ts(
                                gst::DebugGraphDetails::all(),
                                format!("{}-error", obj.name()),
                            )
                        });
                        gst::error!(CAT, obj: obj, "Got error message: {s} from {decoderpipe:?}");
                        if let Err(e) = obj.post_message(s.message().to_owned()) {
                            gst::error!(CAT, "Could not post error message: {e:?}");
                        }
                    }
                    _ => (),
                }
            }
        });
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
            .get(0)
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
        let sink_sinkpad = decoderpipe.sink().sink_pads().get(0).unwrap().clone();

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
            let obj = match sink.try_pull_object(gst::ClockTime::from_mseconds(100)) {
                Some(obj) => {
                    gst::log!(CAT, imp: self, "Got object: {:?}", obj);
                    Ok(obj)
                }
                None => {
                    // Handle the case where the sink changed it EOS state
                    // between the pull and now
                    if is_eos || sink.is_eos() {
                        let mut state = self.state.lock().unwrap();
                        if state.seek_seqnum.is_some() {
                            gst::debug!(CAT, imp: self, "Got EOS while waiting for FLUSH_STOP");
                            continue;
                        }

                        if state.needs_segment {
                            gst::debug!(CAT, imp: self, "Needs segment!");
                            let segment =
                                sink
                                    .sink_pads()
                                    .get(0)
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
                                // Ensure we push the right segment
                                state.segment = Some(segment.clone());

                                drop(state);

                                self.obj().push_segment(&segment);
                            } else {
                                gst::warning!(CAT, imp: self, "No segment to push before EOS");
                            }
                        }

                        gst::debug!(CAT, imp: self, "Got EOS");
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
                        gst::log!(CAT, imp: self, "Got tag event, forwarding");

                        self.obj().src_pad().push_event(event.to_owned());
                        None
                    }
                    gst::EventView::FlushStop(_) => {
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
                                gst::info!(CAT, imp: self, "Got FLUSH_STOP with wrong seqnum");
                            }
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
                    gst::log!(CAT, imp: self, "Got caps: {:?}", c.caps());
                    if matches!(event_type, Some(gst::EventType::Caps)) {
                        return return_func(self, c.caps().to_owned().upcast());
                    } else {
                        gst::debug!(CAT, imp: self, "Pushing new caps downstream");
                        self.obj()
                            .upcast_ref::<gst_base::BaseSrc>()
                            .set_caps(&c.caps().to_owned())
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
                            })?;
                    }
                }
            } else if obj.type_().is_a(gst::Sample::static_type()) {
                if event_type.is_some() {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "Got sample while waiting for caps, dropping"
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
    }

    fn decoderpipe(&self) -> Option<DecoderPipeline> {
        self.state.lock().unwrap().decoderpipe.clone()
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

        self.obj().src_pad().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM,
            glib::clone!(@weak self as this => @default-return gst::PadProbeReturn::Ok, move |_, probe_info| {
                let event = match &probe_info.data {
                    Some(gst::PadProbeData::Event(event)) => event,
                    _ => unreachable!(),
                };

                match event.view() {
                    gst::EventView::StreamStart(s) => {
                        let decoderpipe = this.state.lock().unwrap().decoderpipe.as_ref().unwrap().clone();
                        let stream = if let Some (stream) = decoderpipe.stream() {
                            stream
                        } else {
                            gst::info!(CAT, imp: this, "StreamStart event without stream");
                            return gst::PadProbeReturn::Ok;
                        };

                        let stream_id = stream.stream_id().unwrap();
                        gst::debug!(CAT, imp: this, "{:?} ++++> Got stream: {:?} {}", decoderpipe, stream.stream_type(), stream_id);

                        let settings = this.settings.lock().unwrap();
                        let mut event_builder = gst::event::StreamStart::builder(
                                settings.stream_id.as_ref().map_or_else(|| stream_id.as_str(), |id| {
                                    let pipeline = decoderpipe.pipeline();
                                    if id.as_str() != stream_id.as_str() {
                                        pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), format!("{}-wrong-stream-id", this.obj().name()));
                                        gst::info!(CAT, imp: this, "Selected wrong stream ID {}, {} could probably not be found \
                                            FAKING selected stream ID", stream_id, id)
                                    }

                                    id.as_str()
                                })
                            )
                            .flags(stream.stream_flags())
                            .stream(stream);

                        if let Some(group_id) = s.group_id() {
                            event_builder = event_builder.group_id(group_id);
                        }

                        probe_info.data = Some(gst::PadProbeData::Event(event_builder.build()));
                    },
                    gst::EventView::Segment(s) => {
                        gst::log!(CAT, imp: this, "Got segment {s:?}");
                        let mut state = this.state.lock().unwrap();

                        let segment = state.segment.clone();
                        if let Some(segment) = segment {
                            let mut builder = gst::event::Segment::builder(&segment)
                                .running_time_offset(event.running_time_offset())
                                .seqnum(event.seqnum());


                            if let Some(seqnum) = state.segment_seqnum.as_ref() {
                                builder = builder.seqnum(*seqnum);
                            }

                            state.needs_segment = false;
                            probe_info.data = Some(gst::PadProbeData::Event(builder.build()));
                        } else {
                            gst::debug!(CAT, imp: this, "Trying to push a segment before we received one, dropping it.");
                            return gst::PadProbeReturn::Drop
                        };

                    },
                    _ => (),
                }

                gst::PadProbeReturn::Ok
            }));
    }
}

impl GstObjectImpl for UriDecodePoolSrc {}

impl ElementImpl for UriDecodePoolSrc {
    fn send_event(&self, event: gst::Event) -> bool {
        gst::log!(CAT, imp: self, "Got event {event:?}");
        if let gst::EventView::Seek(s) = event.view() {
            gst::info!(CAT, imp: self, "Seeking {s:?}");

            self.state.lock().unwrap().seek_event = Some(event.clone());
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

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Stop flushing!");
        self.state.lock().unwrap().flushing = false;

        Ok(())
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "START flushing!");
        self.state.lock().unwrap().flushing = true;

        Ok(())
    }

    fn do_seek(&self, segment: &mut gst::Segment) -> bool {
        let mut state = self.state.lock().unwrap();
        let seek_event = if let Some(seek_event) = state.seek_event.take() {
            seek_event
        } else {
            gst::debug!(CAT, imp: self, "Ignoring initial seek");

            return true;
        };
        let decoderpipe = state.decoderpipe.clone();
        drop(state);

        if let Some(decoderpipe) = decoderpipe {
            gst::info!(CAT, imp: self, "Seeking to {segment:?}");
            if let gst::EventView::Seek(s) = seek_event.view() {
                let values = s.get();
                if values.1.contains(gst::SeekFlags::FLUSH) {
                    gst::debug!(CAT, imp: self, "Flushing seek... waiting for flush-stop with right seqnum ({:?}) restarting pushing buffers", seek_event.seqnum());
                    let mut state = self.state.lock().unwrap();
                    state.seek_seqnum = Some(seek_event.seqnum());
                    state.segment_seqnum = Some(seek_event.seqnum());
                    state.needs_segment = true;
                    state.seek_segment = Some(segment.clone());
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

            decoderpipe.imp().seek(seek_event);
            true
        } else {
            gst::info!(CAT, imp: self, "No pipeline to seek");

            false
        }
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Starting");

        let has_uri = self.settings.lock().unwrap().uri.is_some();
        let decoderpipe = if has_uri {
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
            *start_completed = true;
            self.obj().start_complete(gst::FlowReturn::Ok);
        } else {
            let settings = self.settings.lock().unwrap();
            gst::debug!(
                CAT,
                imp: self,
                "{:?} - {:?} Waiting {} state to be reached after {res:?}",
                settings.stream_id,
                settings.caps,
                decoderpipe.imp().name()
            );
        }

        Ok(())
    }

    fn negotiate(&self) -> Result<(), gst::LoggableError> {
        gst::info!(CAT, imp: self, "Caps changed, renegotiating");

        let caps = match self.process_objects(Some(gst::EventType::Caps)) {
            Ok(caps) => caps.downcast::<gst::Caps>().unwrap(),
            Err(e) => {
                if e == gst::FlowError::Eos {
                    let decoderpipe = self.decoderpipe().unwrap();
                    let sink = decoderpipe.sink();
                    let sink_pad = sink.static_pad("sink").unwrap();
                    if let Some(caps) = sink_pad.sticky_event::<gst::event::Caps>(0) {
                        caps.caps_owned()
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
        self.obj()
            .upcast_ref::<gst_base::BaseSrc>()
            .set_caps(&caps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate caps",))
    }

    fn event(&self, event: &gst::Event) -> bool {
        if let gst::EventView::Seek(_s) = event.view() {
            gst::debug!(CAT, imp: self, "Seeking");

            self.state.lock().unwrap().seek_event = Some(event.clone())
        }

        self.parent_event(event)
    }

    fn create(
        &self,
        _offset: u64,
        _buffer: Option<&mut gst::BufferRef>,
        _length: u32,
    ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
        let sample = self
            .process_objects(None)?
            .downcast::<gst::Sample>()
            .expect("Should always have a sample when not waiting for EOS");

        if let Some(segment) = sample.segment() {
            let mut state = self.state.lock().unwrap();
            if Some(segment) != state.segment.as_ref() {
                state.segment = Some(segment.to_owned());
                drop(state);
                gst::debug!(CAT, "Pushing segment {segment:?}");

                self.obj().push_segment(segment);
            }
        }

        if let Some(buffer) = sample.buffer_owned() {
            gst::trace!(CAT, imp: self, "Pushing buffer: {:?}", buffer);
            Ok(gst_base::subclass::base_src::CreateSuccess::NewBuffer(
                buffer,
            ))
        } else if let Some(buffer_list) = sample.buffer_list_owned() {
            Ok(gst_base::subclass::base_src::CreateSuccess::NewBufferList(
                buffer_list,
            ))
        } else {
            gst::error!(CAT, imp: self, "Got sample without buffer or buffer list");

            Err(gst::FlowError::Error)
        }
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Stopping");
        let pipeline = {
            let mut state = self.state.lock().unwrap();

            state.segment = None;
            state.seek_seqnum = None;
            state.segment_seqnum = None;
            state.seek_event = None;

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
                if s.structure()
                    .map_or(false, |s| s.name().as_str() == "can-seek-in-null")
                {
                    gst::info!(CAT, "Marking as seekable in NULL");
                    s.structure_mut().set("res", true);

                    return true;
                } else if let Some(decoderpipe) = self.decoderpipe() {
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
