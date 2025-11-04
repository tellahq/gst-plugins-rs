// SPDX-License-Identifier: MPL-2.0

#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-rsvideoconvert:
 *
 * Since: plugins-rs-0.14.0
 */
use gst::glib;
use std::sync::LazyLock;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rsvideoconvert",
        gst::DebugColorFlags::empty(),
        Some("Rust Video Format Converter"),
    )
});

mod rsvideoconvert;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    rsvideoconvert::register(plugin)
}

gst::plugin_define!(
    rsvideoconvert,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
