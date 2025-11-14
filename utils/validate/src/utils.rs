// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

/// Find a sink element in the pipeline based on the given criteria
pub fn find_sink(
    pipeline: &gst::Pipeline,
    sink_name: Option<&str>,
    factory_name: Option<&str>,
    caps: &Option<gst::Caps>,
) -> Result<gst::Element, String> {
    let found_sink = pipeline.iterate_recurse().find(|element| {
        if !element.has_property_with_type("last-sample", gst::Sample::static_type()) {
            return false;
        }

        if let Some(name) = sink_name {
            return element.name() == name;
        }

        if let Some(factory_name) = factory_name {
            return element
                .factory()
                .is_some_and(|factory| factory.name() == factory_name);
        }

        // Check caps if specified
        if let Some(ref expected_caps) = caps {
            // Check all sink pads
            for pad in element.iterate_sink_pads() {
                let pad = match pad {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if let Some(pad_caps) = pad.current_caps() {
                    if expected_caps.can_intersect(&pad_caps) {
                        return true;
                    }
                }
            }
        }

        // If nothing is specified, just get the first matching sink
        true
    });

    found_sink.ok_or_else(|| "No matching sink found in pipeline".to_string())
}
