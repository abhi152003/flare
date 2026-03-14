use std::mem;

use crate::config::window::{GradientConfig, GradientDirection};
use crate::display::SizeInfo;
use crate::gl;
use crate::gl::types::*;
use crate::renderer::shader::{ShaderProgram, ShaderVersion};

/// GLSL3 gradient shader sources.
const GRADIENT_SHADER_V_GLSL3: &str = include_str!("../../res/glsl3/gradient.v.glsl");
const GRADIENT_SHADER_F_GLSL3: &str = include_str!("../../res/glsl3/gradient.f.glsl");

/// GLES2 gradient shader sources.
const GRADIENT_SHADER_V_GLES2: &str = include_str!("../../res/gradient.v.glsl");
const GRADIENT_SHADER_F_GLES2: &str = include_str!("../../res/gradient.f.glsl");

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Vertex {
    x: f32,
    y: f32,
}

#[derive(Debug)]
pub struct GradientRenderer {
    vao: GLuint,
    vbo: GLuint,
    program: ShaderProgram,
    vao_initialized: bool,
}

impl GradientRenderer {
    pub fn new(shader_version: ShaderVersion) -> Result<Self, super::Error> {
        let (vertex_source, fragment_source) = match shader_version {
            ShaderVersion::Glsl3 => (GRADIENT_SHADER_V_GLSL3, GRADIENT_SHADER_F_GLSL3),
            ShaderVersion::Gles2 => (GRADIENT_SHADER_V_GLES2, GRADIENT_SHADER_F_GLES2),
        };

        let program = ShaderProgram::new(shader_version, None, vertex_source, fragment_source)?;

        Ok(Self { vao: 0, vbo: 0, program, vao_initialized: false })
    }

    /// Draw a fullscreen gradient background with optional rounded corners.
    ///
    /// Returns `true` if a gradient was drawn (and the caller should skip the
    /// regular `gl::ClearColor`/`gl::Clear` path).
    pub fn draw(
        &mut self,
        size_info: &SizeInfo,
        gradient: &GradientConfig,
        opacity: f32,
        border_radius: f32,
    ) {
        let width = size_info.width();
        let height = size_info.height();

        if !self.vao_initialized {
            self.init_gl_objects();
        }

        // Gradient colors normalized to [0, 1].
        let start_r = f32::from(gradient.start.r) / 255.0;
        let start_g = f32::from(gradient.start.g) / 255.0;
        let start_b = f32::from(gradient.start.b) / 255.0;
        let end_r = f32::from(gradient.end.r) / 255.0;
        let end_g = f32::from(gradient.end.g) / 255.0;
        let end_b = f32::from(gradient.end.b) / 255.0;

        let direction = match gradient.direction {
            GradientDirection::Vertical => 0,
            GradientDirection::Horizontal => 1,
            GradientDirection::Diagonal => 2,
        };

        unsafe {
            gl::UseProgram(self.program.id());

            if let Ok(loc) = self.program.get_uniform_location(c"gradientStart") {
                gl::Uniform3f(loc, start_r, start_g, start_b);
            }
            if let Ok(loc) = self.program.get_uniform_location(c"gradientEnd") {
                gl::Uniform3f(loc, end_r, end_g, end_b);
            }
            if let Ok(loc) = self.program.get_uniform_location(c"gradientDirection") {
                gl::Uniform1i(loc, direction);
            }
            if let Ok(loc) = self.program.get_uniform_location(c"opacity") {
                gl::Uniform1f(loc, opacity);
            }
            if let Ok(loc) = self.program.get_uniform_location(c"borderRadius") {
                gl::Uniform1f(loc, border_radius);
            }
            if let Ok(loc) = self.program.get_uniform_location(c"windowSize") {
                gl::Uniform2f(loc, width, height);
            }

            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }
    }

    fn init_gl_objects(&mut self) {
        let half_width = 1.0f32;
        let half_height = 1.0f32;

        let quad = [
            // Top-left triangle.
            Vertex { x: -half_width, y: half_height },
            Vertex { x: -half_width, y: -half_height },
            Vertex { x: half_width, y: half_height },
            // Bottom-right triangle.
            Vertex { x: half_width, y: half_height },
            Vertex { x: -half_width, y: -half_height },
            Vertex { x: half_width, y: -half_height },
        ];

        unsafe {
            gl::GenVertexArrays(1, &mut self.vao);
            gl::GenBuffers(1, &mut self.vbo);

            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);

            gl::BufferData(
                gl::ARRAY_BUFFER,
                (quad.len() * mem::size_of::<Vertex>()) as isize,
                quad.as_ptr() as *const _,
                gl::STATIC_DRAW,
            );

            gl::VertexAttribPointer(
                0,
                2,
                gl::FLOAT,
                gl::FALSE,
                mem::size_of::<Vertex>() as i32,
                std::ptr::null(),
            );
            gl::EnableVertexAttribArray(0);

            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
        }

        self.vao_initialized = true;
    }
}

impl Drop for GradientRenderer {
    fn drop(&mut self) {
        if self.vao_initialized {
            unsafe {
                gl::DeleteBuffers(1, &self.vbo);
                gl::DeleteVertexArrays(1, &self.vao);
            }
        }
    }
}
