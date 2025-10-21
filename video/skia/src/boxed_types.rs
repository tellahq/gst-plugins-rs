// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use skia;
use std::ops::Deref;

#[derive(Clone, Debug, glib::Boxed)]
#[boxed_type(name = "GstRsSkiaImage")]
pub struct SkiaImage(skia::Image);

impl SkiaImage {
    pub fn new(image: skia::Image) -> Self {
        Self(image)
    }

    pub fn into_inner(self) -> skia::Image {
        self.0
    }
}

impl Deref for SkiaImage {
    type Target = skia::Image;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<skia::Image> for SkiaImage {
    fn from(image: skia::Image) -> Self {
        Self(image)
    }
}

impl From<SkiaImage> for skia::Image {
    fn from(boxed: SkiaImage) -> Self {
        boxed.0
    }
}

#[derive(Clone, Debug, glib::Boxed)]
#[boxed_type(name = "GstRsSkiaCanvas")]
pub struct SkiaCanvas(*const skia::Canvas);

impl SkiaCanvas {
    pub fn new(canvas: &skia::Canvas) -> Self {
        Self(canvas as *const skia::Canvas)
    }

    /// # Safety
    /// The caller must ensure that the canvas pointer is still valid
    pub unsafe fn as_ref(&self) -> &skia::Canvas {
        &*self.0
    }
}

// SAFETY: The canvas is only accessed from the signal handler which is called
// synchronously from the same thread that owns the canvas
unsafe impl Send for SkiaCanvas {}
unsafe impl Sync for SkiaCanvas {}

#[derive(Clone, Debug, glib::Boxed)]
#[boxed_type(name = "GstRsSkiaContext")]
pub struct SkiaContext(Option<*mut skia::gpu::DirectContext>);

impl SkiaContext {
    pub fn new(context: Option<&mut skia::gpu::DirectContext>) -> Self {
        Self(context.map(|ctx| ctx as *mut skia::gpu::DirectContext))
    }

    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }

    pub fn is_none(&self) -> bool {
        self.0.is_none()
    }

    /// # Safety
    /// The caller must ensure that the context pointer is still valid
    /// and that there are no other mutable references to the context
    pub unsafe fn as_mut(&self) -> Option<&mut skia::gpu::DirectContext> {
        self.0.map(|ptr| &mut *ptr)
    }
}

// SAFETY: The context is only accessed from the signal handler which is called
// synchronously from the same thread that owns the context
unsafe impl Send for SkiaContext {}
unsafe impl Sync for SkiaContext {}

#[derive(Clone, Debug, glib::Boxed)]
#[boxed_type(name = "GstRsSkiaBufferRef")]
pub struct BufferRef(*const gst::BufferRef);

impl BufferRef {
    pub fn new(buffer: &gst::BufferRef) -> Self {
        Self(buffer as *const gst::BufferRef)
    }

    /// # Safety
    /// The caller must ensure that the buffer pointer is still valid
    pub unsafe fn as_ref(&self) -> &gst::BufferRef {
        &*self.0
    }
}

// SAFETY: The buffer is only accessed from the signal handler which is called
// synchronously from the same thread that owns the buffer
unsafe impl Send for BufferRef {}
unsafe impl Sync for BufferRef {}
