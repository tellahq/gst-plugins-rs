#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use gst::glib;

mod utils;
mod check_last_frame_qrcode;
mod compare_last_frame;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    if let Err(err) = check_last_frame_qrcode::register_validate_actions(plugin) {
        gst::warning!(
            gst::CAT_RUST,
            "Failed to register validate actions: {}",
            err
        );

        return Err(err);
    }

    if let Err(err) = compare_last_frame::register_validate_actions(plugin) {
        gst::warning!(
            gst::CAT_RUST,
            "Failed to register validate actions: {}",
            err
        );

        return Err(err);
    }

    Ok(())
}

gst::plugin_define!(
    rsvalidate,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
