use ges::prelude::*;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct SubTimelineFormatter(ObjectSubclass<imp::SubTimelineFormatter>)
        @extends ges::Formatter, gst::Object;
}

pub fn register(_plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    ges::Formatter::register(
        SubTimelineFormatter::static_type(),
        "GESSubTimelineFormatter",
        Some("GES subtimeline formatter"),
        Some("subtimeline"),
        Some("application/x-subtimeline"),
        1.0,
        gst::Rank::PRIMARY + 100,
    );
    Ok(())
}
