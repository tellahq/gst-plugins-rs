use std::sync::LazyLock;

use crate::subtimelineprimarymanager::SubtimelinePrimaryManager;
use ges::prelude::*;
use ges::subclass::prelude::*;
use gst::glib::{self, translate::*, Properties};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "gessubtimelineformatter",
        gst::DebugColorFlags::empty(),
        Some("GES Subtimeline Primary Manager"),
    )
});

extern "C" {
    fn ges_project_set_loaded(
        project: *mut ges::ffi::GESProject,
        formatter: *mut ges::ffi::GESFormatter,
        error: *mut *mut glib::ffi::GError,
    ) -> glib::ffi::gboolean;
}

#[derive(Default, Properties)]
#[properties(wrapper_type = super::SubTimelineFormatter)]
pub struct SubTimelineFormatter {
    #[property(name = "subtimeline-manager", get = Self::subtimeline_manager_getter, type = SubtimelinePrimaryManager)]
    _subtimeline_manager: std::sync::OnceLock<SubtimelinePrimaryManager>,
}

impl SubTimelineFormatter {
    fn is_subtimeline_uri(uri: &str) -> bool {
        uri.starts_with("gessubtimeline:")
    }

    fn subtimeline_manager_getter(&self) -> SubtimelinePrimaryManager {
        SubtimelinePrimaryManager::get()
    }

    fn parse_primary_id_from_uri(uri: &str) -> Result<String, glib::Error> {
        if let Some(primary_id) = uri.strip_prefix("gessubtimeline:") {
            if primary_id.is_empty() {
                Err(glib::Error::new(
                    gst::CoreError::Failed,
                    "Primary ID cannot be empty in gessubtimeline: URI",
                ))
            } else {
                Ok(primary_id.to_string())
            }
        } else {
            Err(glib::Error::new(
                gst::CoreError::Failed,
                &format!("Invalid subtimeline URI format: {}", uri),
            ))
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SubTimelineFormatter {
    const NAME: &'static str = "GESSubTimelineFormatter";
    type Type = super::SubTimelineFormatter;
    type ParentType = ges::Formatter;
}

#[glib::derived_properties]
impl ObjectImpl for SubTimelineFormatter {}

impl FormatterImpl for SubTimelineFormatter {
    fn can_load_uri(&self, uri: &str) -> Result<(), glib::Error> {
        if Self::is_subtimeline_uri(uri) {
            gst::error!(
                CAT,
                imp = self,
                "SubTimelineFormatter can handle URI: {}",
                uri
            );
            Ok(())
        } else {
            gst::error!(CAT, "Can not load {uri:?}");
            Err(glib::Error::new(
                gst::CoreError::Failed,
                &format!("URI '{}' is not a subtimeline reference", uri),
            ))
        }
    }

    fn load_from_uri(&self, timeline: &ges::Timeline, uri: &str) -> Result<(), glib::Error> {
        gst::error!(CAT, imp = self, "REFCOUNT: {}", timeline.ref_count());
        gst::info!(CAT, imp = self, "Loading subtimeline from URI: {} - ", uri);

        if !Self::is_subtimeline_uri(uri) {
            return Err(glib::Error::new(
                gst::CoreError::Failed,
                "Not a subtimeline URI",
            ));
        }

        // Parse primary ID from URI
        let primary_id = Self::parse_primary_id_from_uri(uri)?;
        SubtimelinePrimaryManager::get().make_replica(&primary_id, timeline)?;

        // FIXME: Mark as loaded in an idle callback because ges_formatter_load ()
        // expects the project to be loaded only after starting its main loop,
        // in case we are in the main context already.
        let formatter = self.obj().clone().upcast::<ges::Formatter>();
        let project = timeline
            .asset()
            .unwrap()
            .downcast::<ges::Project>()
            .unwrap();
        if glib::MainContext::default().is_owner() {
            glib::source::idle_add_local_once(move || unsafe {
                ges_project_set_loaded(
                    project.to_glib_none().0,
                    formatter.to_glib_none().0,
                    std::ptr::null_mut(),
                );
            });
        } else {
            gst::debug!(
                CAT,
                "Not main context owner, calling ges_project_set_loaded directly"
            );
            unsafe {
                ges_project_set_loaded(
                    project.to_glib_none().0,
                    formatter.to_glib_none().0,
                    std::ptr::null_mut(),
                );
            }
        }

        gst::info!(
            CAT,
            imp = self,
            "Successfully loaded subtimeline replica from primary: {}",
            primary_id
        );

        Ok(())
    }

    fn save_to_uri(
        &self,
        _timeline: &ges::Timeline,
        uri: &str,
        _overwrite: bool,
    ) -> Result<(), glib::Error> {
        gst::info!(
            CAT,
            imp = self,
            "Saving {} with SubTimelineFormatter doesn't make much sense, doing nothing.",
            uri
        );
        // Save is a no-op for subtimeline replicas
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::glib;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            crate::plugin_register_static().expect("Failed to register rsges plugin");
            ges::init().unwrap();
        });
    }

    #[test]
    fn test_is_subtimeline_uri() {
        assert!(SubTimelineFormatter::is_subtimeline_uri(
            "gessubtimeline:primary1"
        ));
        assert!(SubTimelineFormatter::is_subtimeline_uri(
            "gessubtimeline:my_primary"
        ));
        assert!(!SubTimelineFormatter::is_subtimeline_uri(
            "file:///tmp/test.xges"
        ));
        assert!(!SubTimelineFormatter::is_subtimeline_uri(
            "http://example.com"
        ));
        assert!(!SubTimelineFormatter::is_subtimeline_uri(""));
    }

    #[test]
    fn test_parse_primary_id_from_uri() {
        let result = SubTimelineFormatter::parse_primary_id_from_uri("gessubtimeline:primary1");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "primary1");

        let result =
            SubTimelineFormatter::parse_primary_id_from_uri("gessubtimeline:my_primary_123");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "my_primary_123");

        let result = SubTimelineFormatter::parse_primary_id_from_uri("gessubtimeline:");
        assert!(result.is_err(), "Should fail for empty primary ID");

        let result = SubTimelineFormatter::parse_primary_id_from_uri("file:///tmp/test.xges");
        assert!(result.is_err(), "Should fail for non-subtimeline URI");
    }

    #[test]
    fn test_formatter_can_load_uri() {
        init();

        let formatter = glib::Object::new::<super::super::SubTimelineFormatter>();

        let result = formatter.imp().can_load_uri("gessubtimeline:primary1");
        assert!(result.is_ok(), "Should accept subtimeline URI");

        let result = formatter.imp().can_load_uri("file:///tmp/test.xges");
        assert!(result.is_err(), "Should reject non-subtimeline URI");

        let result = formatter.imp().can_load_uri("");
        assert!(result.is_err(), "Should reject empty URI");
    }

    #[test]
    fn test_formatter_load_from_uri() {
        init();

        let manager = SubtimelinePrimaryManager::get();

        // Test error case: nonexistent primary
        let formatter = glib::Object::new::<super::super::SubTimelineFormatter>();
        let timeline = ges::Timeline::new();

        let result = formatter
            .imp()
            .load_from_uri(&timeline, "gessubtimeline:nonexistent");
        assert!(result.is_err(), "Should fail for nonexistent primary");

        // Test error case: non-subtimeline URI
        let result = formatter
            .imp()
            .load_from_uri(&timeline, "file:///tmp/test.xges");
        assert!(result.is_err(), "Should fail for non-subtimeline URI");

        // Test success case: register a primary and load it
        let primary_timeline = ges::Timeline::new();
        manager
            .register_primary("test_primary", &primary_timeline)
            .expect("Failed to register primary");

        let replica_timeline = ges::Timeline::new();
        let result = formatter
            .imp()
            .load_from_uri(&replica_timeline, "gessubtimeline:test_primary");
        assert!(
            result.is_ok(),
            "Should succeed loading from registered primary"
        );

        // Clean up
        manager
            .unregister_primary("test_primary")
            .expect("Failed to unregister primary");
    }

    #[test]
    fn test_formatter_save_to_uri() {
        init();

        let formatter = glib::Object::new::<super::super::SubTimelineFormatter>();
        let timeline = ges::Timeline::new();

        // Save operation is a no-op for subtimeline replicas
        let result = formatter
            .imp()
            .save_to_uri(&timeline, "gessubtimeline:primary1", false);
        assert!(
            result.is_ok(),
            "Save should succeed (no-op for subtimeline replicas)"
        );
    }
}
