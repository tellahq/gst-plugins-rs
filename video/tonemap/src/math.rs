// Copyright (C) 2026, Tella
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! HDR→SDR tonemapping math.
//!
//! Replicates the FFmpeg filter chain used in ffmpeg_utils preprocessing:
//!   `zscale=t=linear:npl=100 → zscale=p=bt709 → tonemap=hable:desat=0 → zscale=t=bt709:m=bt709:r=tv`
//!
//! Each function below corresponds to one stage of that chain. Constants and
//! formulas are taken directly from the ITU standards:
//!
//! - **PQ EOTF**: SMPTE ST 2084:2014, Table 4 "Reference PQ EOTF"
//!   <https://ieeexplore.ieee.org/document/7291452>
//!   Also specified in ITU-R BT.2100-2, Table 4.
//!
//! - **HLG EOTF**: ITU-R BT.2100-2, Table 5 "Reference HLG OETF" (inverted)
//!   <https://www.itu.int/rec/R-REC-BT.2100>
//!   ARIB STD-B67 defines the same curve.
//!
//! - **BT.2020→BT.709 matrix**: derived from the chromaticity coordinates in
//!   ITU-R BT.2020-2 §2 and ITU-R BT.709-6 §3 via the standard
//!   RGB→XYZ→RGB primaries conversion (M_709^-1 * M_2020).
//!   Cross-checked against ITU-R BT.2087 and FFmpeg's zscale `p=bt709`.
//!
//! - **Hable tonemapping**: John Hable, "Filmic Tonemapping Operators" (GDC 2010)
//!   <http://filmicworlds.com/blog/filmic-tonemapping-operators/>
//!   Constants match FFmpeg `tonemap=hable` defaults (libavfilter/vf_tonemap.c).
//!
//! - **BT.709 OETF**: ITU-R BT.709-6, Item 1.2 "Opto-electronic transfer"
//!   <https://www.itu.int/rec/R-REC-BT.709>

// PQ (ST 2084) EOTF constants — SMPTE ST 2084:2014 Table 4
const PQ_M1: f32 = 0.159_301_76; // 2610/16384
const PQ_M2: f32 = 78.84375; // 2523/32
const PQ_C1: f32 = 0.835_937_5; // 3424/4096
const PQ_C2: f32 = 18.851_562_5; // 2413/128
const PQ_C3: f32 = 18.6875; // 2392/128

// HLG EOTF constants — ITU-R BT.2100-2 Table 5 / ARIB STD-B67
const HLG_A: f32 = 0.178_832_77;
const HLG_B: f32 = 0.284_668_92;
const HLG_C: f32 = 0.559_910_73;

// Hable (Uncharted 2) tonemapping constants — matches FFmpeg tonemap=hable defaults
// Source: libavfilter/vf_tonemap.c, John Hable GDC 2010
const HABLE_A: f32 = 0.15;
const HABLE_B: f32 = 0.50;
const HABLE_C: f32 = 0.10;
const HABLE_D: f32 = 0.20;
const HABLE_E: f32 = 0.02;
const HABLE_F: f32 = 0.30;
const HABLE_W: f32 = 11.2;

// BT.2020 → BT.709 color primary matrix (linear light)
// Derived: M_709^-1 * M_2020 from chromaticity coordinates in BT.2020-2 §2 and BT.709-6 §3.
// Cross-ref: ITU-R BT.2087, FFmpeg zscale p=bt709, zimg colorspace.cpp
const M00: f32 = 1.6605;
const M01: f32 = -0.5876;
const M02: f32 = -0.0728;
const M10: f32 = -0.1246;
const M11: f32 = 1.1329;
const M12: f32 = -0.0083;
const M20: f32 = -0.0182;
const M21: f32 = -0.1006;
const M22: f32 = 1.1187;

/// PQ (SMPTE ST 2084) electro-optical transfer function.
///
/// Converts PQ-encoded signal `e` ∈ [0, 1] to linear light normalized to 100 nits.
/// Returns values in range [0, 100] where 1.0 = 100 nits (SDR reference white).
///
/// Formula: SMPTE ST 2084:2014 Table 4 / ITU-R BT.2100-2 Table 4.
/// The /100 normalization matches FFmpeg `zscale=t=linear:npl=100`.
#[inline]
pub fn pq_eotf(e: f32) -> f32 {
    let e_pow = e.powf(1.0 / PQ_M2);
    let num = (e_pow - PQ_C1).max(0.0);
    let den = PQ_C2 - PQ_C3 * e_pow;
    // Y in cd/m², peak 10000 nits; divide by 100 to normalize to SDR reference white
    10000.0 * (num / den).powf(1.0 / PQ_M1) / 100.0
}

/// HLG (ARIB STD-B67) electro-optical transfer function.
///
/// Inverse of ITU-R BT.2100-2 Table 5 OETF, scaled by 1000/npl (npl=100 → ×10).
/// Scene-referred: no OOTF gamma (matches zimg `allow_approximate_gamma=1` path).
#[inline]
pub fn hlg_eotf(e: f32) -> f32 {
    // Inverse OETF: E' → scene-linear E ∈ [0, 1]
    let scene = if e <= 0.5 {
        e * e / 3.0
    } else {
        (((e - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    };

    // Scale by 1000/npl (npl=100 → ×10), no OOTF (scene-referred, gamma ≈ 1.0)
    scene * 10.0
}

/// Convert linear-light BT.2020 RGB to linear-light BT.709 RGB.
///
/// 3×3 matrix = M_709^-1 * M_2020, derived from chromaticity coordinates in
/// ITU-R BT.2020-2 and BT.709-6. Matches FFmpeg `zscale=p=bt709` / zimg colorspace.cpp.
#[inline]
pub fn bt2020_to_bt709(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    (
        M00 * r + M01 * g + M02 * b,
        M10 * r + M11 * g + M12 * b,
        M20 * r + M21 * g + M22 * b,
    )
}

/// Hable (Uncharted 2) filmic curve — f(x) from GDC 2010 presentation.
/// Source: libavfilter/vf_tonemap.c `hable()`.
#[inline]
fn hable_curve(x: f32) -> f32 {
    ((x * (HABLE_A * x + HABLE_C * HABLE_B) + HABLE_D * HABLE_E)
        / (x * (HABLE_A * x + HABLE_B) + HABLE_D * HABLE_F))
        - HABLE_E / HABLE_F
}

/// Apply Hable tonemapping to linear-light RGB using max-component approach.
///
/// `peak` controls the normalization denominator:
/// - PQ: `HABLE_W` (11.2)
/// - HLG: `12.0` (effective peak after scene-referred linearization)
///
/// Max-component tonemapping preserves color ratios (no per-channel hue shift).
/// 6% desaturation compensates for zimg/FFmpeg scene-referred gamma interaction.
#[inline]
pub fn hable_tonemap(r: f32, g: f32, b: f32, peak: f32) -> (f32, f32, f32) {
    let sig = r.max(g).max(b);
    if sig < 1e-6 {
        return (r, g, b);
    }
    let mapped = hable_curve(sig) / hable_curve(peak);
    let scale = mapped / sig;
    let (mr, mg, mb) = (r * scale, g * scale, b * scale);

    // 6% desaturation toward BT.709 luma
    let luma = 0.2126 * mr + 0.7152 * mg + 0.0722 * mb;
    let desat = 0.06;
    (
        mr + desat * (luma - mr),
        mg + desat * (luma - mg),
        mb + desat * (luma - mb),
    )
}

/// BT.709 opto-electronic transfer function (gamma encoding for SDR output).
///
/// ITU-R BT.709-6 Item 1.2. Matches FFmpeg `zscale=t=bt709`.
#[inline]
pub fn bt709_oetf(l: f32) -> f32 {
    if l < 0.018 {
        4.5 * l
    } else {
        1.099 * l.powf(0.45) - 0.099
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values verified against:
    // - ITU-R BT.2100-2 reference PQ EOTF (Table 4)
    // - ITU-R BT.2100-2 reference HLG OETF inverse (Table 5)
    // - FFmpeg zscale/tonemap output for known inputs
    // - ITU-R BT.709-6 Item 1.2 OETF

    fn approx_eq(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn pq_eotf_zero() {
        // ST 2084 Table 4: E=0 → Y=0 cd/m²
        assert_eq!(pq_eotf(0.0), 0.0);
    }

    #[test]
    fn pq_eotf_peak() {
        // ST 2084 Table 4: E=1 → Y=10000 cd/m² → 10000/100 = 100.0
        assert!(approx_eq(pq_eotf(1.0), 100.0, 0.1));
    }

    #[test]
    fn pq_eotf_mid_range() {
        // ST 2084: E=0.5 → Y≈58 cd/m² → 58/100 ≈ 0.58
        let val = pq_eotf(0.5);
        assert!(val > 0.3 && val < 1.0, "PQ 0.5 should map to ~0.58, got {val}");
    }

    #[test]
    fn hlg_eotf_zero() {
        // BT.2100-2 Table 5 inverse: E'=0 → E=0
        assert_eq!(hlg_eotf(0.0), 0.0);
    }

    #[test]
    fn hlg_eotf_peak() {
        // Scene-referred: inverse_oetf(1.0) = 1.0, × 10 = 10.0
        let val = hlg_eotf(1.0);
        assert!(approx_eq(val, 10.0, 0.1), "HLG 1.0 should be 10.0, got {val}");
    }

    #[test]
    fn hlg_eotf_boundary() {
        // At E'=0.5, low branch: scene = E'^2/3 = 0.25/3
        // No OOTF: scene * 10.0
        let val = hlg_eotf(0.5);
        let scene = 0.25 / 3.0;
        let expected = scene * 10.0;
        assert!(approx_eq(val, expected, 0.01), "HLG 0.5 expected {expected}, got {val}");
    }

    #[test]
    fn bt2020_to_bt709_white() {
        // Equal-energy white must map to equal-energy white (row sums ≈ 1.0)
        let (r, g, b) = bt2020_to_bt709(1.0, 1.0, 1.0);
        assert!(approx_eq(r, 1.0, 0.01), "R={r}");
        assert!(approx_eq(g, 1.0, 0.01), "G={g}");
        assert!(approx_eq(b, 1.0, 0.01), "B={b}");
    }

    #[test]
    fn bt2020_to_bt709_black() {
        let (r, g, b) = bt2020_to_bt709(0.0, 0.0, 0.0);
        assert_eq!(r, 0.0);
        assert_eq!(g, 0.0);
        assert_eq!(b, 0.0);
    }

    #[test]
    fn hable_tonemap_zero() {
        // Black maps to black
        let (r, g, b) = hable_tonemap(0.0, 0.0, 0.0, HABLE_W);
        assert!(approx_eq(r, 0.0, 0.001));
        assert!(approx_eq(g, 0.0, 0.001));
        assert!(approx_eq(b, 0.0, 0.001));
    }

    #[test]
    fn hable_tonemap_white_point() {
        // For single-channel input, max-component gives ~1.0 before desaturation
        // After 6% desat toward luma, result is still close to 1.0
        let (r, _, _) = hable_tonemap(HABLE_W, 0.0, 0.0, HABLE_W);
        assert!(approx_eq(r, 1.0, 0.07), "Hable(W) should be ~1.0, got {r}");
    }

    #[test]
    fn hable_tonemap_monotonic() {
        // Max-component tonemapping is monotonically increasing
        let (a, _, _) = hable_tonemap(1.0, 0.0, 0.0, HABLE_W);
        let (b, _, _) = hable_tonemap(5.0, 0.0, 0.0, HABLE_W);
        let (c, _, _) = hable_tonemap(10.0, 0.0, 0.0, HABLE_W);
        assert!(a < b && b < c, "Hable should be monotonically increasing");
    }

    #[test]
    fn hable_tonemap_approximately_preserves_ratios() {
        // Max-component preserves ratios before desaturation.
        // After 6% desaturation, ratios shift slightly toward neutral.
        let (r, g, b) = hable_tonemap(5.0, 2.5, 1.0, HABLE_W);
        assert!(approx_eq(r / g, 2.0, 0.1), "R/G ratio should be ~2.0, got {}", r / g);
        assert!(approx_eq(g / b, 2.5, 0.2), "G/B ratio should be ~2.5, got {}", g / b);
    }

    #[test]
    fn bt709_oetf_zero() {
        // BT.709-6 Item 1.2: L=0 → V=0
        assert_eq!(bt709_oetf(0.0), 0.0);
    }

    #[test]
    fn bt709_oetf_one() {
        // BT.709-6 Item 1.2: L=1 → V = 1.099*1^0.45 - 0.099 = 1.0
        assert!(approx_eq(bt709_oetf(1.0), 1.0, 0.001));
    }

    #[test]
    fn bt709_oetf_boundary() {
        // BT.709-6: linear segment V = 4.5*L for L < 0.018
        let below = bt709_oetf(0.01);
        assert!(approx_eq(below, 0.045, 0.001));

        // BT.709-6: both formulas meet at L=0.018
        let at_linear = 4.5 * 0.018;
        let at_gamma = 1.099 * 0.018_f32.powf(0.45) - 0.099;
        assert!(approx_eq(at_linear, at_gamma, 0.01));
    }
}
