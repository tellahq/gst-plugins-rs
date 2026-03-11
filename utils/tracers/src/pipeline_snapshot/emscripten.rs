// Copyright (C) 2025 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Emscripten FFI bindings for main-thread-proxied JavaScript execution.
//!
//! When GStreamer runs with `PROXY_TO_PTHREAD`, pipeline code executes in a
//! worker thread. DOM access (needed by the dots-wasm viewer) must happen on
//! the main browser thread. This module provides [`run_script_on_main_thread`]
//! which uses Emscripten's proxying API to synchronously dispatch a JS string
//! for evaluation on the main thread.

use std::ffi::{c_void, CString};
use std::os::raw::c_char;

unsafe extern "C" {
    fn emscripten_main_runtime_thread_id() -> usize;
    fn emscripten_proxy_sync(
        queue: *mut c_void,
        target_thread: usize,
        func: unsafe extern "C" fn(*mut c_void),
        arg: *mut c_void,
    ) -> bool;
    fn emscripten_proxy_get_system_queue() -> *mut c_void;
    fn emscripten_run_script(script: *const c_char);
    fn pthread_self() -> usize;
}

unsafe extern "C" fn run_script_trampoline(arg: *mut c_void) {
    let script = arg as *const c_char;
    unsafe {
        emscripten_run_script(script);
    }
}

/// Run a JavaScript snippet synchronously on the main browser thread.
///
/// If already on the main thread, the script is executed directly.
/// Otherwise, the calling thread blocks until the script has finished
/// executing on the main thread via `emscripten_proxy_sync`.
pub fn run_script_on_main_thread(script: &str) {
    let c_script = CString::new(script).expect("JS script contained null byte");
    unsafe {
        let main_thread = emscripten_main_runtime_thread_id();
        if pthread_self() == main_thread {
            emscripten_run_script(c_script.as_ptr());
        } else {
            let queue = emscripten_proxy_get_system_queue();
            emscripten_proxy_sync(
                queue,
                main_thread,
                run_script_trampoline,
                c_script.as_ptr() as *mut c_void,
            );
        }
    }
}

/// Notify the browser-side dots-wasm viewer of a new pipeline DOT graph.
///
/// Calls `globalThis.__gstDotsWasm.onNewDot(name, content)` on the main
/// thread if the viewer has been loaded.
pub fn notify_new_dot(name: &str, dot_data: &str) {
    // Escape for JS string literals: backslash, backtick, dollar (template
    // literal delimiters).
    fn escape_js(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('`', "\\`")
            .replace('$', "\\$")
    }

    let js = format!(
        "if (typeof globalThis.__gstDotsWasm !== 'undefined') {{ globalThis.__gstDotsWasm.onNewDot(`{}`, `{}`); }}",
        escape_js(name),
        escape_js(dot_data),
    );

    run_script_on_main_thread(&js);
}
