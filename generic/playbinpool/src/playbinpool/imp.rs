// SPDX-License-Identifier: MPL-2.0

use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use gst::glib::once_cell::sync::Lazy;
use gst::glib::Properties;
use gst::glib::{self, ParamSpec, Value};
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::{prelude::*, subclass::prelude::*};

use super::{pool, PooledPlayBin};

#[derive(Debug)]
struct Settings {
    uri: Option<String>,
    stream_type: gst::StreamType,
    stream_id: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            uri: None,
            stream_type: gst::StreamType::VIDEO,
            stream_id: None,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    playbin: Option<PooledPlayBin>,
    bus_message_sigid: Option<glib::SignalHandlerId>,
    source_setup_sigid: Option<glib::SignalHandlerId>,
    start_completed: bool,

    segment: Option<gst::Segment>,
    seek_event: Option<gst::Event>,
    flushing: bool,
    seek_seqnum: Option<gst::Seqnum>,
    segment_seqnum: Option<gst::Seqnum>,
}

#[derive(Properties, Debug)]
#[properties(wrapper_type = super::PlaybinPoolSrc)]
pub struct PlaybinPoolSrc {
    #[property(name="uri", get, set, type = String, member = uri, blurb = "The URI to play")]
    #[property(name = "stream-type", get, set, type = gst::StreamType, member = stream_type,
        blurb = "The type of stream to be used, this is only used of no `stream-id` is specified"
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

    pool: super::PlaybinPool,
}

impl Default for PlaybinPoolSrc {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            pool: pool::PLAYBIN_POOL.lock().unwrap().clone(),
        }
    }
}

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
        "playbinpoolsrc",
        gst::DebugColorFlags::empty(),
        Some("Playbin Pool Src"),
    )
});

impl PlaybinPoolSrc {
    fn dot_pipeline(&self) -> Option<String> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        if DUMPDOT_DIR.is_none() {
            return None;
        }

        let playbin = match self.state.lock().unwrap().playbin.as_ref() {
            Some(playbin) => playbin.clone(),
            None => {
                gst::info!(CAT, imp: self, "No playbin to dump");
                return None;
            }
        };

        let pipeline = playbin.pipeline();
        let fname = format!(
            "{}-{}-{}.dot",
            COUNTER.fetch_add(1, Ordering::SeqCst),
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
            gst::debug_bin_to_dot_data(&pipeline, gst::DebugGraphDetails::all()).as_bytes(),
        )
        .map_err(|e| {
            gst::warning!(CAT, imp: self, "Failed to write dot file: {e:?}");

            e
        })
        .ok()?;

        Some(fname)
    }

    fn handle_bus_message(&self, _bus: &gst::Bus, message: &gst::Message) {
        let view = message.view();
        let (playbin, start_completed) = {
            let state = self.state.lock().unwrap();

            if let Some(ref playbin) = state.playbin {
                (playbin.clone(), state.start_completed)
            } else {
                gst::debug!(CAT, imp: self, "Got message {:?} without playbin", message);
                // We have been disconnected while we already entered the
                // callback it seems
                return;
            }
        };

        match view {
            gst::MessageView::Element(s) => {
                if let Err(e) = self.obj().post_message(s.message().to_owned()) {
                    gst::warning!(CAT, imp: self, "Failed to forward message: {e:?}");
                }
            }
            gst::MessageView::StateChanged(s) => {
                if !start_completed
                    && s.src() == Some(playbin.pipeline().upcast_ref())
                    && s.pending() == gst::State::VoidPending
                {
                    self.state.lock().unwrap().start_completed = true;
                    self.obj().start_complete(gst::FlowReturn::Ok);
                }
            }
            _ => (),
        }
    }

    /// Ensures that a `stream-start` event with the right ID has been received
    /// already
    fn requested_stream_started(&self, playbin: &PooledPlayBin) -> bool {
        let selected_stream = match playbin.stream() {
            Some(stream) => stream.stream_id(),
            None => {
                gst::info!(CAT, imp: self, "No stream selected yet");
                return false;
            }
        }
        .unwrap();

        playbin
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

        let playbin = self.state.lock().unwrap().playbin.as_ref().unwrap().clone();
        let sink = playbin.sink();
        let sink_sinkpad = playbin.sink().sink_pads().get(0).unwrap().clone();

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

            if self.requested_stream_started(&playbin)
                && self.state.lock().unwrap().seek_seqnum.is_none()
            {
                match event_type {
                    Some(gst::EventType::Caps) => {
                        if let Some(caps) = sink_sinkpad.caps() {
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
                        if self.state.lock().unwrap().seek_seqnum.is_some() {
                            gst::debug!(CAT, imp: self, "Got EOS while waiting for FLUSH_STOP");
                            continue;
                        }

                        gst::debug!(CAT, imp: self, "Got EOS");
                        return Err(gst::FlowError::Eos);
                    }

                    if self.state.lock().unwrap().flushing {
                        gst::debug!(
                            CAT,
                            imp: self,
                            "Flushing"
                        );
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
                                    "Got FLUSH_STOP with right seqnum, restarting pushing buffers"
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

            if !self.requested_stream_started(&playbin) {
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
                        self.obj().set_caps(&c.caps().to_owned()).map_err(|e| {
                            gst::error!(CAT, "Could not set caps: {e:?}");
                            if self.obj().src_pad().pad_flags().contains(gst::PadFlags::FLUSHING) {
                                gst::FlowError::Flushing
                            } else {
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

    fn set_playbin(&self, playbin: &PooledPlayBin) {
        let pipeline = playbin.pipeline();
        let bus = pipeline.bus().unwrap();
        bus.enable_sync_message_emission();
        let mut state = self.state.lock().unwrap();
        state.bus_message_sigid = Some(bus.connect_sync_message(None,
            glib::clone!(@weak self as this => move |bus, message| this.handle_bus_message(bus, message)))
        );

        let obj = self.obj();
        state.source_setup_sigid = Some(playbin.uridecodebin().connect_closure(
            "source-setup",
            false,
            glib::closure!(
                @watch obj => move |_playbin: gst::Element, source: gst::Element| {
                    obj.emit_by_name::<()>("source-setup", &[&source]);
                }
            ),
        ));

        state.playbin = Some(playbin.clone());
    }

    fn playbin(&self) -> Option<PooledPlayBin> {
        self.state.lock().unwrap().playbin.clone()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PlaybinPoolSrc {
    const NAME: &'static str = "GstPlaybinPoolSrc";
    type Type = super::PlaybinPoolSrc;
    type ParentType = gst_base::BaseSrc;

    type Interfaces = (gst::ChildProxy,);
}

impl ObjectImpl for PlaybinPoolSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        Self::derived_properties()
    }

    fn set_property(&self, id: usize, value: &Value, pspec: &ParamSpec) {
        self.derived_set_property(id, value, pspec)
    }

    fn property(&self, id: usize, pspec: &glib::ParamSpec) -> Value {
        self.derived_property(id, pspec)
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                /**
                 * playbinpoolsrc::source-setup:
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
                        let playbin = this.state.lock().unwrap().playbin.as_ref().unwrap().clone();
                        let stream = if let Some (stream) = playbin.stream() {
                            stream
                        } else {
                            gst::info!(CAT, imp: this, "StreamStart event without stream");
                            return gst::PadProbeReturn::Ok;
                        };

                        let stream_id = stream.stream_id().unwrap();
                        gst::debug!(CAT, imp: this, "{:?} ++++> Got stream: {:?} {}", playbin, stream.stream_type(), stream_id);

                        let settings = this.settings.lock().unwrap();
                        let mut event_builder = gst::event::StreamStart::builder(
                                settings.stream_id.as_ref().map_or_else(|| stream_id.as_str(), |id| {
                                    let pipeline = playbin.pipeline();
                                    if id.as_str() != stream_id.as_str() {
                                        gst::debug_bin_to_dot_file_with_ts(&pipeline, gst::DebugGraphDetails::all(), format!("{}-wrong-stream-id", this.obj().name()));
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
                        let state = this.state.lock().unwrap();

                        if let Some(segment) = state.segment.as_ref().clone() {
                            let mut builder = gst::event::Segment::builder(segment)
                                .running_time_offset(event.running_time_offset());

                            if let Some(seqnum) = state.segment_seqnum.as_ref() {
                                builder = builder.seqnum(*seqnum);
                            }

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

        self.parent_constructed();
    }
}

impl GstObjectImpl for PlaybinPoolSrc {}

impl ElementImpl for PlaybinPoolSrc {
    fn send_event(&self, event: gst::Event) -> bool {
        gst::error!(CAT, imp: self, "Got event {event:?}");
        if let gst::EventView::Seek(s) = event.view() {
            gst::error!(CAT, imp: self, "Seeking {s:?}");

            self.state.lock().unwrap().seek_event = Some(event.clone());
        }

        self.parent_send_event(event)
    }

    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Source",
                "Source",
                "Playback source which runs a pool of playbin instances",
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

impl BaseSrcImpl for PlaybinPoolSrc {
    fn is_seekable(&self) -> bool {
        gst::fixme!(CAT, imp: self, "Handle not seekable underlying pipelines");
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
        let playbin = state.playbin.clone();
        drop(state);

        if let Some(playbin) = playbin {
            let pipeline = playbin.pipeline();

            gst::info!(CAT, imp: self, "Seeking to {segment:?}");
            if let gst::EventView::Seek(s) = seek_event.view() {
                let values = s.get();
                if values.1.contains(gst::SeekFlags::FLUSH) {
                    gst::debug!(CAT, imp: self, "Flushing seek... waiting for flush-stop with right seqnum restarting pushing buffers");
                    self.state.lock().unwrap().seek_seqnum = Some(seek_event.seqnum());
                    self.state.lock().unwrap().segment_seqnum = Some(seek_event.seqnum());
                }

                values
            } else {
                unreachable!();
            };

            gst::debug!(
                CAT,
                imp: self,
                "Sending {seek_event:?} to {}",
                pipeline.name()
            );
            if !pipeline.send_event(seek_event) {
                gst::error!(CAT, imp: self, "Failed to seek");
                return false;
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
        let playbin = if has_uri {
            self.pool.get_playbin(&self.obj())
        } else {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                ("No URI provided")
            ));
        };

        self.set_playbin(&playbin);
        let res = playbin.imp().play().map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ("Failed to set underlying pipeline to PLAYING: {err:?}")
            )
        })?;
        if res == gst::StateChangeSuccess::Success {
            gst::debug!(CAT, imp: self, "Already ready");
            self.state.lock().unwrap().start_completed = true;
            self.obj().start_complete(gst::FlowReturn::Ok);
        } else {
            let settings = self.settings.lock().unwrap();
            gst::debug!(
                CAT,
                imp: self,
                "{:?} - {:?} Waiting {} state to be reached after {res:?}",
                settings.stream_id,
                settings.stream_type,
                playbin.imp().name()
            );
        }

        Ok(())
    }

    fn negotiate(&self) -> Result<(), gst::LoggableError> {
        gst::info!(CAT, imp: self, "Caps changed, renegotiating");

        let caps = self
            .process_objects(Some(gst::EventType::Caps))
            .map_or_else(
                |e| {
                    Err(gst::loggable_error!(
                        CAT,
                        format!("No caps on appsink after prerolling {e:?}")
                    ))
                },
                |caps| Ok(caps.downcast::<gst::Caps>().unwrap()),
            )?;

        gst::debug!(CAT, imp: self, "Negotiated caps: {:?}", caps);
        self.obj()
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

            let playbin = state.playbin.as_ref().unwrap().clone();
            let pipeline = playbin.pipeline();
            if let Some(sigid) = state.bus_message_sigid.take() {
                pipeline.bus().unwrap().disconnect(sigid);
            }
            if let Some(sigid) = state.source_setup_sigid.take() {
                playbin.uridecodebin().disconnect(sigid);
            }
            pipeline.bus().unwrap().disable_sync_message_emission();

            state.start_completed = false;
            state.playbin.take().unwrap()
        };
        gst::info!(CAT, imp: self, "Releasing {pipeline:?}");
        self.pool.release(pipeline);

        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Uri(q) => {
                q.set_uri(self.settings.lock().unwrap().uri.as_ref());
            }
            gst::QueryViewMut::Duration(_) => {
                if let Some(playbin) = self.playbin() {
                    return playbin.pipeline().query(query);
                }
            }
            gst::QueryViewMut::Custom(s) => {
                if s.structure().map_or(false, |s| s.name().as_str() == "can-seek-in-null") {
                    gst::error!(CAT, "Can seek in NULL");
                    s.structure_mut().set("res", true);

                    return true;
                } else if let Some(playbin) = self.playbin() {
                    return playbin.pipeline().query(query);
                }
            }
            _ => (),
        }

        BaseSrcImplExt::parent_query(self, query)
    }
}

impl ChildProxyImpl for PlaybinPoolSrc {
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
