use std::sync::Mutex;

use gst::{glib, subclass::prelude::*, prelude::*};

use super::playbinpool::CAT;

#[derive(Debug)]
struct State {
    stream: Option<gst::Stream>,
    stream_id: Option<String>,
    stream_type: gst::StreamType,
    unused_since: Option<std::time::Instant>,
    bus_message_sigid: Option<glib::SignalHandlerId>,
}

pub struct PooledPlayBin {
    pub pipeline: gst::Pipeline,
    pub audio_sink: gst_app::AppSink,
    pub video_sink: gst_app::AppSink,
    state: Mutex<State>,
    name: String,
}

impl Default for PooledPlayBin {
    fn default() -> Self {
        gst::error!(CAT, "Creating default playbin");
        let mut pipeline_builder = gst::ElementFactory::make("playbin3")
            .property("instant-uri", true);

        let audio_sink = gst_app::AppSink::builder().build();
        let video_sink = gst_app::AppSink::builder().build();
        pipeline_builder = pipeline_builder
            .property("video-sink", video_sink.clone())
            .property( "audio-sink", audio_sink.clone());
        let pipeline = pipeline_builder
                .build()
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();

        let name = pipeline.name().to_string();
        let this = Self {
            pipeline: pipeline.clone(),
            audio_sink,
            video_sink,
            state: Mutex::new(State {
                unused_since: None,
                stream: None,
                stream_id: None,
                stream_type: gst::StreamType::VIDEO,
                bus_message_sigid: None,
            }),
            name,
        };

        this
    }
}

impl PartialEq for PooledPlayBin {
    fn eq(&self, other: &Self) -> bool {
        self.pipeline == other.pipeline
    }
}

impl core::fmt::Debug for PooledPlayBin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayBin")
            .field("name", &self.name)
            .field("uri", &self.pipeline.property::<Option<String>>("uri"))
            .field("state", &self.state)
            .finish()
    }
}

impl PooledPlayBin {
    pub(crate) fn stream_type(&self) -> gst::StreamType {
        self.state.lock().unwrap().stream_type
    }

    pub(crate) fn stream_id(&self) -> Option<String> {
        self.state.lock().unwrap().stream_id.clone()
    }

    pub(crate) fn video_sink(&self) -> gst_app::AppSink {
        self.video_sink.clone()
    }

    pub(crate) fn audio_sink(&self) -> gst_app::AppSink {
        self.audio_sink.clone()
    }

    pub(crate) fn pipeline(&self) -> gst::Pipeline {
        self.pipeline.clone()
    }

    fn configure_unused_sink(sink: &gst::Element) {
        sink.set_property("enable-last-sample", false);
        sink.set_property("max-buffers", 1u32);
        sink.set_property("drop", true);
    }

    pub(crate) fn reset(&self, uri: &str, stream_type: gst::StreamType, stream_id: Option<&str>) {
        gst::error!(CAT, "BEFORE RESET: {self:?}");
        let mut state = self.state.lock().unwrap();
        state.unused_since = None;
        state.stream = None;
        state.stream_id = stream_id.map(|s| s.to_string());
        state.stream_type = stream_type;
        let bus = self.pipeline.bus().unwrap();
        bus.enable_sync_message_emission();
        state.bus_message_sigid = Some(
            bus.connect_sync_message(None, glib::clone!(@weak self as this => move |_, msg|
                this.handle_bus_message(msg)
            )
        ));

        self.set_uri(uri);

        gst::error!(CAT, "{self:?} reset");

        Self::configure_unused_sink(&if stream_type == gst::StreamType::VIDEO {
            self.pipeline.property::<gst::Element>("audio-sink")
        } else {
            self.pipeline.property::<gst::Element>("video-sink")
        });
    }

    fn handle_bus_message(&self, message: &gst::Message) {
        let view = message.view();

        match view {
            gst::MessageView::StreamCollection(s) => {
                let collection = s.stream_collection();

                gst::debug!(CAT, "{self:?} Got collection {collection:?}");
                let stream = if let Some(ref wanted_stream_id) = self.stream_id() {
                    if let Some(stream) = collection.iter().find(|stream| {
                        let stream_id = stream.stream_id();
                        stream_id.map_or(false, |s| wanted_stream_id.as_str() == s.as_str())
                    }) {
                        gst::debug!(CAT, "{self:?} Selecting specified stream: {:?}", wanted_stream_id);

                        Some(stream)
                    } else {
                        gst::warning!(CAT, "{self:?} Stream not found: {}", wanted_stream_id);

                        None
                    }
                } else {
                    None
                };

                let stream = if let Some(stream) = stream {
                    stream
                } else {
                    if let Some(stream) = collection.iter().find(|stream|
                            stream.stream_type() == self.stream_type() && stream.stream_id().is_some()
                    ) {
                        gst::debug!(CAT, "{self:?} Selecting stream: {:?}", stream.stream_id());
                        stream
                    } else {
                        /* FIXME --- Post an error on the bus! */
                        gst::error!(CAT, "{self:?} No stream found for type: {:?}", self.stream_type());

                        return;
                    }
                };

                let _ = self.state.lock().unwrap().stream.insert(stream.clone());
                self.pipeline.send_event(gst::event::SelectStreams::new(&[stream.stream_id().unwrap().as_str()]));
            }
            _ => (),
        }
    }

    pub(crate) fn unused_since(&self) -> Option<std::time::Instant> {
        self.state.lock().unwrap().unused_since.clone()
    }

    pub(crate) fn stream(&self) -> Option<gst::Stream> {
        self.state.lock().unwrap().stream.clone()
    }

    fn set_uri(&self, uri: &str) {
        self.pipeline.set_property("uri", uri);
    }

    pub(crate) fn play(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        self.pipeline.set_state(gst::State::Playing)
    }

    pub(crate) fn release(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        self.pipeline.set_state(gst::State::Null)?;
        let mut state = self.state.lock().unwrap();
        state.stream = None;
        drop(state);

        gst::debug!(CAT, "Releasing pipeline {self:?}");
        self.pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::ALL, "releasing");
        Ok(gst::StateChangeSuccess::Success)
    }

    pub(crate) fn stop(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if let Some(sigid) = self.state.lock().unwrap().bus_message_sigid.take() {
            self.pipeline.bus().unwrap().disconnect(sigid);
        }

        self.pipeline.set_state(gst::State::Null)
    }

    pub(crate) fn set_unused(&self) {
        self.state.lock().unwrap().unused_since = Some(std::time::Instant::now());
    }

    pub(crate) fn is_seekable(&self) -> bool {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        if self.pipeline.query(query.query_mut()) {
            query.result().0
        } else {
            gst::warning!(CAT, "Failed to query seekability of playbin");
            false
        }
    }
}

impl ObjectImpl for PooledPlayBin { }

#[glib::object_subclass]
impl ObjectSubclass for PooledPlayBin {
    const NAME: &'static str = "GstPooledPlayBin";
    type Type = super::PooledPlayBin;
}
