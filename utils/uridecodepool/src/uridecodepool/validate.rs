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
    });

    Ok(())
}
