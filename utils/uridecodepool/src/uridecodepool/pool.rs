// SPDX-License-Identifier: MPL-2.0

use std::sync::{Condvar, Mutex, MutexGuard};

use gst::{
    glib::{self, Properties},
    prelude::*,
    subclass::prelude::*,
};
use once_cell::sync::Lazy;
use tokio::runtime;

use super::DecoderPipeline;

pub static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "uridecodepool",
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
    bus: Option<gst::Bus>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            cleanup_timeout: std::time::Duration::from_secs(DEFAULT_CLEANUP_TIMEOUT_SEC),
            bus: None,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    running_pipelines: Vec<DecoderPipeline>,
    unused_pipelines: Vec<DecoderPipeline>,
    prepared_pipelines: Vec<DecoderPipeline>,
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
    #[property(name="bus", get, set, type = Option<gst::Bus>, member = bus)]
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for PlaybinPool {
    const NAME: &'static str = "GstPlaybinPool";
    type Type = super::PlaybinPool;
}

#[glib::derived_properties]
impl ObjectImpl for PlaybinPool {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder("new-pipeline")
                    .param_types([gst::Pipeline::static_type()])
                    .build(),
                glib::subclass::Signal::builder("prepared-pipeline-removed")
                    .param_types([super::UriDecodePoolSrc::static_type()])
                    .build(),
                glib::subclass::Signal::builder("prepare-pipeline")
                    .param_types([super::UriDecodePoolSrc::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(|_, args| {
                        let pool = args[0].get::<super::PlaybinPool>().unwrap();
                        let src = args[1].get::<&super::UriDecodePoolSrc>().unwrap();

                        Some(pool.imp().prepare_pipeline(src).into())
                    })
                    .build(),
                glib::subclass::Signal::builder("unprepare-pipeline")
                    .param_types([super::UriDecodePoolSrc::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(|_, args| {
                        let pool = args[0].get::<super::PlaybinPool>().unwrap();
                        let src = args[1].get::<&super::UriDecodePoolSrc>().unwrap();

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
        gst::error!(CAT, "Deinitializing pool");
        self.set_cleanup_timeout(0);

        let obj = self.obj();
        let mut state = self.state.lock().unwrap();
        while let Some(pipeline) = state.prepared_pipelines.pop() {
            if let Some(src) = pipeline.imp().target_src() {
                obj.emit_by_name::<()>("prepared-pipeline-removed", &[&src]);
            }
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

    fn unprepare_pipeline(&self, src: &super::UriDecodePoolSrc) -> bool {
        gst::debug!(CAT, imp: self, "Unpreparing pipeline for {:?}", src);

        let mut state = self.state.lock().unwrap();
        if let Some(position) = state
            .prepared_pipelines
            .iter()
            .position(|p| p.imp().target_src().as_ref() == Some(src))
        {
            let pipeline = state.prepared_pipelines.remove(position);
            drop(state);

            self.obj().emit_by_name::<()>(
                "prepared-pipeline-removed",
                &[&pipeline.imp().target_src().unwrap()],
            );

            if let Err(err) = pipeline.imp().release() {
                gst::error!(CAT, "Failed to release pipeline: {}", err);
            }

            return true;
        }

        false
    }

    fn prepare_pipeline(&self, src: &super::UriDecodePoolSrc) -> bool {
        gst::info!(CAT, imp: self, "Preparing pipeline for {}:{:?}", src.name(), src as *const _);

        let mut state = self.state.lock().unwrap();

        if state
            .prepared_pipelines
            .iter()
            .any(|p| p.imp().target_src().as_ref() == Some(src))
        {
            gst::debug!(
                CAT,
                "Pipeline already prepared for {}:{:?}",
                src.name(),
                src as *const _
            );

            return false;
        }

        let decoderpipe = self.get_unused_or_create_pipeline(src, &mut state);
        gst::debug!(CAT, imp: self, "Starting {decoderpipe:?}");
        if let Err(err) = decoderpipe.imp().play() {
            gst::warning!(CAT, imp: self, "Failed to play pipeline: {}", err);
            return false;
        }

        state.prepared_pipelines.push(decoderpipe);

        true
    }

    pub(crate) fn get(&self, src: &super::UriDecodePoolSrc) -> DecoderPipeline {
        gst::debug!(CAT, "Getting pipeline for {:?}", src.name());
        let mut state = self.state.lock().unwrap();

        let (decoderpipe, mut state) = if let Some(position) = state
            .prepared_pipelines
            .iter()
            .position(|p| p.imp().target_src().as_ref() == Some(src))
        {
            let pipe = state.prepared_pipelines.remove(position);
            drop(state);

            self.obj()
                .emit_by_name::<()>("prepared-pipeline-removed", &[&src]);

            let pipeline = pipe.pipeline();
            gst::error!(
                CAT,
                obj: pipeline,
                "{} for {} -- {:?}?stream-id{:?} -- {:?}",
                if pipe.imp().seek_handler.has_eos_sample() {
                    "Reusing already running pipeline to try to keep flow"
                } else {
                    "Using already prepared pipeline"
                },
                src.name(),
                src.uri(),
                src.stream_id(),
                pipeline.state(gst::ClockTime::NONE)
            );

            (pipe, self.state.lock().unwrap())
        } else {
            (self.get_unused_or_create_pipeline(src, &mut state), state)
        };

        state.running_pipelines.push(decoderpipe.clone());

        decoderpipe
    }

    fn get_unused_or_create_pipeline<'lt>(
        &'lt self,
        src: &super::UriDecodePoolSrc,
        state: &mut MutexGuard<'lt, State>,
    ) -> DecoderPipeline {
        let uri = src.uri();
        let caps = src.caps();
        let stream_id = src.stream_id();
        let seek = src.imp().initial_seek_event();

        let decoderpipe = if let Some(position) = state.unused_pipelines.iter().position(|p| {
            stream_id.is_some()
                && p.requested_stream_id()
                    .map_or(false, |id| Some(id) == stream_id)
        }) {
            gst::debug!(CAT, "Reusing the exact same pipeline for {:?}", stream_id);
            Some(state.unused_pipelines.remove(position))
        } else if let Some(position) = state.unused_pipelines.iter().position(|p| {
            let initial_seek = p.imp().initial_seek();

            if initial_seek.is_some() && seek.is_none() {
                return false;
            } else if initial_seek.is_some()
                && seek.as_ref().unwrap().structure() != initial_seek.as_ref().unwrap().structure()
            {
                gst::error!(CAT, "Seek events are different");
                return false;
            }

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

        let decoderpipe = decoderpipe.map_or_else(
            || {
                gst::error!(CAT, "Starting new pipeline for {:?}", src.name(),);

                let pipeline =
                    DecoderPipeline::new(&uri, &caps, stream_id.as_deref(), &self.obj(), seek);
                let obj = self.obj();
                let mut outstandings = self.outstandings.n.lock().unwrap();
                *outstandings += 1;
                self.outstandings.cond.notify_one();

                // Make sure the pipeline is returned to the pool once it is ready to be reused
                pipeline.connect_closure(
                    "released",
                    false,
                    glib::closure!(@watch obj => move
                        |pipeline: DecoderPipeline| {
                            gst::error!(CAT, obj: obj, "{pipeline:?} not used anymore.");

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
            |decoderpipe| {
                decoderpipe.reset(&uri, &caps, stream_id.as_deref());

                gst::debug!(CAT, "Reusing existing pipeline: {:?}", decoderpipe,);

                decoderpipe
            },
        );

        decoderpipe.imp().set_target_src(Some(src.clone()));
        decoderpipe
    }

    fn cleanup(&self) {
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
        drop(state);

        let removed = cleaned_up.len();
        for pipeline in cleaned_up.into_iter() {
            pipeline.imp().stop();
        }

        let mut outstandings = self.outstandings.n.lock().unwrap();
        *outstandings -= removed as u32;
        self.outstandings.cond.notify_one();
    }

    pub(crate) fn release(&self, pipeline: DecoderPipeline) {
        let mut state = self.state.lock().unwrap();

        state.running_pipelines.retain(|p| p != &pipeline);

        let pipeline_imp = pipeline.imp();
        if pipeline_imp.seek_handler.has_eos_sample() {
            gst::error!(CAT, obj: pipeline, "Pipeline has EOS sample, keeping it alive for 20s");

            // Try to make it be reused asap
            state.prepared_pipelines.insert(0, pipeline.clone());
            drop(state);
            RUNTIME.spawn(glib::clone!(@weak self as this, @weak pipeline => async move {
                let cleanup_timeout = std::time::Duration::from_secs(DEFAULT_CLEANUP_TIMEOUT_SEC);
                gst::info!(
                    CAT,
                    "Cleaning up unused pipelines in {:?} seconds",
                    cleanup_timeout
                );
                tokio::time::sleep(cleanup_timeout).await;

                if pipeline.imp().target_src().is_none() {
                    if let Err(e) = pipeline.imp().release() {
                        gst::error!(CAT, imp: this, "Failed to release pipeline: {e:?}");
                    }

                    this.cleanup();
                }
            }));

            return;
        } else {
            drop(state);
        }

        if let Err(err) = pipeline.imp().release() {
            gst::error!(CAT, "Failed to release pipeline: {}", err);
        }
    }
}
