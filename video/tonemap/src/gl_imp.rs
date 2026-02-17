// Copyright (C) 2026, Tella
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! GL-accelerated `rstonemapgl` element — HDR→SDR tonemapping via GLSL fragment shader.
//!
//! Extends `GLFilter` so it participates natively in the GL pipeline without
//! CPU download/upload. The tonemapping math is identical to `imp.rs` / `math.rs`
//! but runs entirely on the GPU as a fragment shader.

use gst::{glib, subclass::prelude::*};
use gst_base::subclass::prelude::*;
use gst_gl::{
    prelude::*,
    subclass::{prelude::*, GLFilterMode},
};
use std::sync::{LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rstonemapgl",
        gst::DebugColorFlags::empty(),
        Some("HDR to SDR tonemapping (GL)"),
    )
});

/// GLSL fragment shader that replicates the CPU tonemapping pipeline.
/// PQ EOTF → BT.2020→BT.709 → Hable tonemap → BT.709 OETF.
///
/// All constants match math.rs (sourced from SMPTE ST 2084, ITU-R BT.2100-2,
/// ITU-R BT.709-6, and FFmpeg tonemap=hable).
const FRAGMENT_SHADER: &str = r#"
#ifdef GL_ES
precision highp float;
#endif

varying vec2 v_texcoord;
uniform sampler2D tex;
uniform int active;

// PQ (ST 2084) constants
const float PQ_M1 = 0.1593017578125;
const float PQ_M2 = 78.84375;
const float PQ_C1 = 0.8359375;
const float PQ_C2 = 18.8515625;
const float PQ_C3 = 18.6875;

// Hable constants
const float HABLE_A = 0.15;
const float HABLE_B = 0.50;
const float HABLE_C = 0.10;
const float HABLE_D = 0.20;
const float HABLE_E = 0.02;
const float HABLE_F = 0.30;
const float HABLE_W = 11.2;

// BT.2020 → BT.709 matrix
const mat3 BT2020_TO_BT709 = mat3(
     1.6605, -0.1246, -0.0182,
    -0.5876,  1.1329, -0.1006,
    -0.0728, -0.0083,  1.1187
);

vec3 pq_eotf(vec3 e) {
    vec3 ep = pow(e, vec3(1.0 / PQ_M2));
    vec3 num = max(ep - PQ_C1, 0.0);
    vec3 den = PQ_C2 - PQ_C3 * ep;
    return 10000.0 * pow(num / den, vec3(1.0 / PQ_M1)) / 100.0;
}

float hable_curve(float x) {
    return ((x * (HABLE_A * x + HABLE_C * HABLE_B) + HABLE_D * HABLE_E)
          / (x * (HABLE_A * x + HABLE_B) + HABLE_D * HABLE_F))
          - HABLE_E / HABLE_F;
}

vec3 hable_tonemap(vec3 c) {
    float w = 1.0 / hable_curve(HABLE_W);
    return vec3(
        hable_curve(c.r) * w,
        hable_curve(c.g) * w,
        hable_curve(c.b) * w
    );
}

vec3 bt709_oetf(vec3 l) {
    return mix(
        4.5 * l,
        1.099 * pow(l, vec3(0.45)) - 0.099,
        step(0.018, l)
    );
}

void main() {
    vec4 rgba = texture2D(tex, v_texcoord);

    if (active == 0) {
        gl_FragColor = rgba;
        return;
    }

    vec3 linear_2020 = pq_eotf(rgba.rgb);
    vec3 linear_709 = BT2020_TO_BT709 * linear_2020;
    vec3 tonemapped = hable_tonemap(max(linear_709, 0.0));
    vec3 sdr = bt709_oetf(tonemapped);

    gl_FragColor = vec4(clamp(sdr, 0.0, 1.0), rgba.a);
}
"#;

const VERTEX_SHADER: &str = r#"
attribute vec4 a_position;
attribute vec2 a_texcoord;
varying vec2 v_texcoord;

void main() {
    gl_Position = a_position;
    v_texcoord = a_texcoord;
}
"#;

struct GlState {
    program: gl::types::GLuint,
    vao: gl::types::GLuint,
    vbo: gl::types::GLuint,
    fbo: gl::types::GLuint,
    active_loc: gl::types::GLint,
    tex_loc: gl::types::GLint,
}

// SAFETY: All GL access happens on the GstGLContext thread
unsafe impl Send for GlState {}
unsafe impl Sync for GlState {}

impl Drop for GlState {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteProgram(self.program);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteBuffers(1, &self.vbo);
            gl::DeleteFramebuffers(1, &self.fbo);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    force_active: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            force_active: false,
        }
    }
}

#[derive(Default)]
pub struct RsTonemapGL {
    settings: Mutex<Settings>,
    gl_state: Mutex<Option<GlState>>,
    active: Mutex<bool>,
}

fn detect_hdr_transfer(caps: &gst::Caps) -> bool {
    let caps_str = caps.to_string();
    caps_str.contains("smpte-st-2084") || caps_str.contains("arib-std-b67")
}

fn compile_shader(kind: gl::types::GLenum, source: &str) -> Result<gl::types::GLuint, String> {
    unsafe {
        let shader = gl::CreateShader(kind);
        let c_str = std::ffi::CString::new(source).unwrap();
        gl::ShaderSource(shader, 1, &c_str.as_ptr(), std::ptr::null());
        gl::CompileShader(shader);

        let mut success = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut success);
        if success == 0 {
            let mut len = 0;
            gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
            let mut buf = vec![0u8; len as usize];
            gl::GetShaderInfoLog(shader, len, std::ptr::null_mut(), buf.as_mut_ptr() as _);
            buf.truncate(buf.iter().position(|&c| c == 0).unwrap_or(buf.len()));
            gl::DeleteShader(shader);
            return Err(String::from_utf8_lossy(&buf).into_owned());
        }
        Ok(shader)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RsTonemapGL {
    const NAME: &'static str = "GstRsTonemapGL";
    type Type = super::RsTonemapGL;
    type ParentType = gst_gl::GLFilter;
}

impl ObjectImpl for RsTonemapGL {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![glib::ParamSpecBoolean::builder("force-active")
                .nick("Force active")
                .blurb("Force tonemapping active even if colorimetry is not HDR")
                .default_value(false)
                .mutable_playing()
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "force-active" => {
                let mut settings = self.settings.lock().unwrap();
                settings.force_active = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "force-active" => self.settings.lock().unwrap().force_active.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RsTonemapGL {}

impl ElementImpl for RsTonemapGL {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "HDR Tonemapper (GL)",
                "Filter/Effect/Converter/Video",
                "Tonemaps HDR (PQ/HLG BT.2020) video to SDR (BT.709) using GL shaders",
                "Tella <dev@tella.tv>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BaseTransformImpl for RsTonemapGL {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl GLBaseFilterImpl for RsTonemapGL {
    fn gl_set_caps(
        &self,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        let force = self.settings.lock().unwrap().force_active;
        let hdr = detect_hdr_transfer(incaps);
        let active = hdr || force;

        gst::info!(
            CAT,
            imp = self,
            "GL tonemapping {}: hdr={}, force={}",
            if active { "active" } else { "passthrough" },
            hdr,
            force,
        );

        *self.active.lock().unwrap() = active;

        self.parent_gl_set_caps(incaps, outcaps)
    }

    fn gl_start(&self) -> Result<(), gst::LoggableError> {
        let context = GLBaseFilterExt::context(&*self.obj())
            .ok_or_else(|| gst::loggable_error!(CAT, "No GL context"))?;

        gl::load_with(|name| context.proc_address(name) as *const _);

        let vs = compile_shader(gl::VERTEX_SHADER, VERTEX_SHADER)
            .map_err(|e| gst::loggable_error!(CAT, "Vertex shader: {}", e))?;
        let fs = compile_shader(gl::FRAGMENT_SHADER, FRAGMENT_SHADER)
            .map_err(|e| gst::loggable_error!(CAT, "Fragment shader: {}", e))?;

        let program;
        let tex_loc;
        let active_loc;
        let mut vao = 0;
        let mut vbo = 0;
        let mut fbo = 0;

        unsafe {
            program = gl::CreateProgram();
            gl::AttachShader(program, vs);
            gl::AttachShader(program, fs);
            gl::BindAttribLocation(program, 0, b"a_position\0".as_ptr() as _);
            gl::BindAttribLocation(program, 1, b"a_texcoord\0".as_ptr() as _);
            gl::LinkProgram(program);

            let mut success = 0;
            gl::GetProgramiv(program, gl::LINK_STATUS, &mut success);
            if success == 0 {
                gl::DeleteProgram(program);
                gl::DeleteShader(vs);
                gl::DeleteShader(fs);
                return Err(gst::loggable_error!(CAT, "Shader link failed"));
            }
            gl::DeleteShader(vs);
            gl::DeleteShader(fs);

            tex_loc = gl::GetUniformLocation(program, b"tex\0".as_ptr() as _);
            active_loc = gl::GetUniformLocation(program, b"active\0".as_ptr() as _);

            // Fullscreen quad: position (x,y) + texcoord (s,t)
            #[rustfmt::skip]
            let vertices: [f32; 16] = [
                -1.0, -1.0,  0.0, 0.0,
                 1.0, -1.0,  1.0, 0.0,
                -1.0,  1.0,  0.0, 1.0,
                 1.0,  1.0,  1.0, 1.0,
            ];

            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);
            gl::GenFramebuffers(1, &mut fbo);

            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as _,
                vertices.as_ptr() as _,
                gl::STATIC_DRAW,
            );

            let stride = 4 * std::mem::size_of::<f32>() as gl::types::GLsizei;
            gl::EnableVertexAttribArray(0);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, stride, std::ptr::null());
            gl::EnableVertexAttribArray(1);
            gl::VertexAttribPointer(
                1,
                2,
                gl::FLOAT,
                gl::FALSE,
                stride,
                (2 * std::mem::size_of::<f32>()) as _,
            );

            gl::BindVertexArray(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
        }

        *self.gl_state.lock().unwrap() = Some(GlState {
            program,
            vao,
            vbo,
            fbo,
            active_loc,
            tex_loc,
        });

        gst::info!(CAT, imp = self, "GL resources initialized");
        self.parent_gl_start()
    }

    fn gl_stop(&self) {
        self.parent_gl_stop();
        *self.gl_state.lock().unwrap() = None;
        gst::info!(CAT, imp = self, "GL resources released");
    }
}

impl GLFilterImpl for RsTonemapGL {
    const MODE: GLFilterMode = GLFilterMode::Texture;

    fn transform_internal_caps(
        &self,
        _direction: gst::PadDirection,
        caps: &gst::Caps,
        _filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        Some(caps.clone())
    }

    fn filter_texture(
        &self,
        input: &gst_gl::GLMemory,
        output: &gst_gl::GLMemory,
    ) -> Result<(), gst::LoggableError> {
        let gl_guard = self.gl_state.lock().unwrap();
        let gl = gl_guard
            .as_ref()
            .ok_or_else(|| gst::loggable_error!(CAT, "GL state not initialized"))?;

        let active = *self.active.lock().unwrap();
        let in_tex = input.texture_id();
        let out_tex = output.texture_id();
        let width = output.texture_width();
        let height = output.texture_height();

        unsafe {
            // Bind output texture to FBO
            gl::BindFramebuffer(gl::FRAMEBUFFER, gl.fbo);
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                out_tex,
                0,
            );
            gl::Viewport(0, 0, width as _, height as _);

            // Bind shader + input texture
            gl::UseProgram(gl.program);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, in_tex);
            gl::Uniform1i(gl.tex_loc, 0);
            gl::Uniform1i(gl.active_loc, active as _);

            // Draw fullscreen quad
            gl::BindVertexArray(gl.vao);
            gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);

            // Cleanup
            gl::BindVertexArray(0);
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::UseProgram(0);
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }

        Ok(())
    }
}
