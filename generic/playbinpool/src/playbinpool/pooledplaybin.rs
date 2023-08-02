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
    pub uridecodebin: gst::Element,
    pub sink: gst_app::AppSink,
    state: Mutex<State>,
    name: String,
}

impl Default for PooledPlayBin {
    fn default() -> Self {
        gst::error!(CAT, "Creating default playbin");
        let pipeline = gst::Pipeline::new();

        let uridecodebin = gst::ElementFactory::make("uridecodebin3")
            .property("instant-uri", true)
            .build()
            .expect("Failed to create uridecodebin");

        pipeline.add(&uridecodebin).unwrap();
        let sink = gst_app::AppSink::builder().sync(false).build();
        pipeline.add(&sink).unwrap();


        let name = pipeline.name().to_string();
        let this = Self {
            pipeline: pipeline.clone(),
            sink,
            uridecodebin: uridecodebin.clone(),
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

impl PooledPlayBin {
    fn pad_added(&self, pad: &gst::Pad) {
        gst::error!(CAT, "Pad added: {:?}", pad);
        let sinkpad = self.sink.static_pad("sink").unwrap();
        if sinkpad.is_linked() {
            gst::error!(CAT, "Pad already linked");
            return;
        }

        if let Err(err) = pad.link(&sinkpad) {
            gst::error!(CAT, "Failed to link pads: {:?}", err);
        }
    }

    pub(crate) fn stream_type(&self) -> gst::StreamType {
        self.state.lock().unwrap().stream_type
    }

    pub(crate) fn requested_stream_id(&self) -> Option<String> {
        self.state.lock().unwrap().stream_id.clone()
    }

    pub(crate) fn sink(&self) -> gst_app::AppSink {
        self.sink.clone()
    }

    pub(crate) fn pipeline(&self) -> gst::Pipeline {
        self.pipeline.clone()
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn reset(&self, uri: &str, stream_type: gst::StreamType, stream_id: Option<&str>) {
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
    }

    fn handle_bus_message(&self, message: &gst::Message) {
        let view = message.view();

        match view {
            gst::MessageView::StreamCollection(s) => {
                let collection = s.stream_collection();

                let stream = if let Some(ref wanted_stream_id) = self.requested_stream_id() {
                    if let Some(stream) = collection.iter().find(|stream| {
                        let stream_id = stream.stream_id();
                        stream_id.map_or(false, |s| wanted_stream_id.as_str() == s.as_str())
                    }) {
                        gst::error!(CAT, "{:?} Selecting specified stream: {:?}", self.name, wanted_stream_id);

                        Some(stream)
                    } else {
                        gst::error!(CAT, "{:?} requested stream {} not found in {}", self.name, wanted_stream_id, self.uridecodebin().property::<String>("uri"));

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
                        gst::error!(CAT, "{:?} Selecting stream: {:?}", self.name, stream.stream_id());
                        stream
                    } else {
                        /* FIXME --- Post an error on the bus! */
                        gst::error!(CAT, "{:?} No stream found for type: {:?}", self.name, self.stream_type());

                        return;
                    }
                };

                let _ = self.state.lock().unwrap().stream.insert(stream.clone());
                let uridecodebin = self.uridecodebin();
                message.src().unwrap_or_else(|| uridecodebin.upcast_ref::<gst::Object>()).downcast_ref::<gst::Element>().unwrap().send_event(gst::event::SelectStreams::new(&[stream.stream_id().unwrap().as_str()]));
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

    pub(crate) fn uridecodebin(&self) -> gst::Element {
        self.uridecodebin.clone()
    }

    fn set_uri(&self, uri: &str) {
        self.uridecodebin.set_property("uri", uri);
    }

    pub(crate) fn play(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        self.pipeline.set_state(gst::State::Playing)
    }

    pub(crate) fn release(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        self.pipeline.set_state(gst::State::Null)?;
        let mut state = self.state.lock().unwrap();
        state.stream = None;
        drop(state);

        gst::debug!(CAT, "Releasing pipeline {}", self.name);
        self.pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::ALL, "releasing");
        Ok(gst::StateChangeSuccess::Success)
    }

    pub(crate) fn stop(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::error!(CAT, imp: self, "----> STOPPING < ------------------------");
        if let Some(sigid) = self.state.lock().unwrap().bus_message_sigid.take() {
            self.pipeline.bus().unwrap().disconnect(sigid);
        }

        self.pipeline.set_state(gst::State::Null)
    }

    pub(crate) fn set_unused(&self) {
        self.state.lock().unwrap().unused_since = Some(std::time::Instant::now());
    }
}

impl ObjectImpl for PooledPlayBin {
    fn constructed(&self) {
        self.uridecodebin.connect_pad_added(
            glib::clone!(@weak self as this => move |_, pad| {
                this.pad_added(pad);
        }));
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PooledPlayBin {
    const NAME: &'static str = "GstPooledPlayBin";
    type Type = super::PooledPlayBin;
}
