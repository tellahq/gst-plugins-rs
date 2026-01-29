// SPDX-License-Identifier: MPL-2.0
//
// Minimal reproducer for the GL transparency bug.
//
// 2 layers composited via glvideomixer:
//   0: Solid purple background (videotestsrc)
//   1: Transparent input → skiareshapegl with draw signal (draws a small rect)
//
// The transparent areas of layer 1 should be fully see-through, but instead
// appear greyish when rendered through GL.

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
    let context_boxed = args[4]
        .get::<gstskia::SkiaContext>()
        .expect("arg 4 is SkiaContext");

    let canvas = unsafe { canvas_boxed.as_ref() };
    let direct_context = unsafe { context_boxed.as_mut() };

    let width = video_info.width() as i32;
    let height = video_info.height() as i32;

    let image_info = skia::ImageInfo::new_n32_premul((width, height), None);
    let mut surface = if let Some(ctx) = direct_context {
        skia::gpu::surfaces::render_target(
            ctx,
            skia::gpu::Budgeted::Yes,
            &image_info,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_or_else(|| skia::surfaces::raster(&image_info, None, None).unwrap())
    } else {
        skia::surfaces::raster(&image_info, None, None).unwrap()
    };

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
fn test_gl_transparency() {
    init();

    let pipeline = gst::Pipeline::new();

    let mix = gst::ElementFactory::make("glvideomixer")
        .name("mix")
        .build()
        .unwrap();
    let gldownload = gst::ElementFactory::make("gldownload")
        .build()
        .unwrap();
    let videoconvert = gst::ElementFactory::make("videoconvert")
        .build()
        .unwrap();
    let jpegenc = gst::ElementFactory::make("jpegenc").build().unwrap();
    let filesink = gst::ElementFactory::make("filesink")
        .property("location", "/tmp/gl_transparency_minimal.jpg")
        .build()
        .unwrap();

    pipeline
        .add_many([&mix, &gldownload, &videoconvert, &jpegenc, &filesink])
        .unwrap();
    gst::Element::link_many([&mix, &gldownload, &videoconvert, &jpegenc, &filesink])
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
    let bg_upload = gst::ElementFactory::make("glupload").build().unwrap();
    let bg_colorconvert = gst::ElementFactory::make("glcolorconvert").build().unwrap();

    pipeline
        .add_many([&bg_src, &bg_capsfilter, &bg_upload, &bg_colorconvert])
        .unwrap();
    gst::Element::link_many([&bg_src, &bg_capsfilter, &bg_upload, &bg_colorconvert])
        .unwrap();

    let bg_pad = mix.request_pad_simple("sink_%u").unwrap();
    bg_pad.set_property("zorder", 1u32);
    bg_pad.set_property("width", 320i32);
    bg_pad.set_property("height", 240i32);
    bg_colorconvert
        .static_pad("src")
        .unwrap()
        .link(&bg_pad)
        .unwrap();

    // Source 1: transparent input → skiareshapegl with draw signal
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
    let sub_upload = gst::ElementFactory::make("glupload").build().unwrap();
    let sub_colorconvert = gst::ElementFactory::make("glcolorconvert").build().unwrap();
    let reshape = gst::ElementFactory::make("skiareshapegl")
        .name("reshape")
        .build()
        .unwrap();

    pipeline
        .add_many([&sub_src, &sub_capsfilter, &sub_upload, &sub_colorconvert, &reshape])
        .unwrap();
    gst::Element::link_many([
        &sub_src,
        &sub_capsfilter,
        &sub_upload,
        &sub_colorconvert,
        &reshape,
    ])
    .unwrap();

    let sub_pad = mix.request_pad_simple("sink_%u").unwrap();
    sub_pad.set_property("zorder", 2u32);
    sub_pad.set_property("width", 320i32);
    sub_pad.set_property("height", 240i32);
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

    println!("Done — inspect /tmp/gl_transparency_minimal.jpg");
}
