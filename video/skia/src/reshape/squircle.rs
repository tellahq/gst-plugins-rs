use skia::Path;
use std::f32::consts::PI;

/// Parameters for superellipse path generation
#[derive(Debug, Clone)]
pub struct SuperellipseParams {
    pub width: f32,
    pub height: f32,
    pub corner_radius: f32,
    pub curvature: f32, // CSS K value: 0=bevel, 1=round, 2=squircle, inf=square
}

/// Generate a superellipse corner path segment
/// CSS formula: x^(2K) + y^(2K) = 1, so actual exponent n = 2*K
fn superellipse_corner_points(radius: f32, curvature: f32, num_points: usize) -> Vec<(f32, f32)> {
    let n = 2.0 * curvature; // CSS uses 2K as the exponent
    let exp = 2.0 / n;

    (0..=num_points)
        .map(|i| {
            let t = (i as f32 / num_points as f32) * (PI / 2.0);
            let cos_t = t.cos();
            let sin_t = t.sin();

            let x = radius * cos_t.abs().powf(exp) * cos_t.signum();
            let y = radius * sin_t.abs().powf(exp) * sin_t.signum();

            (x, y)
        })
        .collect()
}

/// Generate a Skia path for a rectangle with superellipse corners
pub fn get_superellipse_path(params: SuperellipseParams) -> Path {
    let SuperellipseParams {
        width,
        height,
        corner_radius,
        curvature,
    } = params;

    // Clamp corner radius to half the smaller dimension
    let max_radius = width.min(height) / 2.0;
    let radius = corner_radius.min(max_radius);

    // If no radius, return simple rectangle
    if radius <= 0.0 {
        let mut path = Path::new();
        path.add_rect(skia::Rect::from_wh(width, height), None);
        return path;
    }

    let mut path = Path::new();

    // Number of points per corner (128 gives smooth curve)
    let points_per_corner = 128;
    let corner_points = superellipse_corner_points(radius, curvature, points_per_corner);

    // Start at top edge, after top-left corner
    path.move_to((radius, 0.0));

    // Top edge to top-right corner
    path.line_to((width - radius, 0.0));

    // Top-right corner (rotate points 270 degrees, translate to corner)
    for (x, y) in &corner_points {
        path.line_to((width - radius + *y, radius - *x));
    }

    // Right edge to bottom-right corner
    path.line_to((width, height - radius));

    // Bottom-right corner
    for (x, y) in &corner_points {
        path.line_to((width - radius + *x, height - radius + *y));
    }

    // Bottom edge to bottom-left corner
    path.line_to((radius, height));

    // Bottom-left corner (rotate 90 degrees)
    for (x, y) in &corner_points {
        path.line_to((radius - *y, height - radius + *x));
    }

    // Left edge to top-left corner
    path.line_to((0.0, radius));

    // Top-left corner
    for (x, y) in &corner_points {
        path.line_to((radius - *x, radius - *y));
    }

    path.close();
    path
}

/// Generate superellipse path with configurable curvature (CSS K value)
/// curvature = 2.0 matches CSS superellipse(2) = squircle
pub fn get_skia_path(width: f32, height: f32, corner_radius: f32, curvature: f32) -> Path {
    get_superellipse_path(SuperellipseParams {
        width,
        height,
        corner_radius,
        curvature,
    })
}
