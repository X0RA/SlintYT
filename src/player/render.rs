use super::gl::Texture;
use super::native::{Loader, RenderContext};
use anyhow::{Result, anyhow};
use glow::HasContext;
use libmpv2::Mpv;
use std::{num::NonZeroU32, rc::Rc, sync::Arc};

pub struct Renderer {
    // Field order matters: destroy the render context before its GL resources
    // and the owning Arc<Mpv>. No mpv instance is leaked to obtain a lifetime.
    context: RenderContext,
    texture: Texture,
    published: bool,
}

impl Renderer {
    pub fn new(
        engine: Arc<Mpv>,
        loader: &Loader<'_>,
        redraw: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let gl = Rc::new(unsafe { glow::Context::from_loader_function_cstr(|name| loader(name)) });
        let texture = Texture::new(gl, 1, 1)?;
        let context = RenderContext::new(engine, loader, redraw)?;
        Ok(Self {
            context,
            texture,
            published: false,
        })
    }

    pub fn render(&mut self, width: u32, height: u32) -> Result<Option<slint::Image>> {
        let (width, height) = (width.max(1), height.max(1));
        if self.texture.width != width || self.texture.height != height {
            self.texture = Texture::new(self.texture.gl.clone(), width, height)?;
            self.published = false;
        }
        self.context.update();
        unsafe {
            let gl = &self.texture.gl;
            let previous_draw =
                NonZeroU32::new(gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING) as u32)
                    .map(glow::NativeFramebuffer);
            let previous_read =
                NonZeroU32::new(gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING) as u32)
                    .map(glow::NativeFramebuffer);
            let rendered = self.context.render(
                self.texture.fbo.0.get() as i32,
                width as i32,
                height as i32,
                false,
            );
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, previous_draw);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, previous_read);
            rendered.map_err(|_| anyhow!("Could not render the video frame."))?;
        }
        #[cfg(test)]
        super::smoke::record_frame(&self.texture);
        if self.published {
            return Ok(None);
        }
        self.published = true;
        // SAFETY: texture belongs to this renderer, is used on the GL thread,
        // and is released only on resize or RenderingTeardown.
        Ok(Some(unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.texture.texture.0,
                (width, height).into(),
            )
            .build()
        }))
    }
}
