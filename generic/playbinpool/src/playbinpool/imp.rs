// SPDX-License-Identifier: MPL-2.0

use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use gst::glib::once_cell::sync::Lazy;
use gst::glib::Properties;
use gst::glib::{self, ParamSpec, Value};
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::{prelude::*, subclass::prelude::*};

use super::{playbinpool, PooledPlayBin};

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
    #[property(name = "stream-id", get, set, type = String, member = stream_id,
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
            pool: playbinpool::PLAYBIN_POOL.lock().unwrap().clone(),
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
        let fname = format!("{}-{}-{}.dot", COUNTER.fetch_add(1, Ordering::SeqCst), self.obj().name(), pipeline.name());
        let dot_file = DUMPDOT_DIR.as_ref().unwrap().join(&fname);
        let mut file = std::fs::File::create(dot_file.clone()).map_err(|e| {
            gst::error!(CAT, "Could not create dot file: {e:?}");
            e
        }).ok()?;

        file.write_all(gst::debug_bin_to_dot_data(&pipeline, gst::DebugGraphDetails::all()).as_bytes()).map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to write dot file: {e:?}");

            e
        }).ok()?;

        Some(fname)
    }

    fn handle_bus_message(&self, _bus: &gst::Bus, message: &gst::Message) {
        let view = message.view();
        let playbin = {
            let state = self.state.lock().unwrap();

            if state.start_completed {
                return;
            }

            if let Some(ref playbin) = state.playbin {
                playbin.clone()
            } else {
                // We have been disconnected while we already entered the
                // callback it seems
                return;
            }
        };

        match view {
            gst::MessageView::StateChanged(s) => {
                if s.src() == Some(playbin.pipeline().upcast_ref()) {
                    if s.pending() == gst::State::VoidPending {
                        self.state.lock().unwrap().start_completed = true;
                        gst::error!(CAT, imp: self, "--> STARTED {:?}", s.current());
                        self.obj().start_complete(gst::FlowReturn::Ok);
                    }
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
                gst::error!(CAT, imp: self, "No stream selected yet");
                return false;
            }
        }.unwrap();

        playbin.sink().sink_pads().get(0).unwrap().stream_id().map_or(false, |id|
            if id.as_str() != selected_stream.as_str() {
                gst::error!(CAT, imp: self, "(Still?) Using wrong stream {} instead of selected: {} - dropping",
                    id, selected_stream);
                false
            } else {
                true
            }
        )
    }

    fn pull_object(&self, wants_caps: bool) -> Result<gst::MiniObject, gst::FlowError> {
        let playbin = self.state.lock().unwrap().playbin.as_ref().unwrap().clone();
        let sink = playbin.sink();
        let mut seqnum = self.state.lock().unwrap().seek_seqnum.take();

        loop {
            if wants_caps && self.requested_stream_started(&playbin) {
                if let Some(caps) = playbin.sink().sink_pads().get(0).unwrap().caps() {
                    if seqnum.is_some() {
                        self.state.lock().unwrap().seek_seqnum = seqnum;
                    }
                    break Ok(caps.to_owned().upcast());
                }
            }

            let is_eos = sink.is_eos();
            let obj = match sink.pull_object() {
                Ok(obj) => Ok(obj),
                Err(e) => {
                    // Handle the case where the sink changed it EOS state
                    // between the pull and now
                    if is_eos || sink.is_eos() {
                        if seqnum.is_some() {
                            // FIXME: Do not busy waiting for the FLUSH_STOP!
                            gst::error!(CAT, imp: self, "Got EOS while waiting for FLUSH_STOP");
                            continue;
                        }

                        gst::error!(CAT, imp: self, "Got EOS");
                        return Err(gst::FlowError::Eos);
                    }

                    gst::error!(CAT, imp: self, "Got error: {e:?}");
                    return Err(gst::FlowError::Error);
                }
            }?;

            if !self.requested_stream_started(&playbin) {
                gst::error!(CAT, imp: self, "Got {obj:?} from wrong stream, dropping");
                continue;
            }

            if obj.type_().is_a(gst::Event::static_type()) {
                let event = obj.downcast_ref::<gst::Event>().unwrap();
                match event.view() {
                    // The segment we send needs to be exactly the one flowing
                    // in the "upstream pipeline"
                    gst::EventView::Segment(s) => {
                        let mut state = self.state.lock().unwrap();
                        let segment = s.segment();
                        let segment_changed = state.segment.as_ref().clone() != Some(segment) && seqnum.is_none();
                        if segment_changed  {
                            state.segment = Some(segment.to_owned());

                            // Working around the base class setting a new
                            // seqnum on the segment while in our case we want
                            // to ensure seqnums between pipelines match
                            state.segment_seqnum = Some(event.seqnum());
                            drop(state);

                            gst::error!(CAT, imp: self, "Sending segment: {:?}", segment);
                            if let Err(e) = self.obj().new_segment(s.segment()) {
                                gst::error!(CAT, imp: self, "Failed to push segment {e:?}");

                                return Err(gst::FlowError::Error)
                            }
                            gst::error!(CAT, imp: self, "Done sending segment");
                        }
                    },
                    gst::EventView::Caps(c) => {
                        gst::error!(CAT, imp: self, "Got caps: {:?}", c.caps());
                        if wants_caps {
                            if seqnum.is_some() {
                                self.state.lock().unwrap().seek_seqnum = seqnum;
                            }
                            break Ok(c.caps().to_owned().upcast());
                        }
                    },
                    gst::EventView::FlushStop(_) => {
                        if let Some(seq) = seqnum {
                            if event.seqnum() == seq {
                                gst::error!(CAT, imp: self, "Got FLUSH_STOP with right seqnum, restarting pushing buffers");
                                let _ = seqnum.take();
                            } else {
                                gst::error!(CAT, imp: self, "Got FLUSH_STOP with wrong seqnum");
                            }
                        }
                    }
                    _ => ()
                }
            } else if obj.type_().is_a(gst::Sample::static_type()) {
                if wants_caps {
                    gst::error!(CAT, imp: self, "Got sample while waiting for caps, dropping");
                    continue;
                }

                if seqnum.is_some() {
                    gst::error!(CAT, imp: self, "Got sample while waiting for FLUSH_STOP, dropping");

                    continue;
                }

                break Ok(obj)
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
                            gst::error!(CAT, imp: this, "StreamStart event without stream");
                            return gst::PadProbeReturn::Ok;
                        };

                        let stream_id = stream.stream_id().unwrap();
                        gst::error!(CAT, imp: this, "{:?} ++++> Got stream: {:?} {}", playbin, stream.stream_type(), stream_id);

                        let settings = this.settings.lock().unwrap();
                        let mut event_builder = gst::event::StreamStart::builder(
                                settings.stream_id.as_ref().map_or_else(|| stream_id.as_str(), |id| {
                                    let pipeline = playbin.pipeline();
                                    gst::debug_bin_to_dot_file_with_ts(&pipeline, gst::DebugGraphDetails::all(), &format!("{}-wrong-stream-id", this.obj().name()));
                                    if id.as_str() != stream_id.as_str() {
                                        gst::error!(CAT, imp: this, "Selected wrong stream ID {}, {} could probably not be found \
                                            FAKING selected stream ID", stream_id, id)
                                    }

                                    id.as_str()
                                })
                            )
                            .flags(stream.stream_flags())
                            .stream(stream.clone());

                        if let Some(group_id) = s.group_id() {
                            event_builder = event_builder.group_id(group_id);
                        }

                        probe_info.data = Some(gst::PadProbeData::Event(event_builder.build()));
                    },
                    gst::EventView::Segment(s) => {
                        gst::error!(CAT, imp: this, "Got segment {s:?}");
                        if let Some(seqnum) = this.state.lock().unwrap().segment_seqnum.take() {
                            let segment = s.segment();
                            probe_info.data = Some(gst::PadProbeData::Event(
                                gst::event::Segment::builder(segment)
                                    .seqnum(seqnum)
                                    .running_time_offset(event.running_time_offset())
                                    .build()));
                        } else {
                            gst::error!(CAT, imp: this, "Trying to push a segment before we received one, dropping it");

                            return gst::PadProbeReturn::Drop;
                        }
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

    fn do_seek(&self, _segment: &mut gst::Segment) -> bool {
        // Revert what gst_segment_do_seek does.
        let mut state = self.state.lock().unwrap();
        let seek_event = if let Some(seek_event) = state.seek_event.take() {
            seek_event
        } else {
            gst::error!(CAT,  imp: self, "Ignoring initial seek");

            return true;
        };
        let playbin = state.playbin.clone();
        drop (state);

        if let Some(playbin) = playbin {
            let pipeline = playbin.pipeline();

            gst::error!(CAT, imp: self, "Sending {seek_event:?} to {}", pipeline.name());
            if !pipeline.send_event(seek_event) {
                gst::error!(CAT, imp: self, "Failed to seek");
                return false;
            }
            true
        } else {
            gst::error!(CAT, imp: self, "No pipeline to seek");

            false
        }

    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::error!(CAT, imp: self, "STARTING!!");

        let settings = self.settings.lock().unwrap();
        let playbin = if let Some(ref uri) = settings.uri.clone() {
            let stream_id = settings.stream_id.clone();
            let stream_type = settings.stream_type;
            drop(settings);

            self.pool.get_playbin(uri.as_str(), stream_type, stream_id.as_ref().map(|id| id.as_str()))
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
        if res == gst::StateChangeSuccess::Success
        {
            gst::error!(CAT, imp: self, "PLAYING!!");
            self.obj().start_complete(gst::FlowReturn::Ok);
        } else {
            let settings = self.settings.lock().unwrap();
            gst::error!(CAT, imp: self, "{:?} - {:?} Waiting {} state to be reached after {res:?}", settings.stream_id, settings.stream_type, playbin.imp().name());
        }

        Ok(())
    }

    fn negotiate(&self) -> Result<(), gst::LoggableError> {
        gst::info!(CAT, imp: self, "Caps changed, renegotiating");

        let caps = self.pull_object(true).map_or_else(
            |e| Err(gst::loggable_error!(
                CAT,
                format!("No caps on appsink after prerolling {e:?}")
            )),
            |caps| Ok(caps.downcast::<gst::Caps>().unwrap()),
        )?;

        gst::error!(CAT, imp: self, "Negotiated caps: {:?}", caps);
        self.obj()
            .set_caps(&caps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate caps",))
    }

    fn event(&self, event: &gst::Event) -> bool {
        match event.view() {
            gst::EventView::Seek(s) => {
                gst::error!(CAT, imp: self,"---> SEEKING!");

                if s.get().1.contains(gst::SeekFlags::FLUSH){
                    gst::error!(CAT, imp: self, "Flushing seek... waiting for flush-stop with right seqnum restarting pushing buffers");
                    self.state.lock().unwrap().seek_seqnum = Some(event.seqnum());
                }
                self.state.lock().unwrap().seek_event = Some(event.clone())
            },
            _ => (),
        }

        return self.parent_event(event);
    }

    fn create(
        &self,
        _offset: u64,
        _buffer: Option<&mut gst::BufferRef>,
        _length: u32,
    ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
        let sample = self.pull_object(false)?.downcast::<gst::Sample>().expect("Should always have a sample when not waiting for EOS");

        if let Some(buffer) = sample.buffer_owned() {
            gst::error!(CAT, imp: self, "Pushing buffer: {:?}", buffer);
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
        gst::error!(CAT, imp: self, "STOPPING");
        let pipeline = {
            let mut state = self.state.lock().unwrap();

            state.segment = None;
            state.start_completed = false;
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
            pipeline
                .bus()
                .unwrap()
                .disable_sync_message_emission();

            state.start_completed = false;
            state.playbin.take().unwrap()
        };
        gst::error!(CAT, imp: self, "Releasing {pipeline:?}");
        self.pool.release(pipeline);

        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Uri(q) => {
                q.set_uri(self.settings.lock().unwrap().uri.as_ref());
            },
            gst::QueryViewMut::Duration(_) => {
                if let Some(playbin) = self.playbin() {
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
