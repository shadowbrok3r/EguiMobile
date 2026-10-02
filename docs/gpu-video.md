# Android GPU Video (0.1.4)

`video::SurfacePlayer` uses Android MediaPlayer for decoding, seeking and synchronized audio.
`video::GpuVideoSurface` exposes an Android Surface for other engines, including LibVLC.
Both present decoded buffers through an external-OES texture in a Glow paint callback.
No per-frame RGBA readback, JNI pixel array or egui texture upload is involved.

Create, paint and drop these objects on the application's Glow render thread. Keep an external
decoder target alive until that decoder has stopped and released its Surface.

```rust,ignore
let surface = egui_mobile::video::GpuVideoSurface::new()?;
surface.set_buffer_size(video_width, video_height);
let decoder_target = surface.java_surface()?;
// Pass decoder_target to the native player. Its output window dimensions must match
// set_buffer_size(), independently of the smaller egui display rectangle.
surface.paint(ui, display_rect);
```

The producer buffer size is essential: an unsized SurfaceTexture can deliver black frames,
and a mismatched native output window introduces borders or clipping inside the texture.
SurfaceTexture's transform is applied when sampling. Each paint respects egui's clipping.
Apps should request repaint at the playback frame rate and pause playback on backgrounding.
`SurfacePlayer::seek` also queues a seek requested before preparation completes.

This path targets SDR compositing. It does not implement HDR passthrough or tone mapping.
Hardware acceleration depends on Android's support for the exact codec/profile/chroma format;
an NPU is not a replacement for a video decoder. Container support in SurfacePlayer follows
Android. Use another demuxer with GpuVideoSurface for formats such as Sony MP4 with PCM audio.

Custom Activity bridges can use `egui_mobile::with_native_activity`. Unlike ndk-context's
Application reference, it supplies the actual Activity and manages a bounded JNI local frame.
