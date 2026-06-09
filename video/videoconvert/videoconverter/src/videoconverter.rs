// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst_video::prelude::*;
use yuv::{
    bgr_to_bgra, bgr_to_rgb, bgr_to_rgba, bgra_to_bgr, bgra_to_rgb, bgra_to_rgba, rgb_to_bgr,
    rgb_to_bgra, rgb_to_rgba, rgba_to_bgr, rgba_to_bgra, rgba_to_rgb, YuvConversionMode, YuvRange,
    YuvStandardMatrix,
};

use crate::CAT;

/// YUV chroma subsampling patterns
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YuvSubsampling {
    S400, // Grayscale (no chroma)
    S420, // 4:2:0
    S422, // 4:2:2
    S444, // 4:4:4
}

/// YUV memory layout
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YuvLayout {
    Planar,      // Y, U, V in separate planes (I420, YV12, Y42B, Y444)
    PlanarAlpha, // Y, U, V, A in separate planes (A420, A422, A444)
    SemiPlanar,  // Y in one plane, UV interleaved in another (NV12, NV21, NV16, NV61, NV24)
    Packed,      // YUV interleaved in single plane (YUY2, UYVY, VYUY, YVYU)
    Grayscale,   // Single Y plane only (GRAY8)
}

/// UV component order for semi-planar formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UvOrder {
    Uv, // UV interleaved (NV12, NV16, NV24)
    Vu, // VU interleaved (NV21, NV61)
}

/// Component order for packed formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackedOrder {
    Yuyv, // YUY2
    Uyvy, // UYVY
    Yvyu, // YVYU
    Vyuy, // VYUY
}

/// RGB format variants (component order and alpha presence)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RgbFormat {
    Rgb,  // 3 components, RGB order
    Rgba, // 4 components, RGBA order
    Bgr,  // 3 components, BGR order
    Bgra, // 4 components, BGRA order
}

/// Validated YUV→RGB conversion params with all parameters needed for dispatch
#[derive(Debug, Clone, Copy)]
enum YuvToRgbParams {
    SemiPlanar {
        subsampling: YuvSubsampling,
        uv_order: UvOrder,
        rgb_format: RgbFormat,
        bit_depth: u32,
        range: YuvRange,
        matrix: YuvStandardMatrix,
    },
    Planar {
        subsampling: YuvSubsampling,
        plane_order: (usize, usize, usize),
        rgb_format: RgbFormat,
        bit_depth: u32,
        range: YuvRange,
        matrix: YuvStandardMatrix,
    },
    PlanarAlpha {
        subsampling: YuvSubsampling,
        plane_order: (usize, usize, usize),
        rgb_format: RgbFormat,
        bit_depth: u32,
        range: YuvRange,
        matrix: YuvStandardMatrix,
        // Note: YUVA always outputs RGBA (the yuv crate only has *_alpha_to_rgba)
    },
    Packed {
        packed_order: PackedOrder,
        range: YuvRange,
        matrix: YuvStandardMatrix,
        // Note: packed YUV always outputs RGB only, not RGBA/BGR/BGRA
    },
    Grayscale {
        rgb_format: RgbFormat,
        range: YuvRange,
        matrix: YuvStandardMatrix,
    },
}

/// Validated RGB→YUV conversion params with all parameters needed for dispatch
/// Note: No Packed variant - RGB→packed YUV is NOT supported by yuv crate
#[derive(Debug, Clone, Copy)]
enum RgbToYuvParams {
    SemiPlanar {
        rgb_format: RgbFormat,
        subsampling: YuvSubsampling,
        uv_order: UvOrder,
        range: YuvRange,
        matrix: YuvStandardMatrix,
    },
    Planar {
        rgb_format: RgbFormat,
        subsampling: YuvSubsampling,
        plane_order: (usize, usize, usize),
        range: YuvRange,
        matrix: YuvStandardMatrix,
    },
    Grayscale {
        rgb_format: RgbFormat,
        range: YuvRange,
        matrix: YuvStandardMatrix,
    },
}

/// Validated RGB→RGB conversion params
#[derive(Debug, Clone, Copy)]
struct RgbToRgbParams {
    src_format: RgbFormat,
    dst_format: RgbFormat,
}

/// All supported conversion types - stored in YuvConverter at construction time
#[derive(Debug, Clone, Copy)]
enum ConversionParams {
    YuvToRgb(YuvToRgbParams),
    RgbToYuv(RgbToYuvParams),
    RgbToRgb(RgbToRgbParams),
}

/// Video format converter with API similar to gst_video::VideoConverter
///
/// Provides format conversion between video frames using either:
/// - yuv crate for fast YUV→RGB conversions
/// - gst_video::VideoConverter as fallback for complex conversions
///
/// API matches gst_video::VideoConverter - caller must allocate output buffer.
pub struct VideoConverter {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    /// Use yuv crate for fast YUV conversions
    Yuv(YuvConverter),
    /// Use GStreamer's VideoConverter for complex conversions
    Gst(gst_video::VideoConverter),
}

#[derive(Debug)]
struct YuvConverter {
    params: ConversionParams,
}

impl VideoConverter {
    /// Create a new video converter
    ///
    /// # Arguments
    /// * `in_info` - Input video format info
    /// * `out_info` - Output video format info
    ///
    /// # Returns
    /// Result with VideoConverter or error if conversion not supported
    pub fn new(
        in_info: &gst_video::VideoInfo,
        out_info: &gst_video::VideoInfo,
        config: Option<gst_video::VideoConverterConfig>,
    ) -> Result<Self, glib::BoolError> {
        if !in_info.is_valid() || !out_info.is_valid() {
            return Err(glib::bool_error!("Invalid video info"));
        }

        Ok(Self {
            inner: Self::create_inner(in_info, out_info, config)?,
        })
    }

    /// Convert a video frame (API matches gst_video::VideoConverter::frame_ref)
    ///
    /// # Arguments
    /// * `src` - Source frame (readable frame reference)
    /// * `dest` - Destination frame (must be pre-allocated with correct size)
    ///
    /// # Returns
    /// Result indicating success or error message
    pub fn frame_ref(
        &self,
        src: &gst_video::VideoFrameRef<&gst::BufferRef>,
        dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<(), String> {
        let start_time = std::time::Instant::now();

        let res = match &self.inner {
            Inner::Gst(converter) => {
                converter.frame_ref(src, dest);
                Ok(())
            }
            Inner::Yuv(yuv_conv) => yuv_conv.convert(src, dest),
        };
        gst::log!(
            CAT,
            "Conversion in {:?} took {:?}",
            self.inner,
            start_time.elapsed()
        );

        res
    }

    fn create_inner(
        in_info: &gst_video::VideoInfo,
        out_info: &gst_video::VideoInfo,
        config: Option<gst_video::VideoConverterConfig>,
    ) -> Result<Inner, glib::BoolError> {
        if let Some(converter) = YuvConverter::try_new(in_info, out_info) {
            gst::debug!(
                CAT,
                "Using yuv crate for conversion from {:?} to {:?}",
                in_info.format(),
                out_info.format()
            );
            Ok(Inner::Yuv(converter))
        } else {
            if config.clone().map_or(false, |config| {
                config.get::<bool>("disable-fallback").unwrap_or(false)
            }) {
                return Err(glib::bool_error!(
                    "Conversion not supported by yuv crate and fallback disabled"
                ));
            }
            // Fall back to GStreamer's converter
            let converter = gst_video::VideoConverter::new(in_info, out_info, config)?;
            gst::fixme!(
                CAT,
                "Falling back to GStreamer's VideoConverter for conversion from {:?} to {:?}",
                in_info.format(),
                out_info.format()
            );
            Ok(Inner::Gst(converter))
        }
    }
}

// ============================================================================
// Format Introspection Helper Functions
// ============================================================================

/// Detect if RGB format uses BGR byte order by checking component pixel offsets
fn is_bgr_format(info: &gst_video::VideoInfo) -> bool {
    let format_info = info.format_info();
    let poffset = format_info.poffset();

    // In RGB formats: component 0=Red, 1=Green, 2=Blue
    // poffset tells byte offset within pixel
    // RGB: poffset=[0,1,2] - red first
    // BGR: poffset=[2,1,0] - blue first
    // Check if blue (component 2) comes before red (component 0)
    let red_offset = poffset.get(0).copied().unwrap_or(0);
    let blue_offset = poffset.get(2).copied().unwrap_or(2);

    blue_offset < red_offset
}

/// Check if RGB format is supported by yuv crate (validates byte order)
///
/// The yuv crate only supports specific RGB byte orderings:
/// - RGB: R at offset 0, G at 1, B at 2
/// - RGBA: R at offset 0, G at 1, B at 2, A at 3 (alpha LAST)
/// - BGR: B at offset 0, G at 1, R at 2
/// - BGRA: B at offset 0, G at 1, R at 2, A at 3 (alpha LAST)
///
/// Formats NOT supported:
/// - ARGB (A at 0, R at 1, G at 2, B at 3) - alpha first
/// - ABGR (A at 0, B at 1, G at 2, R at 3) - alpha first
/// - xRGB, BGRx, etc. - other orderings
fn is_rgb_format_supported_by_yuv(info: &gst_video::VideoInfo) -> bool {
    let format_info = info.format_info();
    let poffset = format_info.poffset();
    let n_components = format_info.n_components();

    // Component indices: 0=Red, 1=Green, 2=Blue, 3=Alpha (if present)
    let red_offset = poffset.get(0).copied().unwrap_or(0);
    let green_offset = poffset.get(1).copied().unwrap_or(1);
    let blue_offset = poffset.get(2).copied().unwrap_or(2);

    let supported = if n_components == 3 {
        // RGB or BGR: must be contiguous starting at offset 0
        // RGB: R=0, G=1, B=2
        // BGR: B=0, G=1, R=2
        let is_rgb_order = red_offset == 0 && green_offset == 1 && blue_offset == 2;
        let is_bgr_order = blue_offset == 0 && green_offset == 1 && red_offset == 2;
        is_rgb_order || is_bgr_order
    } else if n_components == 4 {
        let alpha_offset = poffset.get(3).copied().unwrap_or(3);

        // RGBA or BGRA: RGB/BGR at 0-2, alpha MUST be at offset 3 (last)
        // RGBA: R=0, G=1, B=2, A=3
        // BGRA: B=0, G=1, R=2, A=3
        let is_rgba_order =
            red_offset == 0 && green_offset == 1 && blue_offset == 2 && alpha_offset == 3;
        let is_bgra_order =
            blue_offset == 0 && green_offset == 1 && red_offset == 2 && alpha_offset == 3;

        is_rgba_order || is_bgra_order
    } else {
        // Unsupported component count
        false
    };

    gst::debug!(
        CAT,
        "is_rgb_format_supported_by_yuv({:?}): n_comp={}, offsets=[R={}, G={}, B={}, A={}] → {}",
        info.format(),
        n_components,
        red_offset,
        green_offset,
        blue_offset,
        if n_components == 4 {
            poffset.get(3).copied().unwrap_or(255)
        } else {
            255
        },
        supported
    );

    supported
}

/// Introspect plane order from VideoInfo (automatically detects YV12 vs I420)
fn get_plane_order_introspected(info: &gst_video::VideoInfo) -> (usize, usize, usize) {
    let format_info = info.format_info();
    let plane = format_info.plane();

    // Component 0 = Y (luma), 1 = U/Cb (chroma), 2 = V/Cr (chroma)
    // plane[i] tells us which plane component i is in
    let y_plane = plane.get(0).copied().unwrap_or(0) as usize;
    let u_plane = plane.get(1).copied().unwrap_or(1) as usize;
    let v_plane = plane.get(2).copied().unwrap_or(2) as usize;

    (y_plane, u_plane, v_plane)
}

/// Detect RGB format variant (RGB/RGBA/BGR/BGRA) from VideoInfo
fn detect_rgb_format(info: &gst_video::VideoInfo) -> RgbFormat {
    let format_info = info.format_info();
    let n_components = format_info.n_components();
    let has_alpha = n_components == 4;
    let is_bgr = is_bgr_format(info);

    match (is_bgr, has_alpha) {
        (false, false) => RgbFormat::Rgb,
        (false, true) => RgbFormat::Rgba,
        (true, false) => RgbFormat::Bgr,
        (true, true) => RgbFormat::Bgra,
    }
}

// =============================================================================
// Characteristic-based function mapping
// =============================================================================

/// Type alias for semi-planar YUV→RGB conversion functions
type SemiPlanarConversionFn = fn(
    bi_planar_image: &yuv::YuvBiPlanarImage<u8>,
    rgb: &mut [u8],
    rgb_stride: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    mode: YuvConversionMode,
) -> Result<(), yuv::YuvError>;

/// Type alias for planar YUV→RGB conversion functions
type PlanarConversionFn = fn(
    planar_image: &yuv::YuvPlanarImage<u8>,
    rgb: &mut [u8],
    rgb_stride: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
) -> Result<(), yuv::YuvError>;

/// Type alias for packed YUV→RGB conversion functions
type PackedConversionFn = fn(
    packed_image: &yuv::YuvPackedImage<u8>,
    rgb: &mut [u8],
    rgb_stride: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
) -> Result<(), yuv::YuvError>;

// 10-bit conversion function types (use u16 slices for 10-bit data)

/// Type alias for semi-planar 10-bit YUV→RGB conversion functions
type SemiPlanarConversionFn10bit = fn(
    bi_planar_image: &yuv::YuvBiPlanarImage<u16>,
    rgb: &mut [u8],
    rgb_stride: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    mode: YuvConversionMode,
) -> Result<(), yuv::YuvError>;

/// Type alias for planar 10-bit YUV→RGB conversion functions
type PlanarConversionFn10bit = fn(
    planar_image: &yuv::YuvPlanarImage<u16>,
    rgb: &mut [u8],
    rgb_stride: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
) -> Result<(), yuv::YuvError>;

/// Detect YUV layout from VideoInfo using introspection
fn detect_yuv_layout(info: &gst_video::VideoInfo) -> YuvLayout {
    // Grayscale: 1 plane
    if info.is_gray() {
        return YuvLayout::Grayscale;
    }

    let n_planes = info.n_planes();

    match n_planes {
        4 => YuvLayout::PlanarAlpha, // Y, U, V, A separate (A420, A422, A444)
        3 => YuvLayout::Planar,      // Y, U, V separate (I420, YV12, Y42B, Y444)
        2 => YuvLayout::SemiPlanar,  // Y, UV interleaved (NV12, NV21, NV16, NV61, NV24)
        1 => YuvLayout::Packed,      // YUV interleaved (YUY2, UYVY, VYUY, YVYU)
        _ => unreachable!("Invalid number of planes: {}", n_planes),
    }
}

/// Detect YUV subsampling from VideoInfo using component dimensions
fn detect_yuv_subsampling(info: &gst_video::VideoInfo) -> YuvSubsampling {
    // For grayscale, return special marker
    if info.is_gray() {
        return YuvSubsampling::S400;
    }

    // Get chroma dimensions relative to luma
    // Component 0 = Y (luma), Component 1 = U/Cb (chroma)
    let luma_width = info.comp_width(0);
    let chroma_width = info.comp_width(1);
    let luma_height = info.comp_height(0);
    let chroma_height = info.comp_height(1);

    // Determine if there's horizontal subsampling (chroma_width ≈ luma_width / 2)
    // For odd luma dimensions, chroma = ceil(luma/2), so chroma * 2 >= luma
    // We check if chroma is roughly half of luma by seeing if 2 * chroma >= luma
    let h_subsampled =
        chroma_width > 0 && chroma_width * 2 >= luma_width && chroma_width < luma_width;
    let v_subsampled =
        chroma_height > 0 && chroma_height * 2 >= luma_height && chroma_height < luma_height;

    match (h_subsampled, v_subsampled) {
        (false, false) => YuvSubsampling::S444, // 4:4:4 - no subsampling
        (true, false) => YuvSubsampling::S422,  // 4:2:2 - horizontal subsampling only
        (true, true) => YuvSubsampling::S420,   // 4:2:0 - both directions
        (false, true) => unreachable!(
            "Unsupported subsampling (vertical only): {}x{} / {}x{}",
            luma_width, luma_height, chroma_width, chroma_height
        ),
    }
}

/// Detect UV component order for semi-planar formats using poffset
fn detect_uv_order(info: &gst_video::VideoInfo) -> UvOrder {
    let format_info = info.format_info();
    let poffset = format_info.poffset();

    // For semi-planar formats, components 1 and 2 (U and V) are in the same plane
    // poffset tells us the byte offset of each component
    // NV12/NV16/NV24: U at offset 0, V at offset 1 → UV order
    // NV21/NV61: V at offset 0, U at offset 1 → VU order

    let u_offset = poffset.get(1).copied().unwrap_or(0);
    let v_offset = poffset.get(2).copied().unwrap_or(1);

    if u_offset < v_offset {
        UvOrder::Uv
    } else {
        UvOrder::Vu
    }
}

/// Detect component order for packed formats
///
/// Note: poffset cannot distinguish VYUY from UYVY (both report Y=1, U=0, V=2)
/// so we match on format directly. With only 4 packed formats, direct matching is acceptable.
fn detect_packed_order(info: &gst_video::VideoInfo) -> PackedOrder {
    use gst_video::VideoFormat::*;

    match info.format() {
        Yuy2 => PackedOrder::Yuyv,
        Uyvy => PackedOrder::Uyvy,
        Yvyu => PackedOrder::Yvyu,
        Vyuy => PackedOrder::Vyuy,
        _ => {
            gst::warning!(
                CAT,
                "Unknown packed format: {:?}, defaulting to YUYV",
                info.format()
            );
            PackedOrder::Yuyv
        }
    }
}

// =============================================================================
// Params Detection Functions - Single source of truth for what's supported
// =============================================================================

/// Extract YUV range and matrix from colorimetry info.
/// Returns None if the matrix is unsupported.
fn extract_yuv_colorimetry(
    colorimetry: gst_video::VideoColorimetry,
) -> Option<(YuvRange, YuvStandardMatrix)> {
    let range = gst_to_yuv_range(colorimetry.range());
    let matrix = gst_to_yuv_matrix(colorimetry.matrix()).ok()?;
    Some((range, matrix))
}

/// Determine if YUV→RGB conversion is supported and return all parameters needed for dispatch.
fn yuv_to_rgb_params(
    in_info: &gst_video::VideoInfo,
    out_info: &gst_video::VideoInfo,
) -> Option<YuvToRgbParams> {
    // Must be YUV/Gray input and RGB output
    if !(in_info.is_yuv() || in_info.is_gray()) || !out_info.is_rgb() {
        return None;
    }

    let in_bit_depth = in_info.comp_depth(0);
    let out_bit_depth = out_info.comp_depth(0);

    // Output must be 8-bit RGB
    if out_bit_depth != 8 {
        return None;
    }

    // Validate RGB format is supported by yuv crate
    if !is_rgb_format_supported_by_yuv(out_info) {
        return None;
    }

    // Extract colorimetry from input (YUV side)
    let (range, matrix) = extract_yuv_colorimetry(in_info.colorimetry())?;

    let rgb_format = detect_rgb_format(out_info);
    let layout = detect_yuv_layout(in_info);

    match layout {
        YuvLayout::SemiPlanar => {
            let subsampling = detect_yuv_subsampling(in_info);
            let uv_order = detect_uv_order(in_info);

            // Check specific supported combinations
            match in_bit_depth {
                8 => matches!(
                    (subsampling, uv_order),
                    (YuvSubsampling::S420, UvOrder::Uv | UvOrder::Vu)
                        | (YuvSubsampling::S422, UvOrder::Uv | UvOrder::Vu)
                        | (YuvSubsampling::S444, UvOrder::Uv) // NO VU for S444!
                ),
                10 => {
                    // 10-bit semi-planar: S420/S422/S444 with UV order only
                    matches!(
                        (subsampling, uv_order),
                        (YuvSubsampling::S420, UvOrder::Uv)
                            | (YuvSubsampling::S422, UvOrder::Uv)
                            | (YuvSubsampling::S444, UvOrder::Uv)
                    )
                }
                _ => false,
            }
            .then(|| YuvToRgbParams::SemiPlanar {
                subsampling,
                uv_order,
                rgb_format,
                bit_depth: in_bit_depth,
                range,
                matrix,
            })
        }

        YuvLayout::Planar => {
            let subsampling = detect_yuv_subsampling(in_info);
            let plane_order = get_plane_order_introspected(in_info);

            match in_bit_depth {
                8 => matches!(
                    subsampling,
                    YuvSubsampling::S420 | YuvSubsampling::S422 | YuvSubsampling::S444
                ),
                10 => {
                    // 10-bit planar: S420/S422 support all RGB formats,
                    // S444 only supports RGBA output (yuv crate only has i410_to_rgba)
                    match subsampling {
                        YuvSubsampling::S420 | YuvSubsampling::S422 => true,
                        YuvSubsampling::S444 => rgb_format == RgbFormat::Rgba,
                        _ => false,
                    }
                }
                _ => false,
            }
            .then(|| YuvToRgbParams::Planar {
                subsampling,
                plane_order,
                rgb_format,
                bit_depth: in_bit_depth,
                range,
                matrix,
            })
        }

        YuvLayout::PlanarAlpha => {
            let subsampling = detect_yuv_subsampling(in_info);
            let plane_order = get_plane_order_introspected(in_info);

            // The yuv crate only provides 8-bit YUVA → RGBA conversions.
            (in_bit_depth == 8
                && rgb_format == RgbFormat::Rgba
                && matches!(
                    subsampling,
                    YuvSubsampling::S420 | YuvSubsampling::S422 | YuvSubsampling::S444
                ))
            .then(|| YuvToRgbParams::PlanarAlpha {
                subsampling,
                plane_order,
                rgb_format,
                bit_depth: in_bit_depth,
                range,
                matrix,
            })
        }

        YuvLayout::Packed => {
            // Packed YUV ONLY outputs RGB (not RGBA/BGR/BGRA)
            // The yuv crate functions are: yuyv422_to_rgb, uyvy422_to_rgb, etc.
            if in_bit_depth == 8 && rgb_format == RgbFormat::Rgb {
                Some(YuvToRgbParams::Packed {
                    packed_order: detect_packed_order(in_info),
                    range,
                    matrix,
                })
            } else {
                None
            }
        }

        YuvLayout::Grayscale => {
            // Grayscale only 8-bit supported
            if in_bit_depth == 8 {
                Some(YuvToRgbParams::Grayscale {
                    rgb_format,
                    range,
                    matrix,
                })
            } else {
                None
            }
        }
    }
}

/// Determine if RGB→YUV conversion is supported and return all parameters needed for dispatch.
fn rgb_to_yuv_params(
    in_info: &gst_video::VideoInfo,
    out_info: &gst_video::VideoInfo,
) -> Option<RgbToYuvParams> {
    // Must be RGB input and YUV/Gray output
    if !in_info.is_rgb() || !(out_info.is_yuv() || out_info.is_gray()) {
        return None;
    }

    let in_bit_depth = in_info.comp_depth(0);
    let out_bit_depth = out_info.comp_depth(0);

    // Only 8-bit RGB to 8-bit YUV supported
    if in_bit_depth != 8 || out_bit_depth != 8 {
        return None;
    }

    // Validate RGB format is supported by yuv crate
    if !is_rgb_format_supported_by_yuv(in_info) {
        return None;
    }

    // Extract colorimetry from output (YUV side)
    let (range, matrix) = extract_yuv_colorimetry(out_info.colorimetry())?;

    let rgb_format = detect_rgb_format(in_info);
    let layout = detect_yuv_layout(out_info);

    match layout {
        YuvLayout::Packed => {
            // RGB to packed YUV is NOT SUPPORTED by yuv crate
            None
        }

        YuvLayout::PlanarAlpha => {
            // RGB → YUVA encoding isn't wired here; fall back to the stock
            // converter for those (rare) outputs.
            None
        }

        YuvLayout::SemiPlanar => {
            let subsampling = detect_yuv_subsampling(out_info);
            let uv_order = detect_uv_order(out_info);

            // S444 only supports UV order (no NV42/VU variant functions in yuv crate)
            matches!(
                (subsampling, uv_order),
                (YuvSubsampling::S420, UvOrder::Uv | UvOrder::Vu)
                    | (YuvSubsampling::S422, UvOrder::Uv | UvOrder::Vu)
                    | (YuvSubsampling::S444, UvOrder::Uv) // NO VU!
            )
            .then(|| RgbToYuvParams::SemiPlanar {
                rgb_format,
                subsampling,
                uv_order,
                range,
                matrix,
            })
        }

        YuvLayout::Planar => {
            let subsampling = detect_yuv_subsampling(out_info);
            let plane_order = get_plane_order_introspected(out_info);

            // All planar subsampling types supported
            matches!(
                subsampling,
                YuvSubsampling::S420 | YuvSubsampling::S422 | YuvSubsampling::S444
            )
            .then(|| RgbToYuvParams::Planar {
                rgb_format,
                subsampling,
                plane_order,
                range,
                matrix,
            })
        }

        YuvLayout::Grayscale => Some(RgbToYuvParams::Grayscale {
            rgb_format,
            range,
            matrix,
        }),
    }
}

/// Determine if RGB→RGB conversion is supported and return all parameters needed for dispatch.
fn rgb_to_rgb_params(
    in_info: &gst_video::VideoInfo,
    out_info: &gst_video::VideoInfo,
) -> Option<RgbToRgbParams> {
    if !in_info.is_rgb() || !out_info.is_rgb() {
        return None;
    }

    let in_bit_depth = in_info.comp_depth(0);
    let out_bit_depth = out_info.comp_depth(0);

    // Only 8-bit RGB conversions supported
    if in_bit_depth != 8 || out_bit_depth != 8 {
        return None;
    }

    // Validate both RGB formats are supported by yuv crate
    if !is_rgb_format_supported_by_yuv(in_info) || !is_rgb_format_supported_by_yuv(out_info) {
        return None;
    }

    let src_format = detect_rgb_format(in_info);
    let dst_format = detect_rgb_format(out_info);

    // Same format is a no-op (should be handled by passthrough mode)
    if src_format == dst_format {
        return None;
    }

    Some(RgbToRgbParams {
        src_format,
        dst_format,
    })
}

// =============================================================================
// Generic conversion handlers
// =============================================================================

/// Generic handler for semi-planar YUV→RGB conversions (NV12, NV21, NV16, NV61, NV24, P010)
fn convert_semiplanar_yuv_to_rgb<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    subsampling: YuvSubsampling,
    uv_order: UvOrder,
    rgb_format: RgbFormat,
    bit_depth: u32,
) -> Result<(), String> {
    let rgb_stride = dest.plane_stride()[0] as u32;
    let rgb_data = dest
        .plane_data_mut(0)
        .map_err(|_| "Failed to get RGB plane")?;

    if bit_depth == 8 {
        // 8-bit: use u8 slices directly
        let y_plane = frame.plane_data(0).map_err(|_| "Failed to get Y plane")?;
        let uv_plane = frame.plane_data(1).map_err(|_| "Failed to get UV plane")?;

        let y_stride = frame.plane_stride()[0] as u32;
        let uv_stride = frame.plane_stride()[1] as u32;

        let bi_planar_image = yuv::YuvBiPlanarImage {
            y_plane,
            y_stride,
            uv_plane,
            uv_stride,
            width,
            height,
        };

        // Select conversion function based on subsampling, UV order, and RGB format
        let conv_fn: SemiPlanarConversionFn = match (subsampling, uv_order, rgb_format) {
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Rgb) => yuv::yuv_nv12_to_rgb,
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Rgba) => yuv::yuv_nv12_to_rgba,
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Bgr) => yuv::yuv_nv12_to_bgr,
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Bgra) => yuv::yuv_nv12_to_bgra,
            (YuvSubsampling::S420, UvOrder::Vu, RgbFormat::Rgb) => yuv::yuv_nv21_to_rgb,
            (YuvSubsampling::S420, UvOrder::Vu, RgbFormat::Rgba) => yuv::yuv_nv21_to_rgba,
            (YuvSubsampling::S420, UvOrder::Vu, RgbFormat::Bgr) => yuv::yuv_nv21_to_bgr,
            (YuvSubsampling::S420, UvOrder::Vu, RgbFormat::Bgra) => yuv::yuv_nv21_to_bgra,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Rgb) => yuv::yuv_nv16_to_rgb,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Rgba) => yuv::yuv_nv16_to_rgba,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Bgr) => yuv::yuv_nv16_to_bgr,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Bgra) => yuv::yuv_nv16_to_bgra,
            (YuvSubsampling::S422, UvOrder::Vu, RgbFormat::Rgb) => yuv::yuv_nv61_to_rgb,
            (YuvSubsampling::S422, UvOrder::Vu, RgbFormat::Rgba) => yuv::yuv_nv61_to_rgba,
            (YuvSubsampling::S422, UvOrder::Vu, RgbFormat::Bgr) => yuv::yuv_nv61_to_bgr,
            (YuvSubsampling::S422, UvOrder::Vu, RgbFormat::Bgra) => yuv::yuv_nv61_to_bgra,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Rgb) => yuv::yuv_nv24_to_rgb,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Rgba) => yuv::yuv_nv24_to_rgba,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Bgr) => yuv::yuv_nv24_to_bgr,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Bgra) => yuv::yuv_nv24_to_bgra,
            _ => {
                return Err(format!(
                    "Unsupported 8-bit semi-planar format: {:?} {:?} {:?}",
                    subsampling, uv_order, rgb_format
                ))
            }
        };

        conv_fn(
            &bi_planar_image,
            rgb_data,
            rgb_stride,
            range,
            matrix,
            YuvConversionMode::Balanced,
        )
        .map_err(|e| format!("Semi-planar 8-bit to RGB conversion failed: {:?}", e))
    } else if bit_depth == 10 {
        // 10-bit: cast u8 slices to u16 slices
        let y_plane = frame.plane_data(0).map_err(|_| "Failed to get Y plane")?;
        let uv_plane = frame.plane_data(1).map_err(|_| "Failed to get UV plane")?;

        // Strides are in bytes, but yuv crate expects stride in u16 elements
        let y_stride = (frame.plane_stride()[0] as u32) / 2;
        let uv_stride = (frame.plane_stride()[1] as u32) / 2;

        // P010 uses 16-bit samples, cast to u16 slices
        let y_plane_u16 = unsafe {
            std::slice::from_raw_parts(y_plane.as_ptr() as *const u16, y_plane.len() / 2)
        };
        let uv_plane_u16 = unsafe {
            std::slice::from_raw_parts(uv_plane.as_ptr() as *const u16, uv_plane.len() / 2)
        };

        let bi_planar_image = yuv::YuvBiPlanarImage {
            y_plane: y_plane_u16,
            y_stride,
            uv_plane: uv_plane_u16,
            uv_stride,
            width,
            height,
        };

        // Select conversion function based on subsampling, UV order, and RGB format
        let conv_fn: SemiPlanarConversionFn10bit = match (subsampling, uv_order, rgb_format) {
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Rgb) => yuv::p010_to_rgb,
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Rgba) => yuv::p010_to_rgba,
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Bgr) => yuv::p010_to_bgr,
            (YuvSubsampling::S420, UvOrder::Uv, RgbFormat::Bgra) => yuv::p010_to_bgra,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Rgb) => yuv::p210_to_rgb,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Rgba) => yuv::p210_to_rgba,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Bgr) => yuv::p210_to_bgr,
            (YuvSubsampling::S422, UvOrder::Uv, RgbFormat::Bgra) => yuv::p210_to_bgra,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Rgb) => yuv::p410_to_rgb,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Rgba) => yuv::p410_to_rgba,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Bgr) => yuv::p410_to_bgr,
            (YuvSubsampling::S444, UvOrder::Uv, RgbFormat::Bgra) => yuv::p410_to_bgra,
            _ => {
                return Err(format!(
                    "Unsupported 10-bit semi-planar format: {:?} {:?} {:?}",
                    subsampling, uv_order, rgb_format
                ))
            }
        };

        conv_fn(
            &bi_planar_image,
            rgb_data,
            rgb_stride,
            range,
            matrix,
            YuvConversionMode::Balanced,
        )
        .map_err(|e| format!("Semi-planar 10-bit to RGB conversion failed: {:?}", e))
    } else {
        Err(format!("Unsupported bit depth: {}", bit_depth))
    }
}

/// Generic handler for planar YUV→RGB conversions (I420, YV12, Y42B, Y444, I420_10LE, I422_10LE)
fn convert_planar_yuv_to_rgb<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    plane_order: (usize, usize, usize),
    subsampling: YuvSubsampling,
    rgb_format: RgbFormat,
    bit_depth: u32,
) -> Result<(), String> {
    let (y_plane_idx, u_plane_idx, v_plane_idx) = plane_order;

    let rgb_stride = dest.plane_stride()[0] as u32;
    let rgb_data = dest
        .plane_data_mut(0)
        .map_err(|_| "Failed to get RGB plane")?;

    if bit_depth == 8 {
        // 8-bit: use u8 slices directly
        let y_plane = frame
            .plane_data(y_plane_idx as u32)
            .map_err(|_| "Failed to get Y plane")?;
        let u_plane = frame
            .plane_data(u_plane_idx as u32)
            .map_err(|_| "Failed to get U plane")?;
        let v_plane = frame
            .plane_data(v_plane_idx as u32)
            .map_err(|_| "Failed to get V plane")?;

        let y_stride = frame.plane_stride()[y_plane_idx] as u32;
        let u_stride = frame.plane_stride()[u_plane_idx] as u32;
        let v_stride = frame.plane_stride()[v_plane_idx] as u32;

        let planar_image = yuv::YuvPlanarImage {
            y_plane,
            y_stride,
            u_plane,
            u_stride,
            v_plane,
            v_stride,
            width,
            height,
        };

        // Select conversion function based on subsampling and output RGB format
        let conv_fn: PlanarConversionFn = match (subsampling, rgb_format) {
            (YuvSubsampling::S420, RgbFormat::Rgb) => yuv::yuv420_to_rgb,
            (YuvSubsampling::S420, RgbFormat::Rgba) => yuv::yuv420_to_rgba,
            (YuvSubsampling::S420, RgbFormat::Bgr) => yuv::yuv420_to_bgr,
            (YuvSubsampling::S420, RgbFormat::Bgra) => yuv::yuv420_to_bgra,
            (YuvSubsampling::S422, RgbFormat::Rgb) => yuv::yuv422_to_rgb,
            (YuvSubsampling::S422, RgbFormat::Rgba) => yuv::yuv422_to_rgba,
            (YuvSubsampling::S422, RgbFormat::Bgr) => yuv::yuv422_to_bgr,
            (YuvSubsampling::S422, RgbFormat::Bgra) => yuv::yuv422_to_bgra,
            (YuvSubsampling::S444, RgbFormat::Rgb) => yuv::yuv444_to_rgb,
            (YuvSubsampling::S444, RgbFormat::Rgba) => yuv::yuv444_to_rgba,
            (YuvSubsampling::S444, RgbFormat::Bgr) => yuv::yuv444_to_bgr,
            (YuvSubsampling::S444, RgbFormat::Bgra) => yuv::yuv444_to_bgra,
            _ => {
                return Err(format!(
                    "Unsupported 8-bit planar format: {:?} {:?}",
                    subsampling, rgb_format
                ))
            }
        };

        conv_fn(&planar_image, rgb_data, rgb_stride, range, matrix)
            .map_err(|e| format!("Planar 8-bit to RGB conversion failed: {:?}", e))
    } else if bit_depth == 10 {
        // 10-bit: cast u8 slices to u16 slices
        let y_plane = frame
            .plane_data(y_plane_idx as u32)
            .map_err(|_| "Failed to get Y plane")?;
        let u_plane = frame
            .plane_data(u_plane_idx as u32)
            .map_err(|_| "Failed to get U plane")?;
        let v_plane = frame
            .plane_data(v_plane_idx as u32)
            .map_err(|_| "Failed to get V plane")?;

        // Strides in bytes, convert to u16 elements
        let y_stride = (frame.plane_stride()[y_plane_idx] as u32) / 2;
        let u_stride = (frame.plane_stride()[u_plane_idx] as u32) / 2;
        let v_stride = (frame.plane_stride()[v_plane_idx] as u32) / 2;

        // Cast to u16 slices
        let y_plane_u16 = unsafe {
            std::slice::from_raw_parts(y_plane.as_ptr() as *const u16, y_plane.len() / 2)
        };
        let u_plane_u16 = unsafe {
            std::slice::from_raw_parts(u_plane.as_ptr() as *const u16, u_plane.len() / 2)
        };
        let v_plane_u16 = unsafe {
            std::slice::from_raw_parts(v_plane.as_ptr() as *const u16, v_plane.len() / 2)
        };

        let planar_image = yuv::YuvPlanarImage {
            y_plane: y_plane_u16,
            y_stride,
            u_plane: u_plane_u16,
            u_stride,
            v_plane: v_plane_u16,
            v_stride,
            width,
            height,
        };

        // Select conversion function based on subsampling and output RGB format
        let conv_fn: PlanarConversionFn10bit = match (subsampling, rgb_format) {
            (YuvSubsampling::S420, RgbFormat::Rgb) => yuv::i010_to_rgb,
            (YuvSubsampling::S420, RgbFormat::Rgba) => yuv::i010_to_rgba,
            (YuvSubsampling::S420, RgbFormat::Bgr) => yuv::i010_to_bgr,
            (YuvSubsampling::S420, RgbFormat::Bgra) => yuv::i010_to_bgra,
            (YuvSubsampling::S422, RgbFormat::Rgb) => yuv::i210_to_rgb,
            (YuvSubsampling::S422, RgbFormat::Rgba) => yuv::i210_to_rgba,
            (YuvSubsampling::S422, RgbFormat::Bgr) => yuv::i210_to_bgr,
            (YuvSubsampling::S422, RgbFormat::Bgra) => yuv::i210_to_bgra,
            (YuvSubsampling::S444, RgbFormat::Rgba) => yuv::i410_to_rgba,
            _ => {
                return Err(format!(
                    "Unsupported 10-bit planar format: {:?} {:?}",
                    subsampling, rgb_format
                ))
            }
        };

        conv_fn(&planar_image, rgb_data, rgb_stride, range, matrix)
            .map_err(|e| format!("Planar 10-bit to RGB conversion failed: {:?}", e))
    } else {
        Err(format!("Unsupported bit depth: {}", bit_depth))
    }
}

/// Handler for planar YUV-with-alpha → RGBA conversions (A420, A422, A444).
/// The alpha plane is always the 4th plane; output is RGBA with straight
/// (non-premultiplied) alpha to match the rest of the pipeline.
#[allow(clippy::too_many_arguments)]
fn convert_planar_yuva_to_rgba<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    plane_order: (usize, usize, usize),
    subsampling: YuvSubsampling,
    rgb_format: RgbFormat,
    bit_depth: u32,
) -> Result<(), String> {
    // The yuv crate only provides 8-bit YUVA → RGBA.
    if bit_depth != 8 || rgb_format != RgbFormat::Rgba {
        return Err(format!(
            "Unsupported planar-alpha conversion: {}-bit {:?}",
            bit_depth, rgb_format
        ));
    }

    let (y_plane_idx, u_plane_idx, v_plane_idx) = plane_order;
    let a_plane_idx = 3usize;

    let rgba_stride = dest.plane_stride()[0] as u32;
    let rgba_data = dest
        .plane_data_mut(0)
        .map_err(|_| "Failed to get RGBA plane")?;

    let y_plane = frame
        .plane_data(y_plane_idx as u32)
        .map_err(|_| "Failed to get Y plane")?;
    let u_plane = frame
        .plane_data(u_plane_idx as u32)
        .map_err(|_| "Failed to get U plane")?;
    let v_plane = frame
        .plane_data(v_plane_idx as u32)
        .map_err(|_| "Failed to get V plane")?;
    let a_plane = frame
        .plane_data(a_plane_idx as u32)
        .map_err(|_| "Failed to get A plane")?;

    let y_stride = frame.plane_stride()[y_plane_idx] as u32;
    let u_stride = frame.plane_stride()[u_plane_idx] as u32;
    let v_stride = frame.plane_stride()[v_plane_idx] as u32;
    let a_stride = frame.plane_stride()[a_plane_idx] as u32;

    let image = yuv::YuvPlanarImageWithAlpha {
        y_plane,
        y_stride,
        u_plane,
        u_stride,
        v_plane,
        v_stride,
        a_plane,
        a_stride,
        width,
        height,
    };

    let result = match subsampling {
        YuvSubsampling::S420 => {
            yuv::yuv420_alpha_to_rgba(&image, rgba_data, rgba_stride, range, matrix, false)
        }
        YuvSubsampling::S422 => {
            yuv::yuv422_alpha_to_rgba(&image, rgba_data, rgba_stride, range, matrix, false)
        }
        YuvSubsampling::S444 => {
            yuv::yuv444_alpha_to_rgba(&image, rgba_data, rgba_stride, range, matrix, false)
        }
        _ => {
            return Err(format!(
                "Unsupported planar-alpha subsampling: {:?}",
                subsampling
            ))
        }
    };

    result.map_err(|e| format!("Planar YUVA to RGBA conversion failed: {:?}", e))
}

/// Generic handler for packed YUV→RGB conversions (YUY2, UYVY, VYUY, YVYU)
fn convert_packed_yuv_to_rgb<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    packed_order: PackedOrder,
) -> Result<(), String> {
    // Note: packed YUV is always 8-bit
    let yuy_plane = frame.plane_data(0).map_err(|_| "Failed to get YUY plane")?;
    let yuy_stride = frame.plane_stride()[0] as u32;

    let packed_image = yuv::YuvPackedImage {
        yuy: yuy_plane,
        yuy_stride,
        width,
        height,
    };

    let rgb_stride = dest.plane_stride()[0] as u32;
    let rgb_data = dest
        .plane_data_mut(0)
        .map_err(|_| "Failed to get RGB plane")?;

    // Select conversion function based on packed order
    let conv_fn: PackedConversionFn = match packed_order {
        PackedOrder::Yuyv => yuv::yuyv422_to_rgb,
        PackedOrder::Uyvy => yuv::uyvy422_to_rgb,
        PackedOrder::Yvyu => yuv::yvyu422_to_rgb,
        PackedOrder::Vyuy => yuv::vyuy422_to_rgb,
    };

    conv_fn(&packed_image, rgb_data, rgb_stride, range, matrix)
        .map_err(|e| format!("Packed to RGB conversion failed: {:?}", e))
}

/// Generic handler for grayscale→RGB conversions (GRAY8)
fn convert_grayscale_to_rgb<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    rgb_format: RgbFormat,
) -> Result<(), String> {
    // Note: grayscale is always 8-bit
    let y_plane = frame.plane_data(0).map_err(|_| "Failed to get Y plane")?;
    let y_stride = frame.plane_stride()[0] as u32;

    let gray_image = yuv::YuvGrayImage {
        y_plane,
        y_stride,
        width,
        height,
    };

    let rgb_stride = dest.plane_stride()[0] as u32;
    let rgb_data = dest
        .plane_data_mut(0)
        .map_err(|_| "Failed to get RGB plane")?;

    match rgb_format {
        RgbFormat::Rgb => yuv::yuv400_to_rgb(&gray_image, rgb_data, rgb_stride, range, matrix),
        RgbFormat::Rgba => yuv::yuv400_to_rgba(&gray_image, rgb_data, rgb_stride, range, matrix),
        RgbFormat::Bgr => yuv::yuv400_to_bgr(&gray_image, rgb_data, rgb_stride, range, matrix),
        RgbFormat::Bgra => yuv::yuv400_to_bgra(&gray_image, rgb_data, rgb_stride, range, matrix),
    }
    .map_err(|e| format!("Grayscale to RGB conversion failed: {:?}", e))
}

// =============================================================================
// RGB → YUV conversion handlers
// =============================================================================

/// Generic handler for RGB→semi-planar YUV conversions
fn convert_rgb_to_semiplanar_yuv<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    rgb_format: RgbFormat,
    subsampling: YuvSubsampling,
    uv_order: UvOrder,
) -> Result<(), String> {
    // Get strides first (immutable borrows)
    let rgb_stride = frame.plane_stride()[0] as u32;
    let y_stride = dest.plane_stride()[0] as u32;
    let uv_stride = dest.plane_stride()[1] as u32;

    // Get RGB input plane
    let rgb_plane = frame.plane_data(0).map_err(|_| "Failed to get RGB plane")?;

    // Get YUV output planes as mutable slices
    // We need to use unsafe here because we need multiple mutable borrows to non-overlapping planes
    // The planes are guaranteed to be non-overlapping in memory by GStreamer
    let y_size = (y_stride * height) as usize;
    let uv_height = match subsampling {
        YuvSubsampling::S420 => height / 2,
        YuvSubsampling::S422 => height,
        YuvSubsampling::S444 => height,
        YuvSubsampling::S400 => {
            unreachable!("S400 (grayscale) should use convert_rgb_to_grayscale")
        }
    };
    let uv_size = (uv_stride * uv_height) as usize;

    let y_ptr = {
        let y_data = dest
            .plane_data_mut(0)
            .map_err(|_| "Failed to get Y plane")?;
        y_data.as_mut_ptr()
    };

    let uv_ptr = {
        let uv_data = dest
            .plane_data_mut(1)
            .map_err(|_| "Failed to get UV plane")?;
        uv_data.as_mut_ptr()
    };

    let (y_plane, uv_plane) = unsafe {
        (
            std::slice::from_raw_parts_mut(y_ptr, y_size),
            std::slice::from_raw_parts_mut(uv_ptr, uv_size),
        )
    };

    // Construct YuvBiPlanarImageMut from GStreamer buffer planes
    let mut bi_planar_image = yuv::YuvBiPlanarImageMut {
        y_plane: yuv::BufferStoreMut::Borrowed(y_plane),
        y_stride,
        uv_plane: yuv::BufferStoreMut::Borrowed(uv_plane),
        uv_stride,
        width,
        height,
    };

    // Select appropriate conversion function based on RGB format, subsampling, and UV order
    match (rgb_format, subsampling, uv_order) {
        // RGB → NV12/NV21/NV16/NV61/NV24
        (RgbFormat::Rgb, YuvSubsampling::S420, UvOrder::Uv) => yuv::rgb_to_yuv_nv12(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgb, YuvSubsampling::S420, UvOrder::Vu) => yuv::rgb_to_yuv_nv21(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgb, YuvSubsampling::S422, UvOrder::Uv) => yuv::rgb_to_yuv_nv16(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgb, YuvSubsampling::S422, UvOrder::Vu) => yuv::rgb_to_yuv_nv61(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgb, YuvSubsampling::S444, UvOrder::Uv) => yuv::rgb_to_yuv_nv24(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        // RGBA → NV12/NV21/NV16/NV61/NV24
        (RgbFormat::Rgba, YuvSubsampling::S420, UvOrder::Uv) => yuv::rgba_to_yuv_nv12(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgba, YuvSubsampling::S420, UvOrder::Vu) => yuv::rgba_to_yuv_nv21(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgba, YuvSubsampling::S422, UvOrder::Uv) => yuv::rgba_to_yuv_nv16(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgba, YuvSubsampling::S422, UvOrder::Vu) => yuv::rgba_to_yuv_nv61(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgba, YuvSubsampling::S444, UvOrder::Uv) => yuv::rgba_to_yuv_nv24(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        // BGR → NV12/NV21/NV16/NV61/NV24
        (RgbFormat::Bgr, YuvSubsampling::S420, UvOrder::Uv) => yuv::bgr_to_yuv_nv12(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgr, YuvSubsampling::S420, UvOrder::Vu) => yuv::bgr_to_yuv_nv21(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgr, YuvSubsampling::S422, UvOrder::Uv) => yuv::bgr_to_yuv_nv16(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgr, YuvSubsampling::S422, UvOrder::Vu) => yuv::bgr_to_yuv_nv61(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgr, YuvSubsampling::S444, UvOrder::Uv) => yuv::bgr_to_yuv_nv24(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        // BGRA → NV12/NV21/NV16/NV61/NV24
        (RgbFormat::Bgra, YuvSubsampling::S420, UvOrder::Uv) => yuv::bgra_to_yuv_nv12(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgra, YuvSubsampling::S420, UvOrder::Vu) => yuv::bgra_to_yuv_nv21(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgra, YuvSubsampling::S422, UvOrder::Uv) => yuv::bgra_to_yuv_nv16(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgra, YuvSubsampling::S422, UvOrder::Vu) => yuv::bgra_to_yuv_nv61(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgra, YuvSubsampling::S444, UvOrder::Uv) => yuv::bgra_to_yuv_nv24(
            &mut bi_planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        _ => {
            return Err(format!(
                "Unsupported RGB→semi-planar conversion: {:?} → {:?} {:?}",
                rgb_format, subsampling, uv_order
            ))
        }
    }
    .map_err(|e| format!("RGB→semi-planar YUV conversion failed: {:?}", e))
}

/// Generic handler for RGB→planar YUV conversions
fn convert_rgb_to_planar_yuv<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    rgb_format: RgbFormat,
    subsampling: YuvSubsampling,
    plane_order: (usize, usize, usize),
) -> Result<(), String> {
    let (y_plane_idx, u_plane_idx, v_plane_idx) = plane_order;

    // Get strides first (immutable borrows)
    let rgb_stride = frame.plane_stride()[0] as u32;
    let y_stride = dest.plane_stride()[y_plane_idx] as u32;
    let u_stride = dest.plane_stride()[u_plane_idx] as u32;
    let v_stride = dest.plane_stride()[v_plane_idx] as u32;

    // Get RGB input plane
    let rgb_plane = frame.plane_data(0).map_err(|_| "Failed to get RGB plane")?;

    // Get YUV output planes as mutable slices
    // We need to use unsafe here because we need multiple mutable borrows to non-overlapping planes
    // The planes are guaranteed to be non-overlapping in memory by GStreamer
    let (y_size, u_size, v_size) = match subsampling {
        YuvSubsampling::S420 => {
            let chroma_height = height / 2;
            (
                (y_stride * height) as usize,
                (u_stride * chroma_height) as usize,
                (v_stride * chroma_height) as usize,
            )
        }
        YuvSubsampling::S422 => (
            (y_stride * height) as usize,
            (u_stride * height) as usize,
            (v_stride * height) as usize,
        ),
        YuvSubsampling::S444 => (
            (y_stride * height) as usize,
            (u_stride * height) as usize,
            (v_stride * height) as usize,
        ),
        YuvSubsampling::S400 => {
            unreachable!("S400 (grayscale) should use convert_rgb_to_grayscale")
        }
    };

    let y_ptr = {
        let y_data = dest
            .plane_data_mut(y_plane_idx as u32)
            .map_err(|_| "Failed to get Y plane")?;
        y_data.as_mut_ptr()
    };

    let u_ptr = {
        let u_data = dest
            .plane_data_mut(u_plane_idx as u32)
            .map_err(|_| "Failed to get U plane")?;
        u_data.as_mut_ptr()
    };

    let v_ptr = {
        let v_data = dest
            .plane_data_mut(v_plane_idx as u32)
            .map_err(|_| "Failed to get V plane")?;
        v_data.as_mut_ptr()
    };

    let (y_plane, u_plane, v_plane) = unsafe {
        (
            std::slice::from_raw_parts_mut(y_ptr, y_size),
            std::slice::from_raw_parts_mut(u_ptr, u_size),
            std::slice::from_raw_parts_mut(v_ptr, v_size),
        )
    };

    // Construct YuvPlanarImageMut from GStreamer buffer planes
    let mut planar_image = yuv::YuvPlanarImageMut {
        y_plane: yuv::BufferStoreMut::Borrowed(y_plane),
        y_stride,
        u_plane: yuv::BufferStoreMut::Borrowed(u_plane),
        u_stride,
        v_plane: yuv::BufferStoreMut::Borrowed(v_plane),
        v_stride,
        width,
        height,
    };

    // Select appropriate conversion function based on RGB format and subsampling
    match (rgb_format, subsampling) {
        // RGB → I420/YV12/Y42B/Y444
        (RgbFormat::Rgb, YuvSubsampling::S420) => yuv::rgb_to_yuv420(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgb, YuvSubsampling::S422) => yuv::rgb_to_yuv422(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgb, YuvSubsampling::S444) => yuv::rgb_to_yuv444(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        // RGBA → I420/YV12/Y42B/Y444
        (RgbFormat::Rgba, YuvSubsampling::S420) => yuv::rgba_to_yuv420(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgba, YuvSubsampling::S422) => yuv::rgba_to_yuv422(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Rgba, YuvSubsampling::S444) => yuv::rgba_to_yuv444(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        // BGR → I420/YV12/Y42B/Y444
        (RgbFormat::Bgr, YuvSubsampling::S420) => yuv::bgr_to_yuv420(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgr, YuvSubsampling::S422) => yuv::bgr_to_yuv422(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgr, YuvSubsampling::S444) => yuv::bgr_to_yuv444(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        // BGRA → I420/YV12/Y42B/Y444
        (RgbFormat::Bgra, YuvSubsampling::S420) => yuv::bgra_to_yuv420(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgra, YuvSubsampling::S422) => yuv::bgra_to_yuv422(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),
        (RgbFormat::Bgra, YuvSubsampling::S444) => yuv::bgra_to_yuv444(
            &mut planar_image,
            rgb_plane,
            rgb_stride,
            range,
            matrix,
            yuv::YuvConversionMode::Balanced,
        ),

        _ => {
            return Err(format!(
                "Unsupported RGB→planar conversion: {:?} → {:?}",
                rgb_format, subsampling
            ))
        }
    }
    .map_err(|e| format!("RGB→planar YUV conversion failed: {:?}", e))
}

/// Generic handler for RGB→grayscale conversions
fn convert_rgb_to_grayscale<T>(
    frame: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    range: YuvRange,
    matrix: YuvStandardMatrix,
    rgb_format: RgbFormat,
) -> Result<(), String> {
    // Get strides first (immutable borrows)
    let rgb_stride = frame.plane_stride()[0] as u32;
    let y_stride = dest.plane_stride()[0] as u32;

    // Get RGB input plane
    let rgb_plane = frame.plane_data(0).map_err(|_| "Failed to get RGB plane")?;

    // Get Y output plane as mutable slice
    let y_plane = dest
        .plane_data_mut(0)
        .map_err(|_| "Failed to get Y plane")?;

    // Construct YuvGrayImageMut from GStreamer buffer
    let mut gray_image = yuv::YuvGrayImageMut {
        y_plane: yuv::BufferStoreMut::Borrowed(y_plane),
        y_stride,
        width,
        height,
    };

    // Select appropriate conversion function based on RGB format
    match rgb_format {
        RgbFormat::Rgb => yuv::rgb_to_yuv400(&mut gray_image, rgb_plane, rgb_stride, range, matrix),
        RgbFormat::Rgba => {
            yuv::rgba_to_yuv400(&mut gray_image, rgb_plane, rgb_stride, range, matrix)
        }
        RgbFormat::Bgr => yuv::bgr_to_yuv400(&mut gray_image, rgb_plane, rgb_stride, range, matrix),
        RgbFormat::Bgra => {
            yuv::bgra_to_yuv400(&mut gray_image, rgb_plane, rgb_stride, range, matrix)
        }
    }
    .map_err(|e| format!("RGB→grayscale conversion failed: {:?}", e))
}

/// Convert RGB to RGB (channel shuffling, alpha addition/removal)
fn convert_rgb_to_rgb<T>(
    src: &gst_video::VideoFrameRef<T>,
    dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    width: u32,
    height: u32,
    src_format: RgbFormat,
    dst_format: RgbFormat,
) -> Result<(), String> {
    // Get strides
    let src_stride = src.plane_stride()[0] as u32;
    let dst_stride = dest.plane_stride()[0] as u32;

    // Get plane data
    let src_plane = src
        .plane_data(0)
        .map_err(|_| "Failed to get source RGB plane")?;
    let dst_plane = dest
        .plane_data_mut(0)
        .map_err(|_| "Failed to get destination RGB plane")?;

    // Select appropriate shuffle function based on format pair
    let result = match (src_format, dst_format) {
        (RgbFormat::Rgb, RgbFormat::Rgba) => {
            rgb_to_rgba(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Rgb, RgbFormat::Bgr) => {
            rgb_to_bgr(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Rgb, RgbFormat::Bgra) => {
            rgb_to_bgra(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Rgba, RgbFormat::Rgb) => {
            rgba_to_rgb(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Rgba, RgbFormat::Bgr) => {
            rgba_to_bgr(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Rgba, RgbFormat::Bgra) => {
            rgba_to_bgra(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Bgr, RgbFormat::Rgb) => {
            bgr_to_rgb(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Bgr, RgbFormat::Rgba) => {
            bgr_to_rgba(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Bgr, RgbFormat::Bgra) => {
            bgr_to_bgra(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Bgra, RgbFormat::Rgb) => {
            bgra_to_rgb(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Bgra, RgbFormat::Rgba) => {
            bgra_to_rgba(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        (RgbFormat::Bgra, RgbFormat::Bgr) => {
            bgra_to_bgr(src_plane, src_stride, dst_plane, dst_stride, width, height)
        }
        // Same format - should not happen, but handle as no-op copy
        (src_fmt, dst_fmt) if src_fmt == dst_fmt => {
            return Err(format!(
                "RGB→RGB conversion with identical formats ({:?}→{:?}) should not occur",
                src_fmt, dst_fmt
            ))
        }
        (src_fmt, dst_fmt) => {
            return Err(format!(
                "Unsupported RGB→RGB conversion: {:?}→{:?}",
                src_fmt, dst_fmt
            ))
        }
    };

    result.map_err(|e| format!("RGB→RGB shuffle failed: {:?}", e))
}

impl YuvConverter {
    fn try_new(
        in_info: &gst_video::VideoInfo,
        out_info: &gst_video::VideoInfo,
    ) -> Option<YuvConverter> {
        Some(YuvConverter {
            params: if let Some(params) = yuv_to_rgb_params(in_info, out_info) {
                gst::debug!(
                    CAT,
                    "YuvConverter::conversion_params({:?} → {:?}) = YuvToRgb({:?})",
                    in_info.format(),
                    out_info.format(),
                    params
                );
                ConversionParams::YuvToRgb(params)
            } else if let Some(params) = rgb_to_yuv_params(in_info, out_info) {
                gst::debug!(
                    CAT,
                    "YuvConverter::supports({:?} → {:?}) = RgbToYuv({:?})",
                    in_info.format(),
                    out_info.format(),
                    params
                );
                ConversionParams::RgbToYuv(params)
            } else if let Some(params) = rgb_to_rgb_params(in_info, out_info) {
                gst::debug!(
                    CAT,
                    "YuvConverter::supports({:?} → {:?}) = RgbToRgb({:?})",
                    in_info.format(),
                    out_info.format(),
                    params
                );
                ConversionParams::RgbToRgb(params)
            } else {
                return None;
            },
        })
    }

    fn convert(
        &self,
        src: &gst_video::VideoFrameRef<&gst::BufferRef>,
        dest: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<(), String> {
        // Use output dimensions (visible area) - strides from input handle padding
        let width = dest.info().width();
        let height = dest.info().height();

        // Dispatch based on the params stored at construction time
        match self.params {
            ConversionParams::YuvToRgb(params) => match params {
                YuvToRgbParams::SemiPlanar {
                    subsampling,
                    uv_order,
                    rgb_format,
                    bit_depth,
                    range,
                    matrix,
                } => convert_semiplanar_yuv_to_rgb(
                    src,
                    dest,
                    width,
                    height,
                    range,
                    matrix,
                    subsampling,
                    uv_order,
                    rgb_format,
                    bit_depth,
                ),
                YuvToRgbParams::Planar {
                    subsampling,
                    plane_order,
                    rgb_format,
                    bit_depth,
                    range,
                    matrix,
                } => convert_planar_yuv_to_rgb(
                    src,
                    dest,
                    width,
                    height,
                    range,
                    matrix,
                    plane_order,
                    subsampling,
                    rgb_format,
                    bit_depth,
                ),
                YuvToRgbParams::PlanarAlpha {
                    subsampling,
                    plane_order,
                    rgb_format,
                    bit_depth,
                    range,
                    matrix,
                } => convert_planar_yuva_to_rgba(
                    src,
                    dest,
                    width,
                    height,
                    range,
                    matrix,
                    plane_order,
                    subsampling,
                    rgb_format,
                    bit_depth,
                ),
                YuvToRgbParams::Packed {
                    packed_order,
                    range,
                    matrix,
                } => {
                    convert_packed_yuv_to_rgb(src, dest, width, height, range, matrix, packed_order)
                }
                YuvToRgbParams::Grayscale {
                    rgb_format,
                    range,
                    matrix,
                } => convert_grayscale_to_rgb(src, dest, width, height, range, matrix, rgb_format),
            },
            ConversionParams::RgbToYuv(params) => match params {
                RgbToYuvParams::SemiPlanar {
                    rgb_format,
                    subsampling,
                    uv_order,
                    range,
                    matrix,
                } => convert_rgb_to_semiplanar_yuv(
                    src,
                    dest,
                    width,
                    height,
                    range,
                    matrix,
                    rgb_format,
                    subsampling,
                    uv_order,
                ),
                RgbToYuvParams::Planar {
                    rgb_format,
                    subsampling,
                    plane_order,
                    range,
                    matrix,
                } => convert_rgb_to_planar_yuv(
                    src,
                    dest,
                    width,
                    height,
                    range,
                    matrix,
                    rgb_format,
                    subsampling,
                    plane_order,
                ),
                RgbToYuvParams::Grayscale {
                    rgb_format,
                    range,
                    matrix,
                } => convert_rgb_to_grayscale(src, dest, width, height, range, matrix, rgb_format),
            },
            ConversionParams::RgbToRgb(params) => convert_rgb_to_rgb(
                src,
                dest,
                width,
                height,
                params.src_format,
                params.dst_format,
            ),
        }
    }
}

/// Map GStreamer VideoColorRange to yuv crate YuvRange
fn gst_to_yuv_range(range: gst_video::VideoColorRange) -> YuvRange {
    match range {
        gst_video::VideoColorRange::Range0_255 => YuvRange::Full,
        gst_video::VideoColorRange::Range16_235 => YuvRange::Limited,
        _ => {
            gst::warning!(CAT, "Unknown color range, defaulting to Limited");
            YuvRange::Limited
        }
    }
}

/// Map GStreamer VideoColorMatrix to yuv crate YuvStandardMatrix
fn gst_to_yuv_matrix(matrix: gst_video::VideoColorMatrix) -> Result<YuvStandardMatrix, String> {
    match matrix {
        gst_video::VideoColorMatrix::Bt709 => Ok(YuvStandardMatrix::Bt709),
        gst_video::VideoColorMatrix::Bt601 => Ok(YuvStandardMatrix::Bt601),
        gst_video::VideoColorMatrix::Bt2020 => Ok(YuvStandardMatrix::Bt2020),
        gst_video::VideoColorMatrix::Smpte240m => Ok(YuvStandardMatrix::Smpte240),
        gst_video::VideoColorMatrix::Fcc => Ok(YuvStandardMatrix::Fcc),
        _ => Err(format!("Unsupported color matrix: {:?}", matrix)),
    }
}
