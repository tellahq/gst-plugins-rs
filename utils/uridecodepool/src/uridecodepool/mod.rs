// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

mod imp;
mod pool;
mod decoderpipeline;

glib::wrapper! {
    pub struct UriDecodePoolSrc(ObjectSubclass<imp::UriDecodePoolSrc>)
        @extends gst_base::BaseSrc, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

glib::wrapper! {
    pub struct PlaybinPool(ObjectSubclass<pool::PlaybinPool>);
}

impl PlaybinPool {
    pub(crate) fn get_decoderpipe(&self, src: &UriDecodePoolSrc) -> DecoderPipeline {
        self.imp().get(src)
    }

    pub(crate) fn release(&self, decoderpipe: DecoderPipeline) {
        self.imp().release(decoderpipe)
    }
}

glib::wrapper! {
    pub struct DecoderPipeline(ObjectSubclass<decoderpipeline::DecoderPipeline>);
}

impl DecoderPipeline {
    pub(crate) fn requested_stream_id(&self) -> Option<String> {
        self.imp().requested_stream_id()
    }

    pub(crate) fn sink(&self) -> gst_app::AppSink {
        self.imp().sink()
    }

    pub(crate) fn pipeline(&self) -> gst::Pipeline {
        self.imp().pipeline()
    }

    pub(crate) fn reset(&self, uri: &str, caps: &gst::Caps, stream_id: Option<&str>) {
        self.imp().reset(uri, caps, stream_id);
    }

    pub(crate) fn stream(&self) -> Option<gst::Stream> {
        self.imp().stream()
    }

    pub(crate) fn uridecodebin(&self) -> gst::Element {
        self.imp().uridecodebin()
    }

    pub(crate) fn new(
        uri: &str,
        caps: &gst::Caps,
        stream_id: Option<&str>,
        pool: &PlaybinPool,
    ) -> DecoderPipeline {
        let this: DecoderPipeline = glib::Object::builder().property("pool", pool).build();

        this.reset(uri, caps, stream_id);

        this
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "uridecodepoolsrc",
        gst::Rank::NONE,
        UriDecodePoolSrc::static_type(),
    )
}
