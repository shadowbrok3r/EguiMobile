//! GPU-backed video for Glow applications. The SurfaceTexture feeds an external-OES sampler;
//! no decoded pixels cross JNI or become an egui ColorImage.
use egui::{Rect, Ui};
use glow::HasContext;
use jni::objects::{GlobalRef, JClass, JFloatArray, JLongArray, JObject, JString, JValue};
use std::sync::Arc;
const TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

struct Inner {
    gl: Arc<glow::Context>,
    peer: GlobalRef,
    texture: glow::Texture,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(mut env) = super::attach_env() {
            let _ = env.call_method(self.peer.as_obj(), "close", "()V", &[]);
            let _ = env.exception_clear();
        }
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_texture(self.texture);
        }
    }
}

/// A decoder target that can also be handed to LibVLC or a MediaCodec via [`Self::java_surface`].
/// Create and paint it on the render thread of a `Backend::Glow` app. Keep it alive until the
/// external decoder has released its Surface. Standard SDR video is composited with egui's UI.
#[derive(Clone)]
pub struct GpuVideoSurface(Arc<Inner>);

impl GpuVideoSurface {
    pub fn new() -> Result<Self, String> {
        let gl = crate::glow_context().ok_or("GPU video requires Backend::Glow")?;
        let mut env = super::attach_env().ok_or("No Android VM")?;
        let create = || -> Result<_, String> {
            unsafe {
                let texture = gl.create_texture()?;
                gl.active_texture(glow::TEXTURE1);
                gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(texture));
                gl.tex_parameter_i32(
                    TEXTURE_EXTERNAL_OES,
                    glow::TEXTURE_MIN_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    TEXTURE_EXTERNAL_OES,
                    glow::TEXTURE_MAG_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    TEXTURE_EXTERNAL_OES,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    TEXTURE_EXTERNAL_OES,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
                let program = gl.create_program()?;
                for (kind, source) in [
                    (
                        glow::VERTEX_SHADER,
                        "#version 100\nattribute vec2 a_position; varying vec2 v_uv; uniform mat4 u_transform; void main(){gl_Position=vec4(a_position,0.0,1.0);v_uv=(u_transform*vec4((a_position+1.0)*0.5,0.0,1.0)).xy;}",
                    ),
                    (
                        glow::FRAGMENT_SHADER,
                        "#version 100\n#extension GL_OES_EGL_image_external : require\nprecision mediump float; varying vec2 v_uv; uniform samplerExternalOES u_video; void main(){gl_FragColor=vec4(texture2D(u_video,v_uv).rgb,1.0);}",
                    ),
                ] {
                    let shader = gl.create_shader(kind)?;
                    gl.shader_source(shader, source);
                    gl.compile_shader(shader);
                    if !gl.get_shader_compile_status(shader) {
                        let error = gl.get_shader_info_log(shader);
                        gl.delete_shader(shader);
                        gl.delete_program(program);
                        gl.delete_texture(texture);
                        return Err(error);
                    }
                    gl.attach_shader(program, shader);
                    gl.delete_shader(shader);
                }
                gl.bind_attrib_location(program, 0, "a_position");
                gl.link_program(program);
                if !gl.get_program_link_status(program) {
                    let error = gl.get_program_info_log(program);
                    gl.delete_program(program);
                    gl.delete_texture(texture);
                    return Err(error);
                }
                let vao = gl.create_vertex_array()?;
                let vbo = gl.create_buffer()?;
                gl.bind_vertex_array(Some(vao));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                let points: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
                let bytes = std::slice::from_raw_parts(points.as_ptr().cast::<u8>(), 32);
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
                gl.enable_vertex_attrib_array(0);
                gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
                gl.bind_vertex_array(None);
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
                gl.bind_texture(TEXTURE_EXTERNAL_OES, None);
                gl.active_texture(glow::TEXTURE0);
                Ok((texture, program, vao, vbo))
            }
        };
        let (texture, program, vao, vbo) = create()?;
        let peer = env.with_local_frame(16, |env| -> jni::errors::Result<GlobalRef> {
            let activity =
                unsafe { JObject::from_raw(ndk_context::android_context().context().cast()) };
            let loader = env
                .call_method(
                    &activity,
                    "getClassLoader",
                    "()Ljava/lang/ClassLoader;",
                    &[],
                )?
                .l()?;
            let name = env.new_string("com.github.egui_mobile.EguiVideoTexture")?;
            let class = env
                .call_method(
                    loader,
                    "loadClass",
                    "(Ljava/lang/String;)Ljava/lang/Class;",
                    &[(&name).into()],
                )?
                .l()?;
            let object = env.new_object(
                JClass::from(class),
                "(I)V",
                &[JValue::Int(texture.0.get() as i32)],
            )?;
            env.new_global_ref(object)
        });
        match peer {
            Ok(peer) => Ok(Self(Arc::new(Inner {
                gl,
                peer,
                texture,
                program,
                vao,
                vbo,
            }))),
            Err(error) => {
                let _ = env.exception_clear();
                unsafe {
                    gl.delete_vertex_array(vao);
                    gl.delete_buffer(vbo);
                    gl.delete_program(program);
                    gl.delete_texture(texture);
                }
                Err(error.to_string())
            }
        }
    }

    /// A global JNI reference to android.view.Surface, valid while this object is alive.
    pub fn java_surface(&self) -> Result<GlobalRef, String> {
        let mut env = super::attach_env().ok_or("No Android VM")?;
        let result = env.with_local_frame(8, |env| -> jni::errors::Result<GlobalRef> {
            let surface = env
                .call_method(
                    self.0.peer.as_obj(),
                    "surface",
                    "()Landroid/view/Surface;",
                    &[],
                )?
                .l()?;
            env.new_global_ref(surface)
        });
        let _ = env.exception_clear();
        result.map_err(|e| e.to_string())
    }

    /// Set the producer's buffer dimensions before attaching an external decoder.
    /// Its output window size must match these dimensions, not the egui viewport size.
    pub fn set_buffer_size(&self, width: u32, height: u32) {
        if let Some(mut env) = super::attach_env() {
            let _ = env.call_method(
                self.0.peer.as_obj(),
                "bufferSize",
                "(II)V",
                &[
                    JValue::Int(width.max(1) as i32),
                    JValue::Int(height.max(1) as i32),
                ],
            );
            let _ = env.exception_clear();
        }
    }

    pub fn paint(&self, ui: &Ui, rect: Rect) {
        let inner = self.0.clone();
        ui.painter().add(egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |info, _painter| {
                let Some(mut env) = super::attach_env() else {
                    return;
                };
                unsafe {
                    inner.gl.active_texture(glow::TEXTURE1);
                }
                let mut matrix = [0.0f32; 16];
                let result = env.with_local_frame(8, |env| -> jni::errors::Result<()> {
                    let array = env
                        .call_method(inner.peer.as_obj(), "update", "()[F", &[])?
                        .l()?;
                    env.get_float_array_region(JFloatArray::from(array), 0, &mut matrix)
                });
                if result.is_err() {
                    let _ = env.exception_clear();
                    return;
                }
                let gl = &inner.gl;
                let viewport = info.viewport_in_pixels();
                unsafe {
                    let mut previous = [0; 4];
                    gl.get_parameter_i32_slice(glow::VIEWPORT, &mut previous);
                    gl.viewport(
                        viewport.left_px,
                        viewport.from_bottom_px,
                        viewport.width_px,
                        viewport.height_px,
                    );
                    gl.disable(glow::BLEND);
                    gl.disable(glow::DEPTH_TEST);
                    gl.disable(glow::CULL_FACE);
                    gl.use_program(Some(inner.program));
                    gl.bind_vertex_array(Some(inner.vao));
                    gl.active_texture(glow::TEXTURE1);
                    gl.bind_texture(TEXTURE_EXTERNAL_OES, Some(inner.texture));
                    gl.bind_sampler(1, None);
                    gl.tex_parameter_i32(
                        TEXTURE_EXTERNAL_OES,
                        glow::TEXTURE_MIN_FILTER,
                        glow::LINEAR as i32,
                    );
                    gl.tex_parameter_i32(
                        TEXTURE_EXTERNAL_OES,
                        glow::TEXTURE_MAG_FILTER,
                        glow::LINEAR as i32,
                    );
                    gl.tex_parameter_i32(
                        TEXTURE_EXTERNAL_OES,
                        glow::TEXTURE_WRAP_S,
                        glow::CLAMP_TO_EDGE as i32,
                    );
                    gl.tex_parameter_i32(
                        TEXTURE_EXTERNAL_OES,
                        glow::TEXTURE_WRAP_T,
                        glow::CLAMP_TO_EDGE as i32,
                    );
                    gl.uniform_1_i32(
                        gl.get_uniform_location(inner.program, "u_video").as_ref(),
                        1,
                    );
                    gl.uniform_matrix_4_f32_slice(
                        gl.get_uniform_location(inner.program, "u_transform")
                            .as_ref(),
                        false,
                        &matrix,
                    );
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                    let error = gl.get_error();
                    if error != glow::NO_ERROR {
                        log::error!("GPU video GL error: {error:#x}");
                    }
                    gl.bind_texture(TEXTURE_EXTERNAL_OES, None);
                    gl.bind_vertex_array(None);
                    gl.use_program(None);
                    gl.active_texture(glow::TEXTURE0);
                    gl.viewport(previous[0], previous[1], previous[2], previous[3]);
                }
            })),
        });
    }
}

#[derive(Default, Debug)]
pub struct SurfaceStatus {
    pub position_ms: i64,
    pub duration_ms: i64,
    pub width: u32,
    pub height: u32,
    pub ready: bool,
    pub playing: bool,
    pub fps: f64,
    pub frame_count: i64,
    pub has_audio: bool,
    pub error: Option<String>,
}

/// Platform MediaPlayer with GPU presentation and the platform's synchronized audio clock.
/// Codec/container support follows Android; apps needing additional demuxers can use
/// `GpuVideoSurface` with another player. This API does not allocate per-frame RGBA buffers.
pub struct SurfacePlayer {
    surface: GpuVideoSurface,
}
impl SurfacePlayer {
    pub fn open(path: &str) -> Result<Self, String> {
        let surface = GpuVideoSurface::new()?;
        let mut env = super::attach_env().ok_or("No Android VM")?;
        let text = env.new_string(path).map_err(|e| e.to_string())?;
        let result = env.call_method(
            surface.0.peer.as_obj(),
            "open",
            "(Ljava/lang/String;)V",
            &[(&text).into()],
        );
        let _ = env.exception_clear();
        result.map_err(|e| e.to_string())?;
        Ok(Self { surface })
    }
    fn boolean(&self, method: &str, value: bool) {
        if let Some(mut env) = super::attach_env() {
            let _ = env.call_method(
                self.surface.0.peer.as_obj(),
                method,
                "(Z)V",
                &[JValue::Bool(value as u8)],
            );
            let _ = env.exception_clear();
        }
    }
    pub fn set_playing(&self, value: bool) {
        self.boolean("playing", value);
    }
    pub fn set_muted(&self, value: bool) {
        self.boolean("muted", value);
    }
    pub fn set_looping(&self, value: bool) {
        self.boolean("looping", value);
    }
    pub fn seek(&self, ms: i64) {
        if let Some(mut env) = super::attach_env() {
            let _ = env.call_method(
                self.surface.0.peer.as_obj(),
                "seek",
                "(J)V",
                &[JValue::Long(ms)],
            );
            let _ = env.exception_clear();
        }
    }
    pub fn paint(&self, ui: &Ui, rect: Rect) {
        self.surface.paint(ui, rect);
    }
    pub fn status(&self) -> SurfaceStatus {
        let Some(mut env) = super::attach_env() else {
            return SurfaceStatus::default();
        };
        let result = env.with_local_frame(8, |env| -> jni::errors::Result<SurfaceStatus> {
            let object = env
                .call_method(self.surface.0.peer.as_obj(), "status", "()[J", &[])?
                .l()?;
            let mut values = [0i64; 9];
            env.get_long_array_region(JLongArray::from(object), 0, &mut values)?;
            let error = env
                .call_method(
                    self.surface.0.peer.as_obj(),
                    "error",
                    "()Ljava/lang/String;",
                    &[],
                )?
                .l()?;
            let error: String = env.get_string(&JString::from(error))?.into();
            Ok(SurfaceStatus {
                position_ms: values[0],
                duration_ms: values[1],
                width: values[2] as u32,
                height: values[3] as u32,
                ready: values[4] != 0,
                playing: values[5] != 0,
                fps: values[6] as f64 / 1000.0,
                frame_count: values[7],
                has_audio: values[8] != 0,
                error: (!error.is_empty()).then_some(error),
            })
        });
        let _ = env.exception_clear();
        result.unwrap_or_default()
    }
}
