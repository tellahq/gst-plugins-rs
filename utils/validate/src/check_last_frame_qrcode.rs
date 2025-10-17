// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use std::sync::Once;

static REGISTER_ACTIONS: Once = Once::new();

/// Find a sink element in the pipeline based on the given criteria
fn find_sink(
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

fn decode_qrcode_from_sample(sample: &gst::Sample) -> Result<String, String> {
    let buffer_ref = sample.buffer().ok_or("Sample has no buffer")?;
    let caps = sample.caps().ok_or("Sample has no caps")?;

    let in_info = gst_video::VideoInfo::from_caps(caps)
        .map_err(|_| "Failed to parse video info from caps")?;

    let width = in_info.width();
    let height = in_info.height();

    let out_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray8, width, height)
        .fps(in_info.fps())
        .build()
        .map_err(|_| "Failed to create GRAY8 VideoInfo")?;

    let converter = gst_video::VideoConverter::new(&in_info, &out_info, None)
        .map_err(|e| format!("Failed to create VideoConverter: {}", e))?;

    let in_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer_ref, &in_info)
        .map_err(|e| format!("Failed to map input buffer as VideoFrame: {}", e))?;

    let gray_vec = vec![0u8; out_info.size()];
    let out_buffer = gst::Buffer::from_mut_slice(gray_vec);
    let mut out_frame = gst_video::VideoFrame::from_buffer_writable(out_buffer, &out_info)
        .map_err(|_| "Failed to map output buffer as writable VideoFrame")?;

    {
        let mut out_frame_ref = out_frame.as_mut_video_frame_ref();
        converter.frame_ref(&in_frame, &mut out_frame_ref);
    }

    let out_buffer = out_frame.into_buffer();

    let gray_vec = out_buffer
        .try_into_inner::<Vec<u8>>()
        .map_err(|_| "Failed to get buffer data")?;
    let gray_image = image::ImageBuffer::from_raw(width, height, gray_vec)
        .ok_or("Failed to create image buffer from gray data")?;

    let mut img = rqrr::PreparedImage::prepare(gray_image);
    let grids = img.detect_grids();

    if grids.is_empty() {
        return Err("No QR code found in frame".to_string());
    }

    // Decode the first QR code found
    let (_meta, content) = grids[0]
        .decode()
        .map_err(|e| format!("Failed to decode QR code: {:?}", e))?;

    Ok(content)
}

fn check_last_frame_qrcode(
    scenario: &gst_validate::Scenario,
    action: &gst_validate::Action,
) -> Result<gst_validate::ActionSuccess, gst_validate::ActionError> {
    let structure = action.structure().unwrap();

    let pipeline = gst_validate::prelude::ScenarioExt::pipeline(scenario)
        .ok_or_else(|| gst_validate::ActionError::Error("No pipeline available".to_string()))?
        .downcast::<gst::Pipeline>()
        .expect("Pipeline element is not a gst::Pipeline");

    let sink_name = structure.get::<String>("sink-name").ok();
    let factory_name = structure.get::<String>("sink-factory-name").ok();
    let caps = structure.get::<gst::Caps>("sinkpad-caps").ok();

    let sink = find_sink(
        &pipeline,
        sink_name.as_deref(),
        factory_name.as_deref(),
        &caps,
    )
    .map_err(gst_validate::ActionError::Error)?;

    let sample: gst::Sample = sink
        .property::<Option<gst::Sample>>("last-sample")
        .ok_or_else(|| {
            gst_validate::ActionError::Error(format!(
                "Could not get 'last-sample' from sink '{}'. \
                             Make sure the 'enable-last-sample' property is set to TRUE!",
                sink.name()
            ))
        })?;

    let decoded_data =
        decode_qrcode_from_sample(&sample).map_err(gst_validate::ActionError::Error)?;

    // Get expected data
    let expected_data = structure.get::<String>("expected-data").map_err(|_| {
        gst_validate::ActionError::Error("Missing required parameter 'expected-data'".to_string())
    })?;

    // Compare
    if decoded_data != expected_data {
        return Err(gst_validate::ActionError::Error(format!(
            "QR code data mismatch: expected '{}', got '{}'",
            expected_data, decoded_data
        )));
    }

    gst::debug!(
        gst::CAT_DEFAULT,
        obj = scenario,
        "Successfully validated QR code data: '{}'",
        decoded_data
    );

    Ok(gst_validate::ActionSuccess::Ok)
}

pub fn register_validate_actions(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    REGISTER_ACTIONS.call_once(|| {
        gst_validate::ActionTypeBuilder::new(
            "check-last-frame-qrcode",
            |scenario, action| check_last_frame_qrcode(scenario, action)
        )
        .implementer_namespace(plugin.name().as_str())
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "sink-name",
                "The name of the sink element to check sample on",
            )
            .add_type("string")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "sink-factory-name",
                "The factory name of the sink element to check sample on",
            )
            .add_type("string")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "sinkpad-caps",
                "The caps (as string) of the sink pad to check",
            )
            .add_type("GstCaps")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "expected-data",
                "The expected QR code data content",
            )
            .add_type("string")
            .mandatory()
            .build(),
        )
        .description(
            "Checks that a QR code in the last frame of the specified sink contains the expected data. \
             This allows validating QR code generation in video streams."
        )
        .flags(gst_validate::ActionTypeFlags::CHECK)
        .build();
    });

    Ok(())
}
