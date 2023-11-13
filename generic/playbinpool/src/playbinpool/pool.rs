// SPDX-License-Identifier: MPL-2.0

use std::sync::{Condvar, Mutex, MutexGuard};

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
    prepared_pipelines: Vec<PooledPlayBin>,
}

#[derive(Debug, Default)]
struct Outstandings {
    n: Mutex<u32>,
    cond: Condvar,
}

#[derive(Properties, Debug, Default)]
#[properties(wrapper_type = super::PlaybinPool)]
pub struct PlaybinPool {
    state: Mutex<State>,
    // Number of pipelines in used */
    outstandings: Outstandings,

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

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder("new-pipeline")
                    .param_types([gst::Pipeline::static_type()])
                    .build(),
                glib::subclass::Signal::builder("prepare-pipeline")
                    .param_types([super::PlaybinPoolSrc::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(|_, args| {
                        let pool = args[0].get::<super::PlaybinPool>().unwrap();
                        let src = args[1].get::<&super::PlaybinPoolSrc>().unwrap();

                        Some(pool.imp().prepare_pipeline(src).into())
                    })
                    .build(),
                glib::subclass::Signal::builder("unprepare-pipeline")
                    .param_types([super::PlaybinPoolSrc::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(|_, args| {
                        let pool = args[0].get::<super::PlaybinPool>().unwrap();
                        let src = args[1].get::<&super::PlaybinPoolSrc>().unwrap();

                        Some(pool.imp().unprepare_pipeline(src).into())
                    })
                    .build(),
                /**
                 * GstPlaybinPool::deinit:
                 *
                 * Deinitialize the pool, should be called when deinitializing
                 * GStreamer.
                 */
                glib::subclass::Signal::builder("deinit")
                    .action()
                    .class_handler(|_, args| {
                        let pool = args[0].get::<super::PlaybinPool>().unwrap();

                        pool.imp().deinit();
                        None
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
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
        {
            let mut settings = self.settings.lock().unwrap();
            settings.cleanup_timeout = std::time::Duration::from_secs(timeout);
        }

        self.cleanup();
    }

    fn deinit(&self) {
        self.set_cleanup_timeout(0);

        let mut state = self.state.lock().unwrap();
        while let Some(pipeline) = state.prepared_pipelines.pop() {
            if let Err(err) = pipeline.imp().release() {
                gst::error!(CAT, "Failed to release pipeline: {}", err);
            }
        }
        drop(state);

        let mut outstandings = self.outstandings.n.lock().unwrap();
        while *outstandings > 0 {
            outstandings = self.outstandings.cond.wait(outstandings).unwrap();
        }
    }

    fn unprepare_pipeline(&self, src: &super::PlaybinPoolSrc) -> bool {
        gst::debug!(CAT, imp: self, "Unpreparing pipeline for {:?}", src);

        let mut state = self.state.lock().unwrap();
        if let Some(position) = state
            .prepared_pipelines
            .iter()
            .position(|p| p.imp().target_src().as_ref() == Some(src))
        {
            let pipeline = state.prepared_pipelines.remove(position);
            drop(state);

            if let Err(err) = pipeline.imp().release() {
                gst::error!(CAT, "Failed to release pipeline: {}", err);
            }

            return true;
        }

        false
    }

    fn prepare_pipeline(&self, src: &super::PlaybinPoolSrc) -> bool {
        gst::debug!(CAT, imp: self, "Preparing pipeline for {:?}", src);

        let mut state = self.state.lock().unwrap();

        if state
            .prepared_pipelines
            .iter()
            .any(|p| p.imp().target_src().as_ref() == Some(src))
        {
            gst::debug!(CAT, "Pipeline already prepared for {:?}", src);

            return false;
        }

        let playbin = self.get_unused_or_create_pipeline(src, &mut state);

        gst::debug!(CAT, imp: self, "Starting {playbin:?}");
        if let Err(err) = playbin.imp().play() {
            gst::warning!(CAT, imp: self, "Failed to play pipeline: {}", err);
            return false;
        }

        state.prepared_pipelines.push(playbin);

        true
    }

    pub(crate) fn get(&self, src: &super::PlaybinPoolSrc) -> PooledPlayBin {
        gst::debug!(CAT, "Getting pipeline for {:?}", src);
        let mut state = self.state.lock().unwrap();

        let playbin = if let Some(position) = state
            .prepared_pipelines
            .iter()
            .position(|p| p.imp().target_src().as_ref() == Some(src))
        {
            gst::debug!(CAT, "Using already prepared pipeline for {:?}", src);

            state.prepared_pipelines.remove(position)
        } else {
            self.get_unused_or_create_pipeline(src, &mut state)
        };

        state.running_pipelines.push(playbin.clone());

        playbin
    }

    fn get_unused_or_create_pipeline<'lt>(
        &'lt self,
        src: &super::PlaybinPoolSrc,
        state: &mut MutexGuard<'lt, State>,
    ) -> PooledPlayBin {
        let uri = src.uri();
        let stream_type = src.stream_type();
        let stream_id = src.stream_id();

        let playbin = if let Some(position) = state.unused_pipelines.iter().position(|p| {
            stream_id.is_some()
                && p.requested_stream_id()
                    .map_or(false, |id| Some(id) == stream_id)
        }) {
            gst::debug!(CAT, "Reusing the exact same pipeline for {:?}", stream_id);
            Some(state.unused_pipelines.remove(position))
        } else if let Some(position) = state.unused_pipelines.iter().position(|p| {
            p.uridecodebin()
                .property::<Option<String>>("uri")
                .map_or(false, |uri| uri.as_str() != uri)
        }) {
            gst::debug!(
                CAT,
                "Using another pipeline which use another URI `uridecodebin3` \
                won't allow use to switch stream-id for different media types yet"
            );

            Some(state.unused_pipelines.remove(position))
        } else {
            None
        };

        let playbin = playbin.map_or_else(
            || {
                gst::debug!(CAT, "Starting new pipeline");

                let pipeline = PooledPlayBin::new(uri.as_ref(), stream_type, stream_id.as_deref());
                let obj = self.obj();
                let mut outstandings = self.outstandings.n.lock().unwrap();
                *outstandings += 1;
                self.outstandings.cond.notify_one();

                // Make sure the pipeline is returned to the pool once it is ready to be reused
                pipeline.connect_closure(
                    "released",
                    false,
                    glib::closure!(@watch obj => move
                        |pipeline: PooledPlayBin| {
                            gst::debug!(CAT, obj: obj, "{pipeline:?} not used anymore.");

                            let this = obj.imp();
                            this.state.lock().unwrap().unused_pipelines.insert(0, pipeline);

                            let cleanup_timeout = this.settings.lock().unwrap().cleanup_timeout;
                            RUNTIME.spawn(glib::clone!(@weak this => async move {
                                gst::info!(
                                    CAT,
                                    "Cleaning up unused pipelines in {:?} seconds",
                                    cleanup_timeout
                                );
                                tokio::time::sleep(cleanup_timeout).await;

                                this.cleanup();
                            }));
                        }
                    ),
                );

                obj.emit_by_name::<()>("new-pipeline", &[&pipeline.pipeline()]);

                pipeline
            },
            |playbin| {
                playbin.reset(uri.as_ref(), stream_type, stream_id.as_deref());

                gst::debug!(CAT, "Reusing existing pipeline: {:?}", playbin,);

                playbin
            },
        );

        playbin.imp().set_target_src(Some(src.clone()));
        playbin
    }

    fn cleanup(&self) {
        gst::debug!(CAT, "Cleaning up unused pipelines");
        let mut state = self.state.lock().unwrap();

        let mut cleaned_up = Vec::new();
        state.unused_pipelines.retain(|p| {
            if p.imp()
                .unused_since()
                .expect(
                    "Pipeline without unused_since set shouldn't be in the unused_pipelines list",
                )
                .elapsed()
                > self.settings.lock().unwrap().cleanup_timeout
            {
                cleaned_up.insert(0, p.clone());
                false
            } else {
                true
            }
        });
        gst::debug!(CAT, "Unused pipelines: {:?}", state.unused_pipelines.len());
        drop(state);

        let removed = cleaned_up.len();
        for pipeline in cleaned_up.into_iter() {
            pipeline.imp().stop();
        }

        let mut outstandings = self.outstandings.n.lock().unwrap();
        *outstandings -= removed as u32;
        self.outstandings.cond.notify_one();
        drop(outstandings);
    }

    pub(crate) fn release(&self, pipeline: PooledPlayBin) {
        self.state
            .lock()
            .unwrap()
            .running_pipelines
            .retain(|p| p != &pipeline);

        if let Err(err) = pipeline.imp().release() {
            gst::error!(CAT, "Failed to release pipeline: {}", err);
        }
    }
}
