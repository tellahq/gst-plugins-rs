use once_cell::sync::Lazy;
use skia::{path::ArcSize, Path, PathDirection, Vector};
use std::{cmp::Ordering, collections::HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Top,
    Left,
    Right,
    Bottom,
}

#[derive(Debug, Clone, Copy)]
pub struct Adjacent {
    side: Side,
    corner: Corner,
}

#[derive(Debug, Clone, Copy)]
pub struct RoundedRectangle {
    top_left_corner_radius: f32,
    top_right_corner_radius: f32,
    bottom_right_corner_radius: f32,
    bottom_left_corner_radius: f32,
    width: f32,
    height: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct NormalizedCorner {
    radius: f32,
    rounding_and_smoothing_budget: f32,
}

pub type NormalizedCorners = HashMap<Corner, NormalizedCorner>;

static ADJACENTS_BY_CORNER: Lazy<HashMap<Corner, Vec<Adjacent>>> = Lazy::new(|| {
    HashMap::from([
        (
            Corner::TopLeft,
            vec![
                Adjacent {
                    corner: Corner::TopRight,
                    side: Side::Top,
                },
                Adjacent {
                    corner: Corner::BottomLeft,
                    side: Side::Left,
                },
            ],
        ),
        (
            Corner::TopRight,
            vec![
                Adjacent {
                    corner: Corner::TopLeft,
                    side: Side::Top,
                },
                Adjacent {
                    corner: Corner::BottomRight,
                    side: Side::Right,
                },
            ],
        ),
        (
            Corner::BottomLeft,
            vec![
                Adjacent {
                    corner: Corner::BottomRight,
                    side: Side::Bottom,
                },
                Adjacent {
                    corner: Corner::TopLeft,
                    side: Side::Left,
                },
            ],
        ),
        (
            Corner::BottomRight,
            vec![
                Adjacent {
                    corner: Corner::BottomLeft,
                    side: Side::Bottom,
                },
                Adjacent {
                    corner: Corner::TopRight,
                    side: Side::Right,
                },
            ],
        ),
    ])
});

pub fn distribute_and_normalize(rect: RoundedRectangle) -> NormalizedCorners {
    let mut budget_map = HashMap::new();
    let mut radius_map = HashMap::new();

    for &corner in &[
        Corner::TopLeft,
        Corner::TopRight,
        Corner::BottomLeft,
        Corner::BottomRight,
    ] {
        budget_map.insert(corner, -1.0);
    }

    radius_map.insert(Corner::TopLeft, rect.top_left_corner_radius);
    radius_map.insert(Corner::TopRight, rect.top_right_corner_radius);
    radius_map.insert(Corner::BottomLeft, rect.bottom_left_corner_radius);
    radius_map.insert(Corner::BottomRight, rect.bottom_right_corner_radius);

    let mut corners: Vec<_> = radius_map.iter().collect();
    corners.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(Ordering::Equal));

    let mut new_values = Vec::new();

    for &(corner, &radius) in &corners {
        let adjacents = &ADJACENTS_BY_CORNER[corner];
        let budget = adjacents
            .iter()
            .map(|adjacent| {
                let adj_radius = radius_map[&adjacent.corner];
                if radius == 0.0 && adj_radius == 0.0 {
                    0.0
                } else {
                    let adj_budget = budget_map[&adjacent.corner];
                    let side_length = if adjacent.side == Side::Top || adjacent.side == Side::Bottom
                    {
                        rect.width
                    } else {
                        rect.height
                    };
                    if adj_budget >= 0.0 {
                        side_length - adj_budget
                    } else {
                        (radius / (radius + adj_radius)) * side_length
                    }
                }
            })
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
            .unwrap();

        budget_map.insert(*corner, budget);
        new_values.push((*corner, radius.min(budget)));
    }

    for (corner, radius) in new_values {
        radius_map.insert(corner, radius);
    }

    let mut result = HashMap::new();
    for &corner in &[
        Corner::TopLeft,
        Corner::TopRight,
        Corner::BottomLeft,
        Corner::BottomRight,
    ] {
        result.insert(
            corner,
            NormalizedCorner {
                radius: radius_map[&corner],
                rounding_and_smoothing_budget: budget_map[&corner],
            },
        );
    }
    result
}

#[derive(Debug, Clone)]
struct CornerPathParams {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    p: f32,
    corner_radius: f32,
    arc_section_length: f32,
}

#[derive(Debug, Clone)]
struct CornerParams {
    corner_radius: f32,
    corner_smoothing: f32,
    preserve_smoothing: bool,
    rounding_and_smoothing_budget: f32,
}

fn get_path_params_for_corner(params: CornerParams) -> CornerPathParams {
    let mut p = (1.0 + params.corner_smoothing) * params.corner_radius;
    let mut corner_smoothing = params.corner_smoothing;

    if !params.preserve_smoothing {
        let max_corner_smoothing =
            params.rounding_and_smoothing_budget / params.corner_radius - 1.0;
        corner_smoothing = corner_smoothing.min(max_corner_smoothing);
        p = p.min(params.rounding_and_smoothing_budget);
    }

    let arc_measure = 90.0 * (1.0 - corner_smoothing);
    let arc_section_length =
        (arc_measure / 2.0).to_radians().sin() * params.corner_radius * (2.0f32).sqrt();
    let angle_alpha = (90.0 - arc_measure) / 2.0;
    let p3_to_p4_distance = params.corner_radius * (angle_alpha / 2.0).to_radians().tan();

    let angle_beta = 45.0 * corner_smoothing;
    let c = p3_to_p4_distance * angle_beta.to_radians().cos();
    let d = c * angle_beta.to_radians().tan();

    let mut b = (p - arc_section_length - c - d) / 3.0;
    let mut a = 2.0 * b;
    if params.preserve_smoothing && p > params.rounding_and_smoothing_budget {
        let p1_to_p3_max_distance =
            params.rounding_and_smoothing_budget - d - arc_section_length - c;
        let min_a = p1_to_p3_max_distance / 6.0;
        let max_b = p1_to_p3_max_distance - min_a;

        b = b.min(max_b);
        a = p1_to_p3_max_distance - b;
        p = p.min(params.rounding_and_smoothing_budget);
    }

    CornerPathParams {
        a,
        b,
        c,
        d,
        p,
        arc_section_length,
        corner_radius: params.corner_radius,
    }
}

#[derive(Debug, Clone)]
pub struct SkiaPathInput {
    width: f32,
    height: f32,
    top_right_path_params: CornerPathParams,
    bottom_right_path_params: CornerPathParams,
    bottom_left_path_params: CornerPathParams,
    top_left_path_params: CornerPathParams,
}

#[derive(Debug, Clone)]
pub struct SquircleParams {
    pub corner_radius: Option<f32>,
    pub top_left_corner_radius: Option<f32>,
    pub top_right_corner_radius: Option<f32>,
    pub bottom_right_corner_radius: Option<f32>,
    pub bottom_left_corner_radius: Option<f32>,
    pub corner_smoothing: f32,
    pub width: f32,
    pub height: f32,
    pub preserve_smoothing: Option<bool>,
}

pub fn get_skia_path(params: SquircleParams) -> Path {
    let top_left_corner_radius = params
        .top_left_corner_radius
        .unwrap_or(params.corner_radius.unwrap_or(0.0));
    let top_right_corner_radius = params
        .top_right_corner_radius
        .unwrap_or(params.corner_radius.unwrap_or(0.0));
    let bottom_left_corner_radius = params
        .bottom_left_corner_radius
        .unwrap_or(params.corner_radius.unwrap_or(0.0));
    let bottom_right_corner_radius = params
        .bottom_right_corner_radius
        .unwrap_or(params.corner_radius.unwrap_or(0.0));
    if top_left_corner_radius == top_right_corner_radius
        && top_right_corner_radius == bottom_right_corner_radius
        && bottom_right_corner_radius == bottom_left_corner_radius
        && bottom_left_corner_radius == top_left_corner_radius
    {
        let rounding_and_smoothing_budget = params.width.min(params.height) / 2.0;
        let corner_radius = top_left_corner_radius.min(rounding_and_smoothing_budget);

        let path_params = get_path_params_for_corner(CornerParams {
            corner_radius,
            corner_smoothing: params.corner_smoothing,
            preserve_smoothing: params.preserve_smoothing.unwrap_or(false),
            rounding_and_smoothing_budget,
        });
        return get_skia_path_from_path_params(SkiaPathInput {
            width: params.width,
            height: params.height,
            top_left_path_params: path_params.clone(),
            top_right_path_params: path_params.clone(),
            bottom_left_path_params: path_params.clone(),
            bottom_right_path_params: path_params,
        });
    }

    let normalized_corners = distribute_and_normalize(RoundedRectangle {
        top_left_corner_radius,
        top_right_corner_radius,
        bottom_right_corner_radius,
        bottom_left_corner_radius,
        width: params.width,
        height: params.height,
    });
    let top_left = normalized_corners.get(&Corner::TopLeft).unwrap();
    let top_right = normalized_corners.get(&Corner::TopRight).unwrap();
    let bottom_left = normalized_corners.get(&Corner::BottomLeft).unwrap();
    let bottom_right = normalized_corners.get(&Corner::BottomRight).unwrap();

    get_skia_path_from_path_params(SkiaPathInput {
        width: params.width,
        height: params.height,
        top_left_path_params: get_path_params_for_corner(CornerParams {
            corner_smoothing: params.corner_smoothing,
            preserve_smoothing: params.preserve_smoothing.unwrap_or(false),
            corner_radius: top_left.radius,
            rounding_and_smoothing_budget: top_left.rounding_and_smoothing_budget,
        }),
        top_right_path_params: get_path_params_for_corner(CornerParams {
            corner_smoothing: params.corner_smoothing,
            preserve_smoothing: params.preserve_smoothing.unwrap_or(false),
            corner_radius: top_right.radius,
            rounding_and_smoothing_budget: top_right.rounding_and_smoothing_budget,
        }),
        bottom_right_path_params: get_path_params_for_corner(CornerParams {
            corner_smoothing: params.corner_smoothing,
            preserve_smoothing: params.preserve_smoothing.unwrap_or(false),
            corner_radius: bottom_right.radius,
            rounding_and_smoothing_budget: bottom_right.rounding_and_smoothing_budget,
        }),
        bottom_left_path_params: get_path_params_for_corner(CornerParams {
            corner_smoothing: params.corner_smoothing,
            preserve_smoothing: params.preserve_smoothing.unwrap_or(false),
            corner_radius: bottom_left.radius,
            rounding_and_smoothing_budget: bottom_left.rounding_and_smoothing_budget,
        }),
    })
}

fn get_skia_path_from_path_params(input: SkiaPathInput) -> Path {
    let mut path = Path::new();
    path.move_to((input.width - input.top_right_path_params.p, 0.0));
    draw_top_right_path(&mut path, &input.top_right_path_params);
    path.line_to((
        input.width,
        input.height - input.bottom_right_path_params.p,
    ));
    draw_bottom_right_path(&mut path, &input.bottom_right_path_params);
    path.line_to((input.bottom_left_path_params.p, input.height));
    draw_bottom_left_path(&mut path, &input.bottom_left_path_params);
    path.line_to((0.0, input.top_left_path_params.p));
    draw_top_left_path(&mut path, &input.top_left_path_params);
    path.close();
    path
}

fn draw_top_right_path(path: &mut Path, params: &CornerPathParams) {
    if params.corner_radius > 0.0 {
        path.r_cubic_to(
            Vector::new(params.a, 0.0),
            Vector::new(params.a + params.b, 0.0),
            Vector::new(params.a + params.b + params.c, params.d),
        );
        path.r_arc_to_rotated(
            (params.corner_radius, params.corner_radius),
            0.0,
            ArcSize::Small,
            PathDirection::CW,
            (params.arc_section_length, params.arc_section_length),
        );
        path.r_cubic_to(
            Vector::new(params.d, params.c),
            Vector::new(params.d, params.b + params.c),
            Vector::new(params.d, params.a + params.b + params.c),
        );
    } else {
        path.r_line_to(Vector::new(params.p, 0.0));
    }
}

fn draw_bottom_right_path(path: &mut Path, params: &CornerPathParams) {
    if params.corner_radius > 0.0 {
        path.r_cubic_to(
            Vector::new(0.0, params.a),
            Vector::new(0.0, params.a + params.b),
            Vector::new(-params.d, params.a + params.b + params.c),
        );
        path.r_arc_to_rotated(
            (params.corner_radius, params.corner_radius),
            0.0,
            ArcSize::Small,
            PathDirection::CW,
            (-params.arc_section_length, params.arc_section_length),
        );

        path.r_cubic_to(
            Vector::new(-params.c, params.d),
            Vector::new(-(params.b + params.c), params.d),
            Vector::new(-(params.a + params.b + params.c), params.d),
        );
    } else {
        path.r_line_to(Vector::new(0.0, params.p));
    }
}

fn draw_bottom_left_path(path: &mut Path, params: &CornerPathParams) {
    if params.corner_radius > 0.0 {
        path.r_cubic_to(
            Vector::new(-params.a, 0.0),
            Vector::new(-(params.a + params.b), 0.0),
            Vector::new(-(params.a + params.b + params.c), -params.d),
        );
        path.r_arc_to_rotated(
            (params.corner_radius, params.corner_radius),
            0.0,
            ArcSize::Small,
            PathDirection::CW,
            (-params.arc_section_length, -params.arc_section_length),
        );

        path.r_cubic_to(
            Vector::new(-params.d, -params.c),
            Vector::new(-params.d, -(params.b + params.c)),
            Vector::new(-params.d, -(params.a + params.b + params.c)),
        );
    } else {
        path.r_line_to(Vector::new(-params.p, 0.0));
    }
}

fn draw_top_left_path(path: &mut Path, params: &CornerPathParams) {
    if params.corner_radius > 0.0 {
        path.r_cubic_to(
            Vector::new(0.0, -params.a),
            Vector::new(0.0, -(params.a + params.b)),
            Vector::new(params.d, -(params.a + params.b + params.c)),
        );
        path.r_arc_to_rotated(
            (params.corner_radius, params.corner_radius),
            0.0,
            ArcSize::Small,
            PathDirection::CW,
            (params.arc_section_length, -params.arc_section_length),
        );

        path.r_cubic_to(
            Vector::new(params.c, -params.d),
            Vector::new(params.b + params.c, -params.d),
            Vector::new(params.a + params.b + params.c, -params.d),
        );
    } else {
        path.r_line_to(Vector::new(0.0, -params.p));
    }
}
