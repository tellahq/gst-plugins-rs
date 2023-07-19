// SPDX-License-Identifier: MPL-2.0

use std::sync::Mutex;

use gst::{
    glib::{self, once_cell::sync::Lazy, Properties},
    prelude::*,
    subclass::prelude::*,
};
use tokio::runtime;

use super::PooledPlayBin;

pub static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "playbinpool",
        gst::DebugColorFlags::empty(),
        Some("Playbin Pool"),
    )
});

pub static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

static DEFAULT_CLEANUP_TIMEOUT_SEC: u64 = 15;

#[derive(Debug)]
struct Settings {
    cleanup_timeout: std::time::Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            cleanup_timeout: std::time::Duration::from_secs(DEFAULT_CLEANUP_TIMEOUT_SEC),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    running_pipelines: Vec<PooledPlayBin>,
    unused_pipelines: Vec<PooledPlayBin>,
}

#[derive(Properties, Debug, Default)]
#[properties(wrapper_type = super::PlaybinPool)]
pub struct PlaybinPool {
    state: Mutex<State>,

    #[property(name="cleanup-timeout",
        get = |s: &Self| s.settings.lock().unwrap().cleanup_timeout.as_secs(),
        set = Self::set_cleanup_timeout,
        type = u64,
        member = cleanup_timeout)]
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for PlaybinPool {
    const NAME: &'static str = "GstPlaybinPool";
    type Type = super::PlaybinPool;
}

impl ObjectImpl for PlaybinPool {
    fn properties() -> &'static [glib::ParamSpec] {
        Self::derived_properties()
    }

    fn set_property(&self, id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        self.derived_set_property(id, value, pspec)
    }

    fn property(&self, id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        self.derived_property(id, pspec)
    }
}

pub(crate) static PLAYBIN_POOL: Lazy<Mutex<super::PlaybinPool>> =
    Lazy::new(|| Mutex::new(glib::Object::new()));

impl PlaybinPool {
    fn set_cleanup_timeout(&self, timeout: u64) {
        let mut settings = self.settings.lock().unwrap();
        settings.cleanup_timeout = std::time::Duration::from_secs(timeout);
    }

    pub(crate) fn get(&self, uri: &str, stream_type: gst::StreamType, stream_id: Option<&str>) -> PooledPlayBin {
        gst::error!(CAT, "Getting pipeline for {uri}");
        let mut state = self.state.lock().unwrap();

        let playbin = if let Some(position) = state.unused_pipelines.iter().position(|p|
            stream_id.is_some() && p.stream_id().map_or(false, |id| Some(id.as_str()) == stream_id)
        ) {
            gst::error!(CAT, "Reusing the exact same pipeline for {:?}", stream_id);
            Some(state.unused_pipelines.remove(position))
        } else {
            state.unused_pipelines.pop()
        };

        if let Some(playbin) = playbin {
            playbin.reset(uri, stream_type, stream_id);
            state.running_pipelines.push(playbin.clone());

            gst::error!(CAT, "Reusing existing pipeline: {:?} - {:?}", playbin, playbin.pipeline().state(None));

            return playbin;
        }

        let pipeline = PooledPlayBin::new(uri, stream_type, stream_id);
        state.running_pipelines.push(pipeline.clone());

        pipeline
    }

    fn cleanup(&self) {
        gst::debug!(CAT, "Cleaning up unused pipelines");
        let mut state = self.state.lock().unwrap();

        state.unused_pipelines.retain(|p| {
            if p.imp().unused_since()
                .expect(
                    "Pipeline without unused_since set shouldn't be in the unused_pipelines list",
                )
                .elapsed()
                > self.settings.lock().unwrap().cleanup_timeout
            {
                if let Err(err) = p.imp().stop() {
                    gst::error!(CAT, "Failed to stop pipeline {p:?}: {:?}", err);
                }

                false
            } else {
                true
            }
        });

        gst::error!(CAT, "Unused pipelines: {:?}", state.unused_pipelines.len());
    }

    pub(crate) fn release(&self, pipeline: PooledPlayBin) {
        let mut state = self.state.lock().unwrap();
        state.running_pipelines.retain(|p| p != &pipeline);

        if let Err(err) = pipeline.imp().release() {
            gst::error!(CAT, "Failed to release pipeline: {}", err);
        }
        pipeline.imp().set_unused();
        state.unused_pipelines.push(pipeline);

        let cleanup_timeout = self.settings.lock().unwrap().cleanup_timeout.clone();
        RUNTIME.spawn(glib::clone!(@weak self as this => async move {
            gst::info!(
                CAT,
                "Cleaning up unused pipelines in {:?} seconds",
                cleanup_timeout
            );
            tokio::time::sleep(cleanup_timeout).await;

            this.cleanup();
        }));
    }
}
