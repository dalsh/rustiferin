//! Import a KMS dma-buf plane into GL via EGL, GPU-downscale it, and read the
//! small result back to CPU pixels.
//!
//! The gsr-kms-server hands us the scanout framebuffer as a dma-buf with a
//! concrete DRM fourcc + modifier. We import it as an `EGLImage`
//! (`EGL_LINUX_DMA_BUF_EXT`), sample it through a passthrough shader into a small
//! offscreen FBO (linear filtering does the box-average), then `glReadPixels`
//! only that small buffer. This keeps the GPU->CPU transfer tiny (the expensive
//! part) and normalises exotic scanout formats (e.g. 10-bit `ABGR2101010`) to
//! 8-bit for free. The downscale is why the KMS path forces `subsample = 1`
//! downstream: the reduction already happened on the GPU.
//!
//! A headless GLES2 context on the capturing GPU's render node (via GBM) does
//! the work; no window/surface is involved.

use std::cell::Cell;
use std::ffi::c_void;
use std::fs::File;

use anyhow::{anyhow, bail, Context as _, Result};
use gbm::AsRaw as _;
use glow::HasContext as _;
use khronos_egl as egl;

use super::protocol::KmsResponseItem;
use crate::capture::PixelFormat;

// EGL_EXT_image_dma_buf_import attribute keys (not in khronos-egl core).
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;
const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;

/// Downscale target long-edge. The source is reduced to at most this width
/// (aspect preserved), cutting readback ~16x on a 2560-wide scanout while
/// leaving every edge zone comfortably many pixels to average.
const DOWNSCALE_WIDTH: u32 = 640;

const VERT_SRC: &str = r#"
attribute vec2 a_pos;
attribute vec2 a_uv;
varying vec2 v_uv;
void main() {
    v_uv = a_uv;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

const FRAG_SRC: &str = r#"
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
void main() {
    gl_FragColor = texture2D(u_tex, v_uv);
}
"#;

type EglImageTargetTexture2DOes = unsafe extern "system" fn(target: u32, image: *const c_void);

pub struct KmsImporter {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    context: egl::Context,
    gl: glow::Context,
    image_target_texture: EglImageTargetTexture2DOes,
    program: glow::Program,
    quad_vbo: glow::Buffer,
    /// Texture the imported dma-buf `EGLImage` is bound to (sampled, not read).
    source_texture: glow::Texture,
    /// Small texture the downscale renders into, and the FBO wrapping it.
    dest_texture: glow::Texture,
    dest_fbo: glow::Framebuffer,
    /// Current allocated size of `dest_texture`, so we only reallocate on change.
    dest_size: Cell<(u32, u32)>,
    // Kept alive: the GBM device backs the EGL display; the File backs the GBM device.
    _gbm: gbm::Device<File>,
}

impl KmsImporter {
    /// Set up a headless GLES context on `render_node` (e.g. `/dev/dri/renderD128`)
    /// that can import dma-bufs allocated by that GPU.
    pub fn new(render_node: &str) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(render_node)
            .with_context(|| format!("open render node {render_node}"))?;
        let gbm = gbm::Device::new(file)
            .with_context(|| format!("create gbm device on {render_node}"))?;

        let egl = egl::Instance::new(egl::Static);
        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm.as_raw() as egl::NativeDisplayType,
                &[egl::ATTRIB_NONE],
            )
            .context("eglGetPlatformDisplay(GBM)")?
        };
        let (major, minor) = egl.initialize(display).context("eglInitialize")?;
        if let Ok(exts) = egl.query_string(Some(display), egl::EXTENSIONS) {
            tracing::info!(
                egl_version = format!("{major}.{minor}"),
                dma_buf_import = exts
                    .to_string_lossy()
                    .contains("EGL_EXT_image_dma_buf_import"),
                "egl display initialized"
            );
        }
        egl.bind_api(egl::OPENGL_ES_API).context("eglBindAPI")?;

        // Surfaceless: we never create a window/pbuffer surface, so don't
        // constrain SURFACE_TYPE (GBM configs don't advertise PBUFFER_BIT).
        let config = egl
            .choose_first_config(
                display,
                &[egl::RENDERABLE_TYPE, egl::OPENGL_ES2_BIT, egl::NONE],
            )
            .context("eglChooseConfig")?
            .ok_or_else(|| anyhow!("no EGL config with ES2 renderable type"))?;

        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_MAJOR_VERSION, 2, egl::NONE],
            )
            .context("eglCreateContext")?;
        egl.make_current(display, None, None, Some(context))
            .context("eglMakeCurrent (surfaceless)")?;

        let image_target_texture: EglImageTargetTexture2DOes = {
            let ptr = egl
                .get_proc_address("glEGLImageTargetTexture2DOES")
                .ok_or_else(|| anyhow!("glEGLImageTargetTexture2DOES unavailable"))?;
            // SAFETY: the GL entry point has this exact signature.
            unsafe { std::mem::transmute::<extern "system" fn(), EglImageTargetTexture2DOes>(ptr) }
        };

        // SAFETY: context is current; loader resolves GL entry points via EGL.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                egl.get_proc_address(s)
                    .map_or(std::ptr::null(), |p| p as *const c_void)
            })
        };

        let (program, quad_vbo, source_texture, dest_texture, dest_fbo) =
            unsafe { build_gl_objects(&gl)? };

        Ok(Self {
            egl,
            display,
            context,
            gl,
            image_target_texture,
            program,
            quad_vbo,
            source_texture,
            dest_texture,
            dest_fbo,
            dest_size: Cell::new((0, 0)),
            _gbm: gbm,
        })
    }

    /// Import `plane`'s first dma-buf, GPU-downscale it, and read the small
    /// result into `out` as 8-bit RGBA. Returns the downscaled `(width, height,
    /// format)`. The plane's dma-buf fds remain owned by the caller.
    pub fn read_plane(
        &self,
        plane: &KmsResponseItem,
        out: &mut Vec<u8>,
    ) -> Result<(u32, u32, PixelFormat)> {
        if plane.num_dma_bufs < 1 {
            bail!("plane has no dma-bufs");
        }
        let buf = &plane.dma_buf[0];
        let (sw, sh) = (plane.width, plane.height);
        if sw == 0 || sh == 0 {
            bail!("plane has zero dimension");
        }
        let (dw, dh) = downscaled_size(sw, sh);

        let attribs: [egl::Attrib; 17] = [
            egl::WIDTH as egl::Attrib,
            sw as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            sh as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            plane.pixel_format as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            buf.fd as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            buf.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            buf.pitch as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (plane.modifier & 0xffff_ffff) as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (plane.modifier >> 32) as egl::Attrib,
            egl::ATTRIB_NONE,
        ];

        // SAFETY: NO_CONTEXT / null client buffer are the documented values for
        // dma-buf import; attribs describe a single-plane image.
        let image = self
            .egl
            .create_image(
                self.display,
                unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) },
                EGL_LINUX_DMA_BUF_EXT,
                unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) },
                &attribs,
            )
            .context("eglCreateImage(dma-buf)")?;

        let result =
            unsafe { self.downscale_and_read(image.as_ptr() as *const c_void, dw, dh, out) };
        let _ = self.egl.destroy_image(self.display, image);
        result?;
        // glReadPixels returns RGBA byte order regardless of the source format.
        Ok((dw, dh, PixelFormat::Rgba))
    }

    /// Bind the imported image to the source texture, render it downscaled into
    /// the dest FBO, and read the dest back. Caller owns `image` lifetime.
    unsafe fn downscale_and_read(
        &self,
        image: *const c_void,
        dw: u32,
        dh: u32,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let gl = &self.gl;

        // Bind the dma-buf image to the source texture with linear filtering so
        // the downscale averages neighbouring texels.
        gl.bind_texture(glow::TEXTURE_2D, Some(self.source_texture));
        (self.image_target_texture)(glow::TEXTURE_2D, image);
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );

        // (Re)allocate the dest texture if the target size changed.
        if self.dest_size.get() != (dw, dh) {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.dest_texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                dw as i32,
                dh as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                None,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            self.dest_size.set((dw, dh));
        }

        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.dest_fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(self.dest_texture),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status != glow::FRAMEBUFFER_COMPLETE {
            bail!("downscale framebuffer incomplete: {status:#x}");
        }

        gl.viewport(0, 0, dw as i32, dh as i32);
        gl.use_program(Some(self.program));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.source_texture));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.quad_vbo));
        // Attribute 0 = a_pos (vec2), attribute 1 = a_uv (vec2), interleaved.
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

        out.clear();
        out.resize((dw as usize) * (dh as usize) * 4, 0);
        gl.read_pixels(
            0,
            0,
            dw as i32,
            dh as i32,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(out.as_mut_slice()),
        );
        let err = gl.get_error();
        if err != glow::NO_ERROR {
            bail!("gl error during downscale/readback: {err:#x}");
        }
        Ok(())
    }
}

/// Downscaled size for a source of `sw`x`sh`: width capped at [`DOWNSCALE_WIDTH`],
/// height scaled to preserve aspect (both at least 1).
fn downscaled_size(sw: u32, sh: u32) -> (u32, u32) {
    if sw <= DOWNSCALE_WIDTH {
        return (sw, sh);
    }
    let dw = DOWNSCALE_WIDTH;
    let dh = ((sh as u64 * dw as u64) / sw as u64).max(1) as u32;
    (dw, dh)
}

/// Build the shader program, fullscreen-quad VBO, and source/dest textures + FBO.
unsafe fn build_gl_objects(
    gl: &glow::Context,
) -> Result<(
    glow::Program,
    glow::Buffer,
    glow::Texture,
    glow::Texture,
    glow::Framebuffer,
)> {
    let program = link_program(gl, VERT_SRC, FRAG_SRC)?;
    // Bind attribute locations explicitly so the draw path can assume 0/1.
    // (glow has no bind_attrib_location; we instead query after link below.)
    let a_pos = gl.get_attrib_location(program, "a_pos");
    let a_uv = gl.get_attrib_location(program, "a_uv");
    if a_pos != Some(0) || a_uv != Some(1) {
        // Locations are driver-assigned; enforce our assumption or fail loudly.
        // NVIDIA assigns in declaration order (a_pos=0, a_uv=1); bail otherwise.
        bail!("unexpected attribute locations: a_pos={a_pos:?} a_uv={a_uv:?}");
    }
    if let Some(loc) = gl.get_uniform_location(program, "u_tex") {
        gl.use_program(Some(program));
        gl.uniform_1_i32(Some(&loc), 0);
    }

    // Fullscreen quad as a triangle strip: (x, y, u, v) interleaved.
    // glReadPixels reads the dest FBO bottom-up, so to land screen-top at
    // readback row 0 (pipeline y=0) the bottom clip vertices (y=-1) must sample
    // the source's top (v=0). Verified against a live capture (kms_probe PNG).
    #[rustfmt::skip]
    let quad: [f32; 16] = [
        -1.0, -1.0,  0.0, 0.0,
         1.0, -1.0,  1.0, 0.0,
        -1.0,  1.0,  0.0, 1.0,
         1.0,  1.0,  1.0, 1.0,
    ];
    let quad_vbo = gl.create_buffer().map_err(|e| anyhow!("create vbo: {e}"))?;
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(quad_vbo));
    gl.buffer_data_u8_slice(
        glow::ARRAY_BUFFER,
        std::slice::from_raw_parts(quad.as_ptr().cast::<u8>(), std::mem::size_of_val(&quad)),
        glow::STATIC_DRAW,
    );

    let source_texture = gl
        .create_texture()
        .map_err(|e| anyhow!("source tex: {e}"))?;
    let dest_texture = gl.create_texture().map_err(|e| anyhow!("dest tex: {e}"))?;
    let dest_fbo = gl
        .create_framebuffer()
        .map_err(|e| anyhow!("dest fbo: {e}"))?;
    Ok((program, quad_vbo, source_texture, dest_texture, dest_fbo))
}

unsafe fn link_program(gl: &glow::Context, vs: &str, fs: &str) -> Result<glow::Program> {
    let compile = |ty: u32, src: &str| -> Result<glow::Shader> {
        let sh = gl
            .create_shader(ty)
            .map_err(|e| anyhow!("create shader: {e}"))?;
        gl.shader_source(sh, src);
        gl.compile_shader(sh);
        if !gl.get_shader_compile_status(sh) {
            bail!("shader compile failed: {}", gl.get_shader_info_log(sh));
        }
        Ok(sh)
    };
    let vs = compile(glow::VERTEX_SHADER, vs)?;
    let fs = compile(glow::FRAGMENT_SHADER, fs)?;
    let program = gl
        .create_program()
        .map_err(|e| anyhow!("create program: {e}"))?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        bail!("program link failed: {}", gl.get_program_info_log(program));
    }
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    Ok(program)
}

impl Drop for KmsImporter {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_framebuffer(self.dest_fbo);
            self.gl.delete_texture(self.dest_texture);
            self.gl.delete_texture(self.source_texture);
            self.gl.delete_buffer(self.quad_vbo);
            self.gl.delete_program(self.program);
        }
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

#[cfg(test)]
mod tests {
    use super::downscaled_size;

    #[test]
    fn downscale_preserves_aspect_and_caps_width() {
        assert_eq!(downscaled_size(2560, 1440), (640, 360));
        assert_eq!(downscaled_size(3440, 1440), (640, 267));
    }

    #[test]
    fn downscale_leaves_small_sources_untouched() {
        assert_eq!(downscaled_size(640, 360), (640, 360));
        assert_eq!(downscaled_size(320, 200), (320, 200));
    }
}
