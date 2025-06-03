// Copyright (C) 2025, Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct SkiaReshapeGL(ObjectSubclass<imp::SkiaReshapeGL>) @extends gst_gl::GLFilter, gst_gl::GLBaseFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

impl SkiaReshapeGL {
    pub fn new(name: Option<&str>) -> Self {
        glib::Object::builder().property("name", name).build()
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "skiareshapegl",
        gst::Rank::NONE,
        SkiaReshapeGL::static_type(),
    )
}
