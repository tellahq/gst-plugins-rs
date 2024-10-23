// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex, MutexGuard,
    },
};

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
        gst::DebugColorFlags::FG_YELLOW,
        Some("Decoder Pool"),
    )
});

pub static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("uridecodepool")
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
    // Running pipelines
    running: Vec<DecoderPipeline>,
    // Pipelines that have been released to the pool and can be reused
    pooled: Vec<DecoderPipeline>,
    // Pipelines prepared by the application for specific source
    prepared: Vec<DecoderPipeline>,
    defered_release_tasks: HashMap<DecoderPipeline, std::time::Instant>,
}

#[derive(Properties, Debug, Default)]
#[properties(wrapper_type = super::UriDecodePool)]
pub struct UriDecodePool {
    state: Mutex<State>,
    deinitialized: AtomicBool,

    // All pipelines
    pipelines: Mutex<Vec<DecoderPipeline>>,
    pipelines_cond: Condvar,

    #[property(name="cleanup-timeout",
        get = |s: &Self| s.settings.lock().unwrap().cleanup_timeout.as_secs(),
        set = Self::set_cleanup_timeout,
        type = u64,
        member = cleanup_timeout)]
    #[property(name="bus", get, set, type = Option<gst::Bus>, member = bus)]
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for UriDecodePool {
    const NAME: &'static str = "GstUriDecodePool";
    type Type = super::UriDecodePool;
}

#[glib::derived_properties]
impl ObjectImpl for UriDecodePool {
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
                        let pool = args[0].get::<super::UriDecodePool>().unwrap();
                        let src = args[1].get::<&super::UriDecodePoolSrc>().unwrap();

                        Some(pool.imp().prepare_pipeline(src).into())
                    })
                    .build(),
                glib::subclass::Signal::builder("unprepare-pipeline")
                    .param_types([super::UriDecodePoolSrc::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(|_, args| {
                        let pool = args[0].get::<super::UriDecodePool>().unwrap();
                        let src = args[1].get::<&super::UriDecodePoolSrc>().unwrap();

                        Some(pool.imp().unprepare_pipeline(src).into())
                    })
                    .build(),
                /**
                 * GstUriDecodePool::deinit:
                 *
                 * Deinitialize the pool, should be called when deinitializing
                 * GStreamer.
                 */
                glib::subclass::Signal::builder("deinit")
                    .action()
                    .class_handler(|_, args| {
                        let pool = args[0].get::<super::UriDecodePool>().unwrap();

                        pool.imp().deinit();
                        None
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }
}

pub(crate) static PIPELINE_POOL_POOL: Lazy<Mutex<super::UriDecodePool>> =
    Lazy::new(|| Mutex::new(glib::Object::new()));

impl UriDecodePool {
    fn set_cleanup_timeout(&self, timeout: u64) {
        {
            let mut settings = self.settings.lock().unwrap();
            settings.cleanup_timeout = std::time::Duration::from_secs(timeout);
        }

        self.cleanup();
    }

    pub(crate) fn deinitialized(&self) -> bool {
        self.deinitialized.load(Ordering::SeqCst)
    }

    fn deinit(&self) {
        gst::info!(CAT, "Deinitializing pool");
        self.set_cleanup_timeout(0);

        let obj = self.obj();
        self.deinitialized.store(true, Ordering::SeqCst);

        let all_pipelines = self.pipelines.lock().unwrap().clone();
        for pipeline in all_pipelines.into_iter() {
            let imp = pipeline.imp();

            if let Some(src) = imp.target_src() {
                obj.emit_by_name::<()>("prepared-pipeline-removed", &[&src]);
            }
            if let Err(err) = imp.release() {
                gst::error!(CAT, "Failed to release pipeline: {}", err);
            }
        }

        let mut pipelines = self.pipelines.lock().unwrap();
        gst::log!(
            CAT,
            "Outstandings: {:?}",
            pipelines.iter().map(|p| p.name()).collect::<Vec<_>>()
        );
        while pipelines.len() > 0 {
            pipelines = self.pipelines_cond.wait(pipelines).unwrap();
            gst::log!(
                CAT,
                "Outstandings: {:?}",
                pipelines.iter().map(|p| p.name()).collect::<Vec<_>>()
            );
        }
        gst::log!(
            CAT,
            "YAY: Outstandings: {:?}",
            pipelines.iter().map(|p| p.name()).collect::<Vec<_>>()
        );
        gst::info!(CAT, "Done deinitializaing");
    }

    fn unprepare_pipeline(&self, src: &super::UriDecodePoolSrc) -> bool {
        gst::debug!(CAT, imp: self, "Unpreparing pipeline for {:?}", src);

        let mut state = self.state.lock().unwrap();

        if let Some(position) = state.prepared.iter().position(|p| {
            p.imp().target_src().as_ref() == Some(src) && !p.seek_handler().has_eos_sample()
        }) {
            let pipeline = state.prepared.remove(position);
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
            .prepared
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

        if state
            .running
            .iter()
            .any(|p| p.imp().target_src().as_ref() == Some(src))
        {
            gst::debug!(
                CAT,
                "Pipeline already running for {}:{:?}",
                src.name(),
                src as *const _
            );

            return false;
        }

        let decoderpipe = self.get_unused_or_create_pipeline(src, &mut state);
        gst::debug!(CAT, imp: self, "Starting {decoderpipe:?}");
        if let Err(err) = decoderpipe.imp().play() {
            gst::warning!(CAT, imp: self, "Failed to play pipeline: {}", err);
            if let Err(e) = decoderpipe.imp().release() {
                gst::error!(CAT, imp: self, "Failed to release pipeline: {}", e);
            }

            return false;
        }

        state.prepared.push(decoderpipe);

        true
    }

    pub(crate) fn get(&self, src: &super::UriDecodePoolSrc) -> DecoderPipeline {
        gst::debug!(CAT, "Getting pipeline for {:?}", src.name());
        let mut state = self.state.lock().unwrap();

        let (decoderpipe, mut state) = if let Some(position) = state
            .prepared
            .iter()
            .position(|p| p.imp().target_src().as_ref() == Some(src))
        {
            let pipe = state.prepared.remove(position);
            drop(state);

            self.obj()
                .emit_by_name::<()>("prepared-pipeline-removed", &[&src]);

            let pipeline = pipe.pipeline();
            gst::error!(
                CAT,
                obj: pipeline,
                "{} for {} -- {:?}?stream-id{:?} -- {:?}",
                if pipe.imp().seek_handler().has_eos_sample() {
                    "Reusing already running pipeline to try to keep flow"
                } else {
                    "Using already prepared pipeline"
                },
                src.name(),
                src.uri(),
                src.stream_id(),
                pipeline.state(gst::ClockTime::ZERO)
            );

            (pipe, self.state.lock().unwrap())
        } else {
            (self.get_unused_or_create_pipeline(src, &mut state), state)
        };

        state.running.push(decoderpipe.clone());

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

        let decoderpipe = if let Some(position) = state.pooled.iter().position(|p| {
            stream_id.is_some()
                && !p.seek_handler().has_eos_sample()
                && p.requested_stream_id()
                    .map_or(false, |id| Some(id) == stream_id)
                && false
        }) {
            gst::debug!(CAT, "Reusing the exact same pipeline for {:?}", stream_id);
            Some(state.pooled.remove(position))
        } else if let Some(position) = state.pooled.iter().position(|p| {
            let initial_seek = p.imp().initial_seek();

            if initial_seek.is_some() && seek.is_none() {
                return false;
            } else if initial_seek.is_some()
                && seek.as_ref().unwrap().structure() != initial_seek.as_ref().unwrap().structure()
            {
                gst::error!(
                    CAT,
                    "Seek events are different {seek:?} != {initial_seek:?}"
                );
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

            Some(state.pooled.remove(position))
        } else {
            None
        };

        let decoderpipe = decoderpipe.map_or_else(
            || {
                let pipeline_name = format!(
                    "{}_{}",
                    src.name(),
                    src.imp().prepare_pipeline_next_number()
                );
                let pipeline = DecoderPipeline::new(
                    &pipeline_name,
                    uri.as_ref()
                        .expect("URI should be set when getting an underlying pipeline"),
                    &caps,
                    stream_id.as_deref(),
                    &self.obj(),
                    seek,
                );
                gst::info!(
                    CAT,
                    "Started new pipeline for {:?} -> {}",
                    src.name(),
                    pipeline.pipeline().name()
                );
                let obj = self.obj();

                // Make sure the pipeline is returned to the pool once it is ready to be reused
                pipeline.connect_closure(
                    "released",
                    false,
                    glib::closure!(@watch obj => move
                        |pipeline: DecoderPipeline| {
                            obj.imp().pipeline_released_cb(pipeline);
                        }
                    ),
                );

                pipeline.connect_closure(
                    "stopped",
                    false,
                    glib::closure!(@watch obj => move
                        |pipeline: DecoderPipeline| {
                        obj.imp().pipeline_stopped_cb(pipeline);
                        }
                    ),
                );

                obj.emit_by_name::<()>("new-pipeline", &[&pipeline.pipeline()]);

                let mut all_pipelines = self.pipelines.lock().unwrap();
                all_pipelines.insert(0, pipeline.clone());
                self.pipelines_cond.notify_one();
                pipeline
            },
            |decoderpipe| {
                decoderpipe.reset(uri.as_ref().unwrap(), &caps, stream_id.as_deref());

                gst::debug!(CAT, "Reusing existing pipeline: {:?}", decoderpipe,);

                decoderpipe
            },
        );

        decoderpipe.imp().set_target_src(Some(src.clone()));
        decoderpipe
    }

    fn pipeline_released_cb(&self, pipeline: DecoderPipeline) {
        gst::log!(CAT, imp: self, "{} released.", pipeline.pipeline().name());

        let mut state = self.state.lock().unwrap();
        if state.pooled.contains(&pipeline) {
            gst::debug!(
                CAT,
                "Released {} which was already marked as unused",
                pipeline.pipeline().name()
            );
        } else {
            state.pooled.insert(0, pipeline);
        }

        let cleanup_timeout = self.settings.lock().unwrap().cleanup_timeout;
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

    fn pipeline_stopped_cb(&self, pipeline: DecoderPipeline) {
        gst::log!(CAT, imp: self, "{} not used stopped.", pipeline.pipeline().name());

        let mut all_pipelines = self.pipelines.lock().unwrap();
        all_pipelines.retain(|p| p != &pipeline);
        gst::log!(
            CAT,
            "{} => {:?} outstanding pipelines",
            pipeline.name(),
            *all_pipelines
        );
        self.pipelines_cond.notify_one();
    }

    fn cleanup(&self) {
        let mut state = self.state.lock().unwrap();

        let mut cleaned_up = Vec::new();
        state.pooled.retain(|p| {
            if p.imp()
                .unused_since()
                .expect("Pipeline without unused_since set shouldn't be in the pooled list")
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

        for pipeline in cleaned_up.into_iter() {
            pipeline.imp().stop();
        }
    }

    pub(crate) fn release(&self, pipeline: DecoderPipeline) {
        let mut state = self.state.lock().unwrap();

        state.running.retain(|p| p != &pipeline);

        let pipeline_imp = pipeline.imp();
        if pipeline_imp.seek_handler().has_eos_sample() {
            let mut cleanup_timeout = self.settings.lock().unwrap().cleanup_timeout;

            // FIXME: Find a better way to handle keeping the pipeline with fake EOS around
            if cleanup_timeout < std::time::Duration::from_secs(1) {
                cleanup_timeout = std::time::Duration::from_secs(1);
            }
            // Let it be reused asap
            state.prepared.insert(0, pipeline.clone());
            let now = std::time::Instant::now();
            state.defered_release_tasks.insert(pipeline.clone(), now);

            RUNTIME.spawn(
                glib::clone!(@weak self as this, @weak pipeline => async move {
                    gst::info!(
                        CAT,
                        "Cleaning up unused pipeline {} in {:?} seconds",
                        pipeline.pipeline().name(),
                        cleanup_timeout
                    );
                    tokio::time::sleep(cleanup_timeout).await;

                    let mut state = this.state.lock().unwrap();
                    if let Some(created_time) = state.defered_release_tasks.get(&pipeline) {
                        if created_time != &now {
                            return;
                        }
                    } else {
                        return;
                    }

                    if pipeline.imp().target_src().is_none() || this.deinitialized.load(Ordering::SeqCst) {
                        gst::info!(CAT, "Releasing pipeline {}", pipeline.pipeline().name());
                        if let Some(src) = pipeline.imp().target_src() {
                            this.obj().emit_by_name::<()>("prepared-pipeline-removed", &[&src]);
                        }

                        if let Err(e) = pipeline.imp().release() {
                            gst::error!(CAT, imp: this, "Failed to release pipeline: {e:?}");
                        }

                        state.defered_release_tasks.remove(&pipeline);
                        drop(state);

                        this.cleanup();
                    } else {
                        gst::info!(CAT, "Pipeline {} now has a target, not releasing", pipeline.pipeline().name());
                        state.defered_release_tasks.remove(&pipeline);
                    }
                })
            );

            return;
        } else {
            state.defered_release_tasks.remove(&pipeline);
            drop(state);
        }

        if let Err(err) = pipeline.imp().release() {
            gst::error!(CAT, "Failed to release pipeline: {}", err);
        }
    }
}
