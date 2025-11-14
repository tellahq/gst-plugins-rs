#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use std::sync::LazyLock;

use gst::glib;

mod check_last_frame_qrcode;
mod compare_last_frame;
mod utils;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rsvalidate",
        gst::DebugColorFlags::empty(),
        Some("GStreamer Validate Rust Plugin"),
    )
});

fn register_actions() -> Result<(), glib::BoolError> {
    check_last_frame_qrcode::register_validate_actions("rsvalidate")?;
    compare_last_frame::register_validate_actions("rsvalidate")
}

fn plugin_init(_plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    register_actions()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
            gst_validate::init();
        });
    }

    #[test]
    fn test_plugin_functions_exist() {
        init();

        register_actions().expect("Failed to register actions");
    }
}
