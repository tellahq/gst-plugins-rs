use ges::prelude::*;
use gst::glib;
use gst::subclass::prelude::*;

mod imp;
use crate::CAT;

#[cfg(feature = "validate")]
pub(crate) mod validate;

glib::wrapper! {
    pub struct SubtimelinePrimaryManager(ObjectSubclass<imp::SubtimelinePrimaryManager>)
        @extends gst::Object;
}

// SAFETY: GESTimeline is not Send+Sync but the APIs we use are thread safe and
// there are runtime checks for the others
unsafe impl Send for SubtimelinePrimaryManager {}
unsafe impl Sync for SubtimelinePrimaryManager {}

impl SubtimelinePrimaryManager {
    pub fn get() -> Self {
        static INSTANCE: std::sync::OnceLock<SubtimelinePrimaryManager> =
            std::sync::OnceLock::new();

        INSTANCE
            .get_or_init(|| {
                let res: Self = glib::Object::builder().build();
                res.set_object_flags(gst::ObjectFlags::MAY_BE_LEAKED);
                gst::debug!(
                    CAT,
                    obj = &res,
                    "Created SubtimelinePrimaryManager singleton instance",
                );
                res
            })
            .clone()
    }

    pub fn register_primary(
        &self,
        primary_id: &str,
        timeline: &ges::Timeline,
    ) -> Result<(), glib::Error> {
        self.imp().register_primary(primary_id, timeline)
    }

    pub fn unregister_primary(&self, primary_id: &str) -> Result<(), glib::Error> {
        self.imp().unregister_primary(primary_id)
    }

    pub fn get_primary(&self, primary_id: &str) -> Option<ges::Timeline> {
        self.imp().get_primary(primary_id)
    }

    pub fn make_replica(
        &self,
        primary_id: &str,
        timeline: &ges::Timeline,
    ) -> Result<(), glib::Error> {
        self.imp().make_replica(primary_id, timeline)
    }

    pub fn is_primary(&self, timeline: &ges::Timeline) -> bool {
        self.imp().is_primary(timeline)
    }

    pub fn get_primary_id(&self, timeline: &ges::Timeline) -> Option<String> {
        self.imp().get_primary_id(timeline)
    }
}
