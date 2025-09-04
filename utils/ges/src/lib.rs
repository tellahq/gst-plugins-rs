#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use gst::glib;
use std::sync::LazyLock;

mod subtimelineformatter;
mod subtimelineprimarymanager;

pub static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "gesrs",
        gst::DebugColorFlags::FG_YELLOW | gst::DebugColorFlags::BOLD,
        Some("GES plugin"),
    )
});

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    subtimelineformatter::register(plugin)?;

    // Register validate actions
    // At this point, gst_validate::init() has been called by the application
    #[cfg(feature = "validate")]
    {
        if let Err(err) = subtimelineprimarymanager::validate::register_validate_actions() {
            gst::warning!(CAT, "Failed to register validate actions: {}", err);
        }
    }
    #[cfg(not(feature = "validate"))]
    gst::info!(
        CAT,
        "Plugin built without 'validate' feature, validate actions will not be available"
    );

    Ok(())
}

gst::plugin_define!(
    rsges,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
