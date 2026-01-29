// SPDX-License-Identifier: MPL-2.0
//
// Software-only minimal reproducer for comparison with GL transparency test.
//
// 2 layers composited via skiacompositor (no GL):
//   0: Solid purple background (videotestsrc)
//   1: Transparent input → skiareshape with draw signal (subtitle backdrop)

use gst::glib;
use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstskia::plugin_register_static().unwrap();
    });
}

fn draw_subtitle_backdrop(args: &[glib::Value]) -> Option<glib::Value> {
    let video_info = args[2]
        .get::<gst_video::VideoInfo>()
        .expect("arg 2 is VideoInfo");
    let canvas_boxed = args[3]
        .get::<gstskia::SkiaCanvas>()
        .expect("arg 3 is SkiaCanvas");

    let canvas = unsafe { canvas_boxed.as_ref() };

    let width = video_info.width() as i32;
    let height = video_info.height() as i32;

    let image_info = skia::ImageInfo::new_n32_premul((width, height), None);
    let mut surface = skia::surfaces::raster(&image_info, None, None).unwrap();

    let w = width as f32;
    let h = height as f32;

    let box_width = w * 0.5;
    let box_height = h * 0.08;
    let box_left = (w - box_width) / 2.0;
    let box_top = h * 0.82;
    let border_radius = 12.0;

    let rect = skia::Rect::from_xywh(box_left, box_top, box_width, box_height);

    surface.canvas().save_layer_alpha(None, 255);

    // Shadow
    {
        let shadow_rect =
            skia::Rect::from_xywh(box_left, box_top + 20.0, box_width, box_height);

        let mut shadow_paint = skia::Paint::default();
        shadow_paint.set_color(skia::Color::from_argb(25, 0, 0, 0));
        shadow_paint.set_anti_alias(true);
        shadow_paint.set_mask_filter(skia::MaskFilter::blur(
            skia::BlurStyle::Normal,
            12.5,
            false,
        ));
        surface
            .canvas()
            .draw_round_rect(shadow_rect, border_radius, border_radius, &shadow_paint);
    }

    // Backdrop
    {
        let mut bg_paint = skia::Paint::default();
        bg_paint.set_color(skia::Color::from_argb(25, 255, 255, 255));
        bg_paint.set_anti_alias(true);
        surface
            .canvas()
            .draw_round_rect(rect, border_radius, border_radius, &bg_paint);
    }

    surface.canvas().restore();

    let image = surface.image_snapshot();

    let mut composite_paint = skia::Paint::default();
    composite_paint.set_blend_mode(skia::BlendMode::SrcOver);
    canvas.draw_image(&image, (0.0, 0.0), Some(&composite_paint));

    None
}

#[test]
fn test_sw_transparency() {
    init();

    let pipeline = gst::Pipeline::new();

    let mix = gst::ElementFactory::make("skiacompositor")
        .name("mix")
        .property_from_str("background", "Transparent")
        .build()
        .unwrap();
    let videoconvert = gst::ElementFactory::make("videoconvert")
        .build()
        .unwrap();
    let jpegenc = gst::ElementFactory::make("jpegenc").build().unwrap();
    let filesink = gst::ElementFactory::make("filesink")
        .property("location", "/tmp/sw_transparency_minimal.jpg")
        .build()
        .unwrap();

    pipeline
        .add_many([&mix, &videoconvert, &jpegenc, &filesink])
        .unwrap();
    gst::Element::link_many([&mix, &videoconvert, &jpegenc, &filesink])
        .unwrap();

    // Source 0: solid purple background
    let bg_src = gst::ElementFactory::make("videotestsrc")
        .property_from_str("pattern", "solid-color")
        .property("foreground-color", 0xFFDED5F5u32)
        .property("num-buffers", 1i32)
        .build()
        .unwrap();
    let bg_capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .width(320)
                .height(240)
                .framerate(gst::Fraction::new(1, 1))
                .build(),
        )
        .build()
        .unwrap();

    pipeline.add_many([&bg_src, &bg_capsfilter]).unwrap();
    gst::Element::link_many([&bg_src, &bg_capsfilter]).unwrap();

    let bg_pad = mix.request_pad_simple("sink_%u").unwrap();
    bg_pad.set_property("width", 320.0f32);
    bg_pad.set_property("height", 240.0f32);
    bg_capsfilter
        .static_pad("src")
        .unwrap()
        .link(&bg_pad)
        .unwrap();

    // Source 1: transparent input → skiareshape with draw signal
    let sub_src = gst::ElementFactory::make("videotestsrc")
        .property_from_str("pattern", "solid-color")
        .property("foreground-color", 0x00000000u32)
        .property("num-buffers", 1i32)
        .build()
        .unwrap();
    let sub_capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .width(320)
                .height(240)
                .framerate(gst::Fraction::new(1, 1))
                .build(),
        )
        .build()
        .unwrap();
    let reshape = gst::ElementFactory::make("skiareshape")
        .name("reshape")
        .build()
        .unwrap();

    pipeline
        .add_many([&sub_src, &sub_capsfilter, &reshape])
        .unwrap();
    gst::Element::link_many([&sub_src, &sub_capsfilter, &reshape])
        .unwrap();

    let sub_pad = mix.request_pad_simple("sink_%u").unwrap();
    sub_pad.set_property("width", 320.0f32);
    sub_pad.set_property("height", 240.0f32);
    reshape
        .static_pad("src")
        .unwrap()
        .link(&sub_pad)
        .unwrap();

    reshape.connect("draw", false, draw_subtitle_backdrop);

    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set pipeline to Playing");

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
        match msg.view() {
            gst::MessageView::Eos(..) => break,
            gst::MessageView::Error(err) => {
                eprintln!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }
            _ => {}
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Failed to set pipeline to Null");

    println!("Done — inspect /tmp/sw_transparency_minimal.jpg");
}
