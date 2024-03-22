use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex,
};

use gst::{glib, prelude::*, subclass::prelude::*};
use once_cell::sync::Lazy;

use super::pool::CAT;

#[derive(Debug)]
struct State {
    stream: Option<gst::Stream>,
    stream_id: Option<String>,
    stream_type: gst::StreamType,
    unused_since: Option<std::time::Instant>,
    bus_message_sigid: Option<glib::SignalHandlerId>,

    target_src: Option<super::PlaybinPoolSrc>,
}

pub struct PooledPlayBin {
    pub pipeline: gst::Pipeline,
    pub uridecodebin: gst::Element,
    pub sink: gst_app::AppSink,
    state: Mutex<State>,
    // Working around https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/150 by
    // ensuring we do not send `SELECT_STREAM` while tearing down
    state_lock: Mutex<bool>,
    name: String,
}

impl Default for PooledPlayBin {
    fn default() -> Self {
        static N_PIPELINES: AtomicU32 = AtomicU32::new(0);
        let pipeline = gst::ElementFactory::make("pipeline")
            .name(&format!(
                "pooledpipeline-{}",
                N_PIPELINES.fetch_add(1, Ordering::SeqCst)
            ))
            .build()
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();

        let uridecodebin = gst::ElementFactory::make("uridecodebin3")
            .property("instant-uri", true)
            .build()
            .expect("Failed to create uridecodebin")
            .downcast::<gst::Bin>()
            .unwrap();

        // FIXME - in multiqueue fix the logic for pushing on unlinked pads
        // so it doesn't wait forever in many of our case.
        // We do not care about unlinked pads streams ourselves!
        {
            for element in uridecodebin.iterate_all_by_element_factory_name("multiqueue") {
                if let Ok(element) = element {
                    element
                        .set_property("unlinked-cache-time", gst::ClockTime::from_seconds(86400));
                }
            }

            uridecodebin.connect_deep_element_added(|_, _, element| {
                if element.factory() == gst::ElementFactory::find("multiqueue") {
                    element
                        .set_property("unlinked-cache-time", gst::ClockTime::from_seconds(86400));
                }
            });
        }

        pipeline.add(&uridecodebin).unwrap();
        let sink = gst_app::AppSink::builder()
            .sync(false)
            .enable_last_sample(false)
            .max_buffers(1)
            .build();
        pipeline.add(&sink).unwrap();

        let name = pipeline.name().to_string();

        Self {
            pipeline,
            sink,
            uridecodebin: uridecodebin.upcast(),
            state: Mutex::new(State {
                unused_since: None,
                stream: None,
                stream_id: None,
                stream_type: gst::StreamType::VIDEO,
                bus_message_sigid: None,
                target_src: None,
            }),
            state_lock: Mutex::new(false),
            name,
        }
    }
}

impl PartialEq for PooledPlayBin {
    fn eq(&self, other: &Self) -> bool {
        self.pipeline == other.pipeline
    }
}

impl PooledPlayBin {
    fn pad_removed(&self, _pad: &gst::Pad) {
        let sinkpad = self.sink.static_pad("sink").unwrap();

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
        let sinkpad = self.sink.static_pad("sink").unwrap();
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

        if self.stream_type() == gst::StreamType::VIDEO {
            let videorate = gst::ElementFactory::make("videorate").build().unwrap();

            if let Err(err) = self.pipeline().add(&videorate) {
                gst::error!(CAT, imp: self, "Failed to add videorate: {:?}", err);
                return;
            }

            videorate.sync_state_with_parent().unwrap();

            let videorate_sinkpad = videorate.static_pad("sink").unwrap();
            if let Err(err) = pad.link(&videorate_sinkpad) {
                gst::error!(CAT, imp: self, "Failed to link pads: {:?}", err);
            }

            let pad = videorate.static_pad("src").unwrap();
            if let Err(err) = pad.link(&sinkpad) {
                gst::error!(CAT, imp: self, "Failed to link pads: {:?}", err);
            }

            return;
        }

        if let Err(err) = pad.link(&sinkpad) {
            gst::error!(CAT, imp: self, "Failed to link pads: {:?}", err);
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
        if state.bus_message_sigid.is_none() {
            let bus = self.pipeline.bus().unwrap();
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

    fn handle_bus_message(&self, message: &gst::Message) {
        if let gst::MessageView::StreamCollection(s) = message.view() {
            let collection = s.stream_collection();

            let stream = if let Some(ref wanted_stream_id) = self.requested_stream_id() {
                if let Some(stream) = collection.iter().find(|stream| {
                    let stream_id = stream.stream_id();
                    stream_id.map_or(false, |s| wanted_stream_id.as_str() == s.as_str())
                }) {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "{:?} Selecting specified stream: {:?}",
                        self.name,
                        wanted_stream_id
                    );

                    Some(stream)
                } else {
                    gst::warning!(
                        CAT,
                        imp: self,
                        "{:?} requested stream {} not found in {} - available: {:?}",
                        self.name,
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
                    imp: self,
                    "{:?} Selecting stream: {:?}",
                    self.name,
                    stream.stream_id()
                );
                stream
            } else {
                /* FIXME --- Post an error on the bus! */
                gst::error!(
                    CAT,
                    imp: self,
                    "{:?} No stream found for type: {:?}",
                    self.name,
                    self.stream_type()
                );

                return;
            };

            let _ = self.state.lock().unwrap().stream.insert(stream.clone());
            let uridecodebin = self.uridecodebin();

            if let Ok(_state_lock) = self.state_lock.try_lock() {
                message
                    .src()
                    .unwrap_or_else(|| uridecodebin.upcast_ref::<gst::Object>())
                    .downcast_ref::<gst::Element>()
                    .unwrap()
                    .send_event(gst::event::SelectStreams::new(&[stream
                        .stream_id()
                        .unwrap()
                        .as_str()]));
            }
        }
    }

    pub(crate) fn unused_since(&self) -> Option<std::time::Instant> {
        self.state.lock().unwrap().unused_since
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
        let _ = self.state_lock.lock();
        self.pipeline.set_state(gst::State::Playing)
    }

    pub(crate) fn target_src(&self) -> Option<super::PlaybinPoolSrc> {
        self.state.lock().unwrap().target_src.clone()
    }

    pub(crate) fn set_target_src(&self, target_src: Option<super::PlaybinPoolSrc>) {
        let mut state = self.state.lock().unwrap();

        if target_src.is_some() {
            state.unused_since = None;
        }
        state.target_src = target_src;
    }

    pub(crate) fn release(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, "Releasing pipeline {}", self.name);
        self.set_target_src(None);

        let obj = self.obj().clone();
        self.pipeline.call_async(move |pipeline| {
            let this = obj.imp();
            let state_lock = this.state_lock.lock();
            if let Err(err) = pipeline.set_state(gst::State::Null) {
                gst::error!(CAT, imp: this, "Could not teardown pipeline {err:?}");
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
        gst::debug!(CAT, imp: self, "Stopping");
        if let Some(sigid) = self.state.lock().unwrap().bus_message_sigid.take() {
            self.pipeline.bus().unwrap().disconnect(sigid);
        }

        let obj = self.obj().clone();
        self.pipeline.call_async(move |pipeline| {
            let this = obj.imp();

            let _ = this.state_lock.lock();
            if let Err(err) = pipeline.set_state(gst::State::Null) {
                gst::error!(CAT, obj: pipeline, "Could not teardown pipeline {err:?}");
            }
        });
    }
}

impl ObjectImpl for PooledPlayBin {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> =
            Lazy::new(|| vec![glib::subclass::Signal::builder("released").build()]);

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.uridecodebin
            .connect_pad_added(glib::clone!(@weak self as this => move |_, pad| {
                    this.pad_added(pad);
            }));
        self.uridecodebin
            .connect_pad_removed(glib::clone!(@weak self as this => move |_, pad| {
                    this.pad_removed(pad);
            }));
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PooledPlayBin {
    const NAME: &'static str = "GstPooledPlayBin";
    type Type = super::PooledPlayBin;
}
