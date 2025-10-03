// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstskia::plugin_register_static().unwrap();
        gst::meta::CustomMeta::register("GstSkiaReshapeTestMeta", &[]);
    });
}

#[test]
fn test_draw_signal() {
    init();

    let mut h = gst_check::Harness::with_padnames("skiareshape", Some("sink"), Some("src"));

    // Get the element to connect to the signal
    let element = h.element().unwrap();

    // Set caps for RGBA format
    let caps = gst_video::VideoCapsBuilder::new()
        .format(gst_video::VideoFormat::Rgba)
        .width(320)
        .height(240)
        .framerate(gst::Fraction::new(30, 1))
        .build();

    h.set_sink_caps(caps.clone());
    h.set_src_caps(caps);

    // Create a white buffer (all pixels set to 255)
    let mut buffer = gst::Buffer::with_size(320 * 240 * 4).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        let _ = gst::meta::CustomMeta::add(buffer, "GstSkiaReshapeTestMeta").unwrap();
        buffer.set_pts(gst::ClockTime::ZERO);
        let mut map = buffer.map_writable().unwrap();
        // Fill with white (RGBA: 255, 255, 255, 255)
        map.as_mut_slice().fill(255);
    }

    // Connect to the draw signal
    element.connect("draw", false, |args| {
        let buffer = args[1].get::<gstskia::BufferRef>().unwrap();
        let video_info = args[2].get::<gst_video::VideoInfo>().unwrap();
        let canvas_boxed = args[3].get::<gstskia::SkiaCanvas>().unwrap();
        let _context_boxed = args[4].get::<gstskia::SkiaContext>().unwrap();

        // SAFETY: The canvas pointer is valid for the duration of the signal handler
        let canvas = unsafe { canvas_boxed.as_ref() };

        assert_eq!(video_info.width(), 320);
        assert_eq!(video_info.height(), 240);

        // SAFETY: The buffer pointer is valid for the duration of the signal handler
        let buffer_ref = unsafe { buffer.as_ref() };
        gst::meta::CustomMeta::from_buffer(buffer_ref, "GstSkiaReshapeTestMeta")
            .expect("Meta missing");

        // Paint the entire canvas green
        let paint = skia::Paint::new(skia::Color4f::new(0.0, 1.0, 0.0, 1.0), None);
        canvas.draw_paint(&paint);

        None
    });

    // Push the white buffer
    let result = h.push(buffer);
    assert_eq!(
        result,
        Ok(gst::FlowSuccess::Ok),
        "Failed to push buffer: {:?}",
        result
    );

    // Pull the output buffer
    let output_buffer = h.pull().unwrap();

    // Verify the output buffer is green
    let map = output_buffer.map_readable().unwrap();
    let pixels = map.as_slice();

    // Check a few sample pixels to verify they're green (RGBA: 0, 255, 0, 255)
    for y in [0, 120, 239] {
        for x in [0, 160, 319] {
            let offset = (y * 320 + x) * 4;
            assert_eq!(
                pixels[offset], 0,
                "Red channel should be 0 at pixel ({}, {})",
                x, y
            );
            assert_eq!(
                pixels[offset + 1],
                255,
                "Green channel should be 255 at pixel ({}, {})",
                x,
                y
            );
            assert_eq!(
                pixels[offset + 2],
                0,
                "Blue channel should be 0 at pixel ({}, {})",
                x,
                y
            );
            assert_eq!(
                pixels[offset + 3],
                255,
                "Alpha channel should be 255 at pixel ({}, {})",
                x,
                y
            );
        }
    }
}
