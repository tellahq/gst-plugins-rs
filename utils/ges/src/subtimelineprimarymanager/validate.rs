// SPDX-License-Identifier: MPL-2.0

use ges::prelude::*;
use gst::glib;
use std::sync::Once;

use super::SubtimelinePrimaryManager;

static REGISTER_ACTIONS: Once = Once::new();

pub fn register_validate_actions() -> Result<(), glib::BoolError> {
    REGISTER_ACTIONS.call_once(|| {
        gst_validate::ActionTypeBuilder::new(
            "register-subtimeline-primary",
            |_scenario, action| {
                let structure = action.structure().unwrap();
                let primary_id = structure
                    .get::<String>("primary-id")
                    .expect("primary-id is mandatory");

                // Create a new timeline or load from URI if provided
                let timeline = if let Ok(uri) = structure.get::<String>("uri") {
                    // Load timeline from file
                    ges::Timeline::from_uri(&uri).map_err(|err| {
                        gst_validate::ActionError::Error(format!(
                            "Failed to load timeline from '{}': {}",
                            uri, err
                        ))
                    })?
                } else {
                    // Create a new blank timeline
                    ges::Timeline::new()
                };

                // Register the timeline as a primary
                timeline
                    .register_as_subtimeline_primary(&primary_id)
                    .map_err(|err| {
                        gst_validate::ActionError::Error(format!(
                            "Failed to register timeline as primary '{}': {}",
                            primary_id, err
                        ))
                    })?;

                gst::info!(
                    gst::CAT_DEFAULT,
                    "Successfully registered timeline as subtimeline primary with ID: {}",
                    primary_id
                );

                Ok(gst_validate::ActionSuccess::Ok)
            },
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "primary-id",
                "Unique identifier for this primary timeline",
            )
            .add_type("string")
            .mandatory()
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "uri",
                "Optional URI to load the timeline from. If not specified, a blank timeline is created.",
            )
            .add_type("string")
            .build(),
        )
        .build();

        gst_validate::ActionTypeBuilder::new(
            "unregister-subtimeline-primary",
            |_scenario, action| {
                let structure = action.structure().unwrap();
                let primary_id = structure
                    .get::<String>("primary-id")
                    .expect("primary-id is mandatory");

                // Get the manager singleton and unregister
                let manager = SubtimelinePrimaryManager::get();
                manager.unregister_primary(&primary_id).map_err(|err| {
                    gst_validate::ActionError::Error(format!(
                        "Failed to unregister primary '{}': {}",
                        primary_id, err
                    ))
                })?;

                gst::info!(
                    gst::CAT_DEFAULT,
                    "Successfully unregistered subtimeline primary with ID: {}",
                    primary_id
                );

                Ok(gst_validate::ActionSuccess::Ok)
            },
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "primary-id",
                "The ID of the primary timeline to unregister",
            )
            .add_type("string")
            .mandatory()
            .build(),
        )
        .build();
    });

    Ok(())
}
