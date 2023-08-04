use std::sync::Mutex;

use gst::{glib, subclass::prelude::*, prelude::*};

use super::playbinpool::CAT;

#[derive(Debug)]
struct State {
    stream: Option<gst::Stream>,
    stream_id: Option<String>,
    stream_type: gst::StreamType,
    unused_since: Option<std::time::Instant>,
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
            .expect("Failed to create uridecodebin")
            .downcast::<gst::Bin>()
            .unwrap();

        uridecodebin.connect_deep_element_added(|_, _, element| {
            if element.factory() == gst::ElementFactory::find("multiqueue") {
                // FIXME - in multiqueue fix the logic for pushing on unlinked pads
                // so it doesn't wait forever in many of our case.
                // We do not care about unlinked pads streams ourselves!
                element.set_property("unlinked-cache-time", gst::ClockTime::from_seconds(86400));
            }
        });

        pipeline.add(&uridecodebin).unwrap();
        let sink = gst_app::AppSink::builder().sync(false).build();
        pipeline.add(&sink).unwrap();


        let name = pipeline.name().to_string();
        let this = Self {
            pipeline: pipeline.clone(),
            sink,
            uridecodebin: uridecodebin.clone().upcast(),
            state: Mutex::new(State {
                unused_since: None,
                stream: None,
                stream_id: None,
                stream_type: gst::StreamType::VIDEO,
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

        self.set_uri(uri);
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

        self.uridecodebin.connect_closure("select-stream", false,
            glib::closure!(@weak-allow-none self as this => move |_db: gst::Element, collection: gst::StreamCollection, stream: gst::Stream| {
                let this = this.unwrap();

                if let Some(ref wanted_stream_id) = this.requested_stream_id() {
                    if stream.stream_id().map_or(false, |sid| sid.as_str() == wanted_stream_id) {
                        gst::error!(CAT, "{:?} Selecting specified stream: {:?}", this.name, wanted_stream_id);

                        this.state.lock().unwrap().stream = Some(stream.clone());
                        return 1 as i32;
                    }

                    if collection.iter().find(|potential_stream| {
                        let stream_id = potential_stream.stream_id();
                        stream_id.map_or(false, |s| wanted_stream_id.as_str() == s.as_str())
                    }).is_some() {
                        gst::error!(CAT, "{:?} Specified stream in collection, ignoring this one {:?}", this.name, wanted_stream_id);

                        return 0;
                    }
                }

                gst::error!(CAT, "Selecting the first stream of type: {:?}", this.stream_type());
                if let Some(first_stream_of_type) = collection.iter().find(|s|
                        s.stream_type() == this.stream_type() && s.stream_id().is_some()
                ) {
                    if stream == first_stream_of_type {
                        gst::error!(CAT, "{:?} Selecting stream: {:?}", this.name, first_stream_of_type.stream_id());

                        this.state.lock().unwrap().stream = Some(stream.clone());
                        return 1 as i32;
                    }

                    gst::error!(CAT, "Waiting to select first stream of type: {:?}", this.stream_type());
                } else {
                    /* FIXME --- Post an error on the bus! */
                    gst::error!(CAT, "{:?} No stream found for type: {:?}", this.name, this.stream_type());
                }

                return 0;
            })
        );

    }
}

#[glib::object_subclass]
impl ObjectSubclass for PooledPlayBin {
    const NAME: &'static str = "GstPooledPlayBin";
    type Type = super::PooledPlayBin;
}
