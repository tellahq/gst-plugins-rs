// SPDX-License-Identifier: MPL-2.0

#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use std::sync::LazyLock;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rsvideoconverter",
        gst::DebugColorFlags::empty(),
        Some("Video Format Converter Library"),
    )
});

mod videoconverter;
pub use videoconverter::VideoConverter;
