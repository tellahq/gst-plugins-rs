// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst_video::prelude::*;
use image::GenericImageView;
use std::sync::{LazyLock, Once};

static REGISTER_ACTIONS: Once = Once::new();

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rsvalidate",
        gst::DebugColorFlags::empty(),
        Some("GStreamer Validate Rust Plugin"),
    )
});

/// Extract an RGB/RGBA image from a GStreamer sample
fn extract_image_from_sample(sample: &gst::Sample) -> Result<image::DynamicImage, String> {
    let buffer_ref = sample.buffer().ok_or("Sample has no buffer")?;
    let caps = sample.caps().ok_or("Sample has no caps")?;

    let video_info = gst_video::VideoInfo::from_caps(caps)
        .map_err(|_| "Failed to parse video info from caps")?;

    let width = video_info.width();
    let height = video_info.height();
    let format = video_info.format();

    // Map the frame as readable
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer_ref, &video_info)
        .map_err(|e| format!("Failed to map buffer as VideoFrame: {}", e))?;

    // Convert to image::DynamicImage based on format
    match format {
        gst_video::VideoFormat::Rgb => {
            let data = frame
                .plane_data(0)
                .map_err(|_| "Failed to get plane data")?;
            let stride = frame.plane_stride()[0] as u32;

            // If stride equals width * 3, we can use the data directly
            if stride == width * 3 {
                let img = image::RgbImage::from_raw(width, height, data.to_vec())
                    .ok_or("Failed to create RGB image")?;
                Ok(image::DynamicImage::ImageRgb8(img))
            } else {
                // Need to copy row by row to remove padding
                let mut rgb_data = Vec::with_capacity((width * height * 3) as usize);
                for y in 0..height {
                    let row_start = (y * stride) as usize;
                    let row_end = row_start + (width * 3) as usize;
                    rgb_data.extend_from_slice(&data[row_start..row_end]);
                }
                let img = image::RgbImage::from_raw(width, height, rgb_data)
                    .ok_or("Failed to create RGB image from strided data")?;
                Ok(image::DynamicImage::ImageRgb8(img))
            }
        }
        gst_video::VideoFormat::Rgba => {
            let data = frame
                .plane_data(0)
                .map_err(|_| "Failed to get plane data")?;
            let stride = frame.plane_stride()[0] as u32;

            if stride == width * 4 {
                let img = image::RgbaImage::from_raw(width, height, data.to_vec())
                    .ok_or("Failed to create RGBA image")?;
                Ok(image::DynamicImage::ImageRgba8(img))
            } else {
                // Need to copy row by row to remove padding
                let mut rgba_data = Vec::with_capacity((width * height * 4) as usize);
                for y in 0..height {
                    let row_start = (y * stride) as usize;
                    let row_end = row_start + (width * 4) as usize;
                    rgba_data.extend_from_slice(&data[row_start..row_end]);
                }
                let img = image::RgbaImage::from_raw(width, height, rgba_data)
                    .ok_or("Failed to create RGBA image from strided data")?;
                Ok(image::DynamicImage::ImageRgba8(img))
            }
        }
        _ => Err(format!(
            "Unsupported video format: {:?}. Please convert to RGB or RGBA in the pipeline.",
            format
        )),
    }
}

/// Compare two images and optionally save a diff heatmap
fn compare_images(
    reference: &image::DynamicImage,
    actual: &image::DynamicImage,
    metric: &str,
    diff_output: Option<&str>,
) -> Result<f64, String> {
    // Check dimensions match
    if reference.dimensions() != actual.dimensions() {
        return Err(format!(
            "Image dimensions mismatch: reference is {}x{}, actual is {}x{}",
            reference.width(),
            reference.height(),
            actual.width(),
            actual.height()
        ));
    }

    // Perform comparison based on metric and image type
    let result = match (reference, actual) {
        (image::DynamicImage::ImageRgb8(ref_img), image::DynamicImage::ImageRgb8(act_img)) => {
            match metric {
                "rms" => image_compare::rgb_hybrid_compare(ref_img, act_img)
                    .map_err(|e| format!("RMS comparison failed: {}", e))?,
                "mssim" => {
                    let gray_ref = image::DynamicImage::ImageRgb8(ref_img.clone()).to_luma8();
                    let gray_act = image::DynamicImage::ImageRgb8(act_img.clone()).to_luma8();
                    image_compare::gray_similarity_structure(
                        &image_compare::Algorithm::MSSIMSimple,
                        &gray_ref,
                        &gray_act,
                    )
                    .map_err(|e| format!("MSSIM comparison failed: {}", e))?
                }
                _ => return Err(format!("Unknown metric: {}", metric)),
            }
        }
        (image::DynamicImage::ImageRgba8(ref_img), image::DynamicImage::ImageRgba8(act_img)) => {
            match metric {
                "rms" => image_compare::rgba_hybrid_compare(ref_img, act_img)
                    .map_err(|e| format!("RMS comparison failed: {}", e))?,
                "mssim" => {
                    let gray_ref = image::DynamicImage::ImageRgba8(ref_img.clone()).to_luma8();
                    let gray_act = image::DynamicImage::ImageRgba8(act_img.clone()).to_luma8();
                    image_compare::gray_similarity_structure(
                        &image_compare::Algorithm::MSSIMSimple,
                        &gray_ref,
                        &gray_act,
                    )
                    .map_err(|e| format!("MSSIM comparison failed: {}", e))?
                }
                _ => return Err(format!("Unknown metric: {}", metric)),
            }
        }
        _ => {
            return Err(
                "Image format mismatch: both images must be RGB or both must be RGBA".to_string(),
            )
        }
    };

    let score = result.score;

    // Save diff heatmap if requested
    if let Some(output_path) = diff_output {
        let diff_image = result.image.to_color_map();
        diff_image
            .save(output_path)
            .map_err(|e| format!("Failed to save diff heatmap to '{}': {}", output_path, e))?;
        actual
            .save(format!("{}_actual.png", output_path))
            .map_err(|e| format!("Failed to save actual image: {}", e))?;
        gst::info!(CAT, "Saved diff heatmap to '{}'", output_path);
    }

    Ok(score)
}

fn compare_last_frame(
    scenario: &gst_validate::Scenario,
    action: &gst_validate::Action,
) -> Result<gst_validate::ActionSuccess, gst_validate::ActionError> {
    let structure = action.structure().unwrap();

    let pipeline = gst_validate::prelude::ScenarioExt::pipeline(scenario)
        .ok_or_else(|| gst_validate::ActionError::Error("No pipeline available".to_string()))?
        .downcast::<gst::Pipeline>()
        .expect("Pipeline element is not a gst::Pipeline");

    // Get action parameters
    let sink_name = structure.get::<String>("sink-name").ok();
    let factory_name = structure.get::<String>("sink-factory-name").ok();
    let caps = structure.get::<gst::Caps>("sinkpad-caps").ok();

    let reference_file = structure.get::<String>("reference-file").map_err(|_| {
        gst_validate::ActionError::Error("Missing required parameter 'reference-file'".to_string())
    })?;

    let threshold = structure.get::<f64>("threshold").map_err(|_| {
        gst_validate::ActionError::Error("Missing required parameter 'threshold'".to_string())
    })?;

    let metric = structure
        .get::<String>("metric")
        .unwrap_or_else(|_| "rms".to_string());

    let diff_output = structure.get::<String>("diff-output").ok();

    // Find sink element
    let sink = crate::utils::find_sink(
        &pipeline,
        sink_name.as_deref(),
        factory_name.as_deref(),
        &caps,
    )
    .map_err(gst_validate::ActionError::Error)?;

    // Get last sample
    let sample: gst::Sample = sink
        .property::<Option<gst::Sample>>("last-sample")
        .ok_or_else(|| {
            gst_validate::ActionError::Error(format!(
                "Could not get 'last-sample' from sink '{}'. \
                 Make sure the 'enable-last-sample' property is set to TRUE!",
                sink.name()
            ))
        })?;

    // Load reference image
    let reference_image = image::open(&reference_file).map_err(|e| {
        gst_validate::ActionError::Error(format!(
            "Failed to load reference image '{}': {}",
            reference_file, e
        ))
    })?;

    // Extract actual frame from sample
    let actual_image =
        extract_image_from_sample(&sample).map_err(gst_validate::ActionError::Error)?;

    // Compare images
    let score = compare_images(
        &reference_image,
        &actual_image,
        &metric,
        diff_output.as_deref(),
    )
    .map_err(gst_validate::ActionError::Error)?;

    // Check threshold
    // For RMS/MSSIM, score ranges from -1.0 to 1.0, where 1.0 is perfect match
    // We want to fail if the score is below the threshold (i.e., images are too different)
    // But for better UX, we report the "difference" which is (1.0 - score)
    let difference = 1.0 - score;

    if difference > threshold {
        return Err(gst_validate::ActionError::Error(format!(
            "Frame comparison failed: difference {:.6} exceeds threshold {:.6} (metric: {}, reference: {})",
            difference, threshold, metric, reference_file
        )));
    }

    gst::info!(
        CAT,
        obj = scenario,
        "Frame comparison passed: difference {:.6} within threshold {:.6} (metric: {})",
        difference,
        threshold,
        metric
    );

    Ok(gst_validate::ActionSuccess::Ok)
}

pub fn register_validate_actions(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    REGISTER_ACTIONS.call_once(|| {
        gst_validate::ActionTypeBuilder::new(
            "compare-last-frame",
            |scenario, action| compare_last_frame(scenario, action)
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
                "reference-file",
                "Path to reference PNG image to compare against",
            )
            .add_type("string")
            .mandatory()
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "threshold",
                "Maximum acceptable difference (0.0-1.0, where 0.0 means identical)",
            )
            .add_type("double")
            .mandatory()
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "metric",
                "Comparison metric to use: 'rms' (default) for pixel-level differences, or 'mssim' for structural similarity",
            )
            .add_type("string")
            .default_value("rms")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "diff-output",
                "Optional path to save diff heatmap PNG (black=identical, bright=different)",
            )
            .add_type("string")
            .build(),
        )
        .description(
            "Compares the last frame from a sink against a reference PNG image. \
             The frame must be in RGB or RGBA format. \
             Returns the difference score and optionally generates a visual diff heatmap."
        )
        .flags(gst_validate::ActionTypeFlags::CHECK)
        .build();
    });

    Ok(())
}
