// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;
use gst_validate::prelude::*;
use std::sync::Once;

use super::pool;

static REGISTER_ACTIONS: Once = Once::new();

pub fn register_validate_actions() -> Result<(), glib::BoolError> {
    REGISTER_ACTIONS.call_once(|| {
        gst_validate::ActionTypeBuilder::new(
            "check-uridecodepool-pipelines",
            |_scenario, action| {
                let pool = pool::PIPELINE_POOL_POOL.lock().unwrap().clone();
                let (total, running, prepared) = pool.pipeline_count();

                gst::info!(
                    crate::uridecodepool::pool::CAT,
                    "Global uridecodepool pipeline counts: total={}, running={}, prepared={}",
                    total,
                    running,
                    prepared
                );

                let structure = action.structure().unwrap();
                if let Ok(expected_total) = structure.get::<i32>("expected-total") {
                    if expected_total >= 0 && total != expected_total as usize {
                        return Err(gst_validate::ActionError::Error(format!(
                            "Expected {} total pipelines, got {}",
                            expected_total, total
                        )));
                    }
                }

                if let Ok(expected_running) = structure.get::<i32>("expected-running") {
                    if expected_running >= 0 && running != expected_running as usize {
                        return Err(gst_validate::ActionError::Error(format!(
                            "Expected {} running pipelines, got {}",
                            expected_running, running
                        )));
                    }
                }

                if let Ok(expected_prepared) = structure.get::<i32>("expected-prepared") {
                    if expected_prepared >= 0 && prepared != expected_prepared as usize {
                        return Err(gst_validate::ActionError::Error(format!(
                            "Expected {} prepared pipelines, got {}",
                            expected_prepared, prepared
                        )));
                    }
                }

                Ok(gst_validate::ActionSuccess::Ok)
            },
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "expected-total",
                "Expected total number of pipelines",
            )
            .add_type("int")
            .default_value("-1")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "expected-running",
                "Expected number of running pipelines",
            )
            .add_type("int")
            .default_value("-1")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "expected-prepared",
                "Expected number of prepared+pooled pipelines",
            )
            .add_type("int")
            .default_value("-1")
            .build(),
        )
        .build();

        gst_validate::ActionTypeBuilder::new(
            "check-uridecodepool-pipeline-use-count",
            |_scenario, action| {
                let structure = action.structure().unwrap();
                let uri = structure.get::<String>("uri").expect("uri is mandatory");
                let expected_reuse_count = structure
                    .get::<i32>("expected-use-count")
                    .expect("expected-use-count is mandatory");

                if expected_reuse_count < 0 {
                    return Err(gst_validate::ActionError::Error(
                        "expected-use-count must be >= 0".to_string()
                    ));
                }

                let scenario = action.scenario().ok_or_else(|| {
                    gst_validate::ActionError::Error("No scenario found".to_string())
                })?;
                let pipeline = gst_validate::prelude::ScenarioExt::pipeline(&scenario).ok_or_else(|| {
                    gst_validate::ActionError::Error("No pipeline found in scenario".to_string())
                })?;

                let mut found_element = None;
                let mut elements_to_check = vec![pipeline.clone().upcast::<gst::Element>()];
                while let Some(element) = elements_to_check.pop() {
                    if let Some(uridecodepoolsrc) = element.downcast_ref::<crate::uridecodepool::UriDecodePoolSrc>() {
                        if uridecodepoolsrc.uri().as_ref() == Some(&uri) {
                            if found_element.is_some() {
                                return Err(gst_validate::ActionError::Error(
                                    "Multiple uridecodepoolsrc elements found with same URI. Consider using ?id=unique_id in URI".to_string()
                                ));
                            }
                            found_element = Some(uridecodepoolsrc.clone());
                        }

                        if let Some(pipeline) = uridecodepoolsrc.pipeline() {
                            elements_to_check.push(pipeline.upcast::<gst::Element>());
                        }
                    }

                    if let Ok(bin) = element.dynamic_cast::<gst::Bin>() {
                        let iter = bin.iterate_elements();
                        for child in iter {
                            if let Ok(child) = child {
                                elements_to_check.push(child);
                            }
                        }
                    }
                }

                let Some(uridecodepoolsrc) = found_element else {
                    return Err(gst_validate::ActionError::Error(format!(
                        "No uridecodepoolsrc found with URI: {}",
                        uri
                    )));
                };

                let pipeline = uridecodepoolsrc.imp().decoderpipe()
                    .ok_or_else(|| {
                        gst_validate::ActionError::Error(
                            "Failed to get pipeline property from uridecodepoolsrc".to_string()
                        )
                    })?;

                let actual_reuse_count = pipeline.imp().reuse_count();

                if actual_reuse_count != expected_reuse_count {
                    return Err(gst_validate::ActionError::Error(format!(
                        "Expected pipeline reuse count {}, got {}",
                        expected_reuse_count, actual_reuse_count
                    )));
                }

                gst::info!(
                    crate::uridecodepool::pool::CAT,
                    "Pipeline for URI {} has been reused {} times as expected",
                    uri,
                    expected_reuse_count
                );

                Ok(gst_validate::ActionSuccess::Ok)
            },
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new("uri", "URI to check for pipeline reuse")
                .add_type("string")
                .mandatory()
                .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "expected-use-count",
                "Expected number of times the pipeline has been reused",
            )
            .add_type("int")
            .mandatory()
            .build(),
        )
        .build();
    });

    Ok(())
}
