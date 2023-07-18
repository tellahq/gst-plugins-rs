// SPDX-License-Identifier: MPL-2.0

#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-uridecodepool:
 *
 * Since: plugins-rs-0.13.0
 */
use gst::glib;

mod uridecodepool;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    uridecodepool::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    uridecodepool,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
