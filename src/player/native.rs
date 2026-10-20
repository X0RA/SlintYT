//! Narrow render API binding. Parameters stay on the stack and callbacks are
//! unregistered before their Rust storage is dropped. No libmpv2 render API is used.
use anyhow::{Result, bail};
use libmpv2::Mpv;
use libmpv2_sys as ffi;
use std::{
    ffi::{CStr, c_char, c_void},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{self, NonNull},
    rc::Rc,
    sync::Arc,
};

pub type Loader<'a> = dyn Fn(&CStr) -> *const c_void + 'a;
struct LoaderRef<'a>(&'a Loader<'a>);
struct Callback(Box<dyn Fn() + Send>);

unsafe extern "C" fn load_gl(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    // mpv resolves the OpenGL functions synchronously during context creation.
    // The borrowed loader and both parameter arrays outlive that entire call.
    catch_unwind(AssertUnwindSafe(|| unsafe {
        let loader = &*ctx.cast::<LoaderRef<'_>>();
        (loader.0)(CStr::from_ptr(name)).cast_mut()
    }))
    .unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn redraw(ctx: *mut c_void) {
    // mpv serializes update callbacks. Unregistration synchronizes with an
    // executing callback, so Callback remains valid until it has returned.
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        ((*ctx.cast::<Callback>()).0)();
    }));
}

fn parameter<T>(kind: u32, data: &mut T) -> ffi::mpv_render_param {
    ffi::mpv_render_param {
        type_: kind,
        data: ptr::from_mut(data).cast(),
    }
}

fn end() -> ffi::mpv_render_param {
    ffi::mpv_render_param {
        type_: 0,
        data: ptr::null_mut(),
    }
}

pub struct RenderContext {
    raw: NonNull<ffi::mpv_render_context>,
    _callback: Box<Callback>,
    _engine: Arc<Mpv>,
    // All rendering and destruction must happen on the context's GL thread.
    _thread: PhantomData<Rc<()>>,
}

impl RenderContext {
    pub fn new(
        engine: Arc<Mpv>,
        loader: &Loader<'_>,
        callback: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let mut loader = LoaderRef(loader);
        let mut init = ffi::mpv_opengl_init_params {
            get_proc_address: Some(load_gl),
            get_proc_address_ctx: ptr::from_mut(&mut loader).cast(),
        };
        let mut params = [
            ffi::mpv_render_param {
                type_: ffi::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: c"opengl".as_ptr().cast_mut().cast(),
            },
            parameter(
                ffi::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                &mut init,
            ),
            end(),
        ];
        let mut raw = ptr::null_mut();
        let result = unsafe {
            ffi::mpv_render_context_create(&mut raw, engine.ctx.as_ptr(), params.as_mut_ptr())
        };
        if result < 0 {
            bail!("Could not initialize mpv video rendering.");
        }
        let raw =
            NonNull::new(raw).ok_or_else(|| anyhow::anyhow!("mpv returned no render context."))?;
        let mut callback = Box::new(Callback(Box::new(callback)));
        unsafe {
            ffi::mpv_render_context_set_update_callback(
                raw.as_ptr(),
                Some(redraw),
                ptr::from_mut(callback.as_mut()).cast(),
            );
        }
        Ok(Self {
            raw,
            _callback: callback,
            _engine: engine,
            _thread: PhantomData,
        })
    }

    pub fn update(&self) {
        unsafe {
            ffi::mpv_render_context_update(self.raw.as_ptr());
        }
    }

    pub fn render(&self, fbo: i32, width: i32, height: i32, flip: bool) -> Result<()> {
        let mut target = ffi::mpv_opengl_fbo {
            fbo,
            w: width,
            h: height,
            internal_format: 0,
        };
        let mut flip = i32::from(flip);
        let mut block = 0_i32;
        let mut params = [
            parameter(
                ffi::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
                &mut target,
            ),
            parameter(
                ffi::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
                &mut flip,
            ),
            // Slint owns the event loop; do not wait for frame presentation here.
            parameter(
                ffi::mpv_render_param_type_MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                &mut block,
            ),
            end(),
        ];
        if unsafe { ffi::mpv_render_context_render(self.raw.as_ptr(), params.as_mut_ptr()) } < 0 {
            bail!("Could not render the video frame.");
        }
        Ok(())
    }
}

impl Drop for RenderContext {
    fn drop(&mut self) {
        unsafe {
            ffi::mpv_render_context_set_update_callback(self.raw.as_ptr(), None, ptr::null_mut());
            ffi::mpv_render_context_free(self.raw.as_ptr());
        }
        // Callback storage and the owning Mpv Arc are dropped afterwards.
    }
}
