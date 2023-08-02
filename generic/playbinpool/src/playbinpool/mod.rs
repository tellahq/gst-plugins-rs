// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

mod imp;
mod playbinpool;
mod pooledplaybin;

glib::wrapper! {
    pub struct PlaybinPoolSrc(ObjectSubclass<imp::PlaybinPoolSrc>)
        @extends gst_base::BaseSrc, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

glib::wrapper! {
    pub struct PlaybinPool(ObjectSubclass<playbinpool::PlaybinPool>);
}

impl PlaybinPool {
    pub(crate) fn get_playbin(
        &self,
        uri: &str,
        stream_type: gst::StreamType,
        stream_id: Option<&str>,
    ) -> PooledPlayBin {
        self.imp().get(uri, stream_type, stream_id)
    }

    pub(crate) fn release(&self, playbin: PooledPlayBin) {
        self.imp().release(playbin)
    }
}

glib::wrapper! {
    pub struct PooledPlayBin(ObjectSubclass<pooledplaybin::PooledPlayBin>);
}

impl PooledPlayBin {
    pub(crate) fn requested_stream_id(&self) -> Option<String> {
        self.imp().requested_stream_id()
    }

    pub(crate) fn sink(&self) -> gst_app::AppSink {
        self.imp().sink()
    }

    pub(crate) fn pipeline(&self) -> gst::Pipeline {
        self.imp().pipeline()
    }

    pub(crate) fn reset(&self, uri: &str, stream_type: gst::StreamType, stream_id: Option<&str>) {
        self.imp().reset(uri, stream_type, stream_id);
    }

    pub(crate) fn stream(&self) -> Option<gst::Stream> {
        self.imp().stream()
    }

    pub(crate) fn uridecodebin(&self) -> gst::Element {
        self.imp().uridecodebin()
    }

    pub(crate) fn new(
        uri: &str,
        stream_type: gst::StreamType,
        stream_id: Option<&str>,
    ) -> PooledPlayBin {
        let this: PooledPlayBin = glib::Object::new();

        this.reset(uri, stream_type, stream_id);

        this
    }

}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "playbinpoolsrc",
        gst::Rank::None,
        PlaybinPoolSrc::static_type(),
    )
}
