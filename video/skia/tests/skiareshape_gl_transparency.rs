// SPDX-License-Identifier: MPL-2.0
//
// Reproduces the GL transparency bug using the real renderer's sources.
//
// 4 layers composited via glvideomixer (same as GESSmartMixer):
//   0: Purple background SVG
//   1: Screen recording (DASH)
//   2: Webcam recording (DASH)
//   3: Transparent SVG → skiareshapegl with draw signal (subtitle layer)

use std::sync::atomic::{AtomicBool, Ordering};

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

const BG_SVG_URI: &str = "data:image/svg+xml;utf8,<svg width=\"1920\" height=\"1080\" viewBox=\"0 0 1920 1080\" fill=\"none\" xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"1920\" height=\"1080\" fill=\"rgba(222,213,245,1)\"/></svg>";

const SUBTITLE_SVG_URI: &str = "data:image/svg+xml;utf8,<svg width=\"1920\" height=\"1080\" viewBox=\"0 0 1920 1080\" fill=\"none\" xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"1920\" height=\"1080\"/></svg>";

const SCREEN_URI: &str =
    "https://dev-raw.tella.dev/su_cmjb92xbx001404la9ens2g8d/output.mpd";

const WEBCAM_URI: &str =
    "https://dev-raw.tella.dev/su_cmjb92xbd001304la5br911ey/output.mpd";

/// Create a uridecodebin → [imagefreeze →] glupload → glcolorconvert chain.
/// For SVG sources, `needs_imagefreeze` should be true (single image → video).
/// Returns (uridecodebin, tail_element) where tail_element is the last element to link to the mixer.
fn make_source_chain(
    pipeline: &gst::Pipeline,
    uri: &str,
    needs_imagefreeze: bool,
    name_prefix: &str,
) -> gst::Element {
    let uridecodebin = gst::ElementFactory::make("uridecodebin")
        .name(&format!("{name_prefix}_src"))
        .property("uri", uri)
        .build()
        .unwrap();

    let glupload = gst::ElementFactory::make("glupload")
        .name(&format!("{name_prefix}_upload"))
        .build()
        .unwrap();

    let glcolorconvert = gst::ElementFactory::make("glcolorconvert")
        .name(&format!("{name_prefix}_colorconvert"))
        .build()
        .unwrap();

    if needs_imagefreeze {
        let imagefreeze = gst::ElementFactory::make("imagefreeze")
            .name(&format!("{name_prefix}_freeze"))
            .property("num-buffers", 1i32)
            .build()
            .unwrap();

        pipeline
            .add_many([&uridecodebin, &imagefreeze, &glupload, &glcolorconvert])
            .unwrap();

        gst::Element::link_many([&imagefreeze, &glupload, &glcolorconvert]).unwrap();

        let imagefreeze_weak = imagefreeze.downgrade();
        uridecodebin.connect_pad_added(move |_src, pad| {
            let Some(imagefreeze) = imagefreeze_weak.upgrade() else {
                return;
            };
            let sink_pad = imagefreeze.static_pad("sink").unwrap();
            if !sink_pad.is_linked() {
                pad.link(&sink_pad).unwrap();
            }
        });
    } else {
        pipeline
            .add_many([&uridecodebin, &glupload, &glcolorconvert])
            .unwrap();

        gst::Element::link_many([&glupload, &glcolorconvert]).unwrap();

        let glupload_weak = glupload.downgrade();
        uridecodebin.connect_pad_added(move |_src, pad| {
            let Some(glupload) = glupload_weak.upgrade() else {
                return;
            };
            let name = pad.name();
            if !name.starts_with("video") && !name.starts_with("src") {
                return;
            }
            let sink_pad = glupload.static_pad("sink").unwrap();
            if !sink_pad.is_linked() {
                pad.link(&sink_pad).unwrap();
            }
        });
    }

    glcolorconvert
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

    // Output chain: glvideomixer → identity → gldownload → videoconvert → jpegenc → filesink
    // identity with a pad probe sends EOS after the first buffer to capture a single frame.
    let mix = gst::ElementFactory::make("glvideomixer")
        .name("mix")
        .build()
        .unwrap();
    let identity = gst::ElementFactory::make("identity")
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
        .property("location", "/tmp/therealtest.jpg")
        .build()
        .unwrap();

    pipeline
        .add_many([&mix, &identity, &gldownload, &videoconvert, &jpegenc, &filesink])
        .unwrap();
    gst::Element::link_many([&mix, &identity, &gldownload, &videoconvert, &jpegenc, &filesink])
        .unwrap();

    // Lepipelinet the first buffer through, then inject EOS downstream to capture a single frame
    let got_first = AtomicBool::new(false);
    identity.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::BUFFER,
        move |pad, _info| {
            if got_first.swap(true, Ordering::SeqCst) {
                if let Some(peer) = pad.peer() {
                    peer.send_event(gst::event::Eos::new());
                }
                return gst::PadProbeReturn::Drop;
            }
            gst::PadProbeReturn::Ok
        },
    );

    // Source 0: purple background SVG (zorder=1, full frame)
    let bg_tail = make_source_chain(&pipeline, BG_SVG_URI, true, "bg");
    let bg_pad = mix.request_pad_simple("sink_%u").unwrap();
    bg_pad.set_property("zorder", 1u32);
    bg_pad.set_property("width", 1920i32);
    bg_pad.set_property("height", 1080i32);
    bg_tail.static_pad("src").unwrap().link(&bg_pad).unwrap();

    // Source 1: screen recording (zorder=2, 1545x869 at 46,46)
    let screen_tail = make_source_chain(&pipeline, SCREEN_URI, false, "screen");
    let screen_pad = mix.request_pad_simple("sink_%u").unwrap();
    screen_pad.set_property("zorder", 2u32);
    screen_pad.set_property("xpos", 46i32);
    screen_pad.set_property("ypos", 46i32);
    screen_pad.set_property("width", 1545i32);
    screen_pad.set_property("height", 869i32);
    screen_tail.static_pad("src").unwrap().link(&screen_pad).unwrap();

    // Source 2: webcam recording (zorder=3, 428x428 at 1464,620)
    let webcam_tail = make_source_chain(&pipeline, WEBCAM_URI, false, "webcam");
    let webcam_pad = mix.request_pad_simple("sink_%u").unwrap();
    webcam_pad.set_property("zorder", 3u32);
    webcam_pad.set_property("xpos", 1464i32);
    webcam_pad.set_property("ypos", 620i32);
    webcam_pad.set_property("width", 428i32);
    webcam_pad.set_property("height", 428i32);
    webcam_tail.static_pad("src").unwrap().link(&webcam_pad).unwrap();

    // Source 3: subtitle layer (zorder=4, full frame, transparent SVG → skiareshapegl)
    let sub_tail = make_source_chain(&pipeline, SUBTITLE_SVG_URI, true, "sub");
    let reshape = gst::ElementFactory::make("skiareshapegl")
        .name("reshape")
        .build()
        .unwrap();
    pipeline.add(&reshape).unwrap();
    sub_tail.link(&reshape).unwrap();

    let sub_pad = mix.request_pad_simple("sink_%u").unwrap();
    sub_pad.set_property("zorder", 4u32);
    sub_pad.set_property("width", 1920i32);
    sub_pad.set_property("height", 1080i32);
    reshape.static_pad("src").unwrap().link(&sub_pad).unwrap();

    reshape.connect("draw", false, draw_subtitle_backdrop);

    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set pipeline to Playing");

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(30)) {
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

    println!("Done — inspect test.jpg");
}
