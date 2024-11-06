// Copyright (C) 2024 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-memory:
 *
 * This tracer provides an easy way to track memory allocations over time in a pipeline.
 */
use gst::glib;
use gst::glib::translate::ToGlibPtr;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "memory-tracer",
        gst::DebugColorFlags::empty(),
        Some("Tracer to collect information about GStreamer memory allocations"),
    )
});

struct MemoryEvent {
    timestamp: u64,
    ptr: usize,
    parent: usize,
    size: usize,
    is_alloc: bool,
    memory_type: &'static std::ffi::CStr,
}

impl MemoryEvent {
    fn event_type(&self) -> &str {
        if self.is_alloc {
            "alloc"
        } else {
            "free"
        }
    }
}

#[derive(Debug)]
struct Settings {
    file: PathBuf,
    include_filter: Option<Regex>,
    exclude_filter: Option<Regex>,
}

impl Settings {
    fn update_from_params(&mut self, imp: &MemoryTracer, params: String) {
        let s = match gst::Structure::from_str(&format!("memory-tracer,{params}")) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(CAT, "failed to parse tracer parameters: {}", err);
                return;
            }
        };

        if let Ok(file) = s.get::<&str>("file") {
            gst::log!(CAT, "file= {}", file);
            self.file = PathBuf::from(file);
        }

        if let Ok(filter) = s.get::<&str>("include-filter") {
            gst::log!(CAT, imp: imp, "include filter= {}", filter);
            let filter = match Regex::new(filter) {
                Ok(filter) => Some(filter),
                Err(err) => {
                    gst::error!(
                        CAT,
                        imp: imp,
                        "Failed to compile include-filter regex: {}",
                        err
                    );
                    None
                }
            };
            self.include_filter = filter;
        }

        if let Ok(filter) = s.get::<&str>("exclude-filter") {
            gst::log!(CAT, imp: imp, "exclude filter= {}", filter);
            let filter = match Regex::new(filter) {
                Ok(filter) => Some(filter),
                Err(err) => {
                    gst::error!(
                        CAT,
                        imp: imp,
                        "Failed to compile exclude-filter regex: {}",
                        err
                    );
                    None
                }
            };
            self.exclude_filter = filter;
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            file: PathBuf::from("/tmp/memory_tracer.log"),
            include_filter: None,
            exclude_filter: None,
        }
    }
}

#[derive(Default)]
struct State {
    log: Vec<MemoryEvent>,
    settings: Settings,
    logs_written: bool,
}
#[derive(Default)]
pub struct MemoryTracer {
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for MemoryTracer {
    const NAME: &'static str = "GstMemoryTracer";
    type Type = super::MemoryTracer;
    type ParentType = gst::Tracer;
}

impl MemoryTracer {
    fn write_log(&self, file_path: Option<String>) {
        use std::io::prelude::*;

        let mut state = self.state.lock().unwrap();
        let mut file = match file_path.map_or_else(
            || std::fs::File::create(&state.settings.file),
            |path| std::fs::File::create(PathBuf::from(path)),
        ) {
            Ok(file) => file,
            Err(err) => {
                gst::error!(CAT, imp: self, "Failed to create file: {err}");
                return;
            }
        };

        gst::error!(
            CAT,
            imp: self,
            "Writing file {:?}",
            file
        );

        let log = std::mem::replace(&mut state.log, Vec::new());
        state.logs_written = true;
        drop(state);

        for event in &log {
            if let Err(err) = writeln!(
                &mut file,
                "{},{},{},0x{:08x},{:?},{}",
                event.timestamp,
                event.event_type(),
                event.ptr,
                event.parent,
                event.memory_type,
                event.size
            ) {
                gst::error!(CAT, imp: self, "Failed to write to file: {err}");
            }
        }
    }
}

impl ObjectImpl for MemoryTracer {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![glib::subclass::Signal::builder("write-log")
                .action()
                .param_types([Option::<String>::static_type()])
                .class_handler(|_, args| {
                    let obj = args[0].get::<super::MemoryTracer>().unwrap();
                    let obj = args[0].get::<super::MemoryTracer>().unwrap();

                    obj.imp()
                        .write_log(args[1].get::<Option<String>>().unwrap());

                    None
                })
                .build()]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        if let Some(params) = self.obj().property::<Option<String>>("params") {
            let mut state = self.state.lock().unwrap();
            state.settings.update_from_params(self, params);
        }

        self.register_hook(TracerHook::MemoryInit);
        self.register_hook(TracerHook::MemoryFreePre);
    }

    fn dispose(&self) {
        if self.state.lock().unwrap().logs_written {
            gst::info!(CAT, "Logs were written manually, not overriting on dispose");
            return;
        }

        self.write_log(None);
    }
}

impl TracerImpl for MemoryTracer {
    fn memory_init(&self, ts: u64, memory: &gst::MemoryRef) {
        let mut state = self.state.lock().unwrap();
        let size = memory.maxsize();
        let ptr = memory.as_ptr() as usize;

        let parent = memory.parent().map_or(0 as usize, |p| p.as_ptr() as usize);

        let memory_type = unsafe {
            let alloc: *const gst::ffi::GstAllocator = memory.allocator().unwrap().to_glib_none().0;
            std::ffi::CStr::from_ptr((*alloc).mem_type)
        };
        state.log.push(MemoryEvent {
            timestamp: ts,
            ptr: memory.as_ptr() as usize,
            parent,
            is_alloc: true,
            memory_type,
            size,
        });
    }

    fn memory_free_pre(&self, ts: u64, memory: &gst::MemoryRef) {
        let mut state = self.state.lock().unwrap();
        let ptr = memory.as_ptr() as usize;

        let memory_type = unsafe {
            let alloc: *const gst::ffi::GstAllocator = memory.allocator().unwrap().to_glib_none().0;
            std::ffi::CStr::from_ptr((*alloc).mem_type)
        };
        let parent = memory.parent().map_or(0 as usize, |p| p.as_ptr() as usize);
        state.log.push(MemoryEvent {
            timestamp: ts,
            parent,
            ptr: memory.as_ptr() as usize,
            is_alloc: false,
            memory_type,
            size: memory.maxsize(),
        });
    }
}

impl GstObjectImpl for MemoryTracer {}
