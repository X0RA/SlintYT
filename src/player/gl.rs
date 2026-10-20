use anyhow::{Result, anyhow};
use glow::HasContext;
use std::{num::NonZeroU32, rc::Rc};

pub struct Texture {
    pub gl: Rc<glow::Context>,
    pub texture: glow::NativeTexture,
    pub fbo: glow::NativeFramebuffer,
    pub width: u32,
    pub height: u32,
}

impl Texture {
    /// Must be created and dropped with Slint's GL context current.
    pub fn new(gl: Rc<glow::Context>, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let texture = gl
                .create_texture()
                .map_err(|_| anyhow!("Could not create the video texture."))?;
            let fbo = match gl.create_framebuffer() {
                Ok(fbo) => fbo,
                Err(_) => {
                    gl.delete_texture(texture);
                    return Err(anyhow!("Could not create the video framebuffer."));
                }
            };
            let previous_texture =
                NonZeroU32::new(gl.get_parameter_i32(glow::TEXTURE_BINDING_2D) as u32)
                    .map(glow::NativeTexture);
            let previous_fbo =
                NonZeroU32::new(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32)
                    .map(glow::NativeFramebuffer);
            let previous_unpack =
                NonZeroU32::new(gl.get_parameter_i32(glow::PIXEL_UNPACK_BUFFER_BINDING) as u32)
                    .map(glow::NativeBuffer);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            for (parameter, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value as i32);
            }
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::DRAW_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let complete =
                gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, previous_fbo);
            gl.bind_texture(glow::TEXTURE_2D, previous_texture);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, previous_unpack);
            let result = Self {
                gl,
                texture,
                fbo,
                width,
                height,
            };
            if !complete {
                return Err(anyhow!("The video framebuffer is unavailable."));
            }
            Ok(result)
        }
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_framebuffer(self.fbo);
            self.gl.delete_texture(self.texture);
        }
    }
}
