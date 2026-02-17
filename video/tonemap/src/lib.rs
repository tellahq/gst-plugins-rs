// Copyright (C) 2026, Tella
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-rstonemap:
 *
 * Since: plugins-rs-0.14.0
 */
use gst::glib;
use gst::prelude::StaticType;

mod imp;
mod math;

glib::wrapper! {
    pub struct RsTonemap(ObjectSubclass<imp::RsTonemap>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rstonemap",
        gst::Rank::NONE,
        RsTonemap::static_type(),
    )
}

gst::plugin_define!(
    rstonemap,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
