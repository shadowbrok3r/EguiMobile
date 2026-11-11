//! Microphone capture that survives the screen turning off:
//! [`EguiMicService`](../../java/com/github/egui_mobile/EguiMicService.java), a foreground
//! service of type `microphone` that records with `AudioRecord` and hands the PCM down.
//!
//! [`start`] needs a visible Activity, because Android only lets a microphone service start
//! from the foreground. Everything after that — [`hold`], [`set_text`], [`release`], the sink
//! and the events — works from any thread with or without one, so a recording carries on and
//! can be finished after the task is swiped away.
//!
//! The service has two phases. While capturing it shows a running timer and a Stop button.
//! [`hold`] ends the capture but keeps the service, the process and a wake lock, for work on
//! the recording that outlasts it; only [`release`] takes the notification down.
//!
//! The app's manifest declares the service (`foreground_service_type = "microphone"`) and the
//! `RECORD_AUDIO`, `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_MICROPHONE` and `WAKE_LOCK`
//! permissions; `POST_NOTIFICATIONS` makes the row visible.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use jni::JavaVM;
use jni::objects::{GlobalRef, JClass, JObject, JShortArray, JString, JValue};

use crate::host::with_native_activity;

const SERVICE_CLASS: &str = "com.github.egui_mobile.EguiMicService";
const ACTION_START: &str = "com.github.egui_mobile.action.MIC_START";

type Sink = Box<dyn FnMut(&[i16], u32) + Send>;

static EVENTS: Mutex<VecDeque<MicEvent>> = Mutex::new(VecDeque::new());
static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// The rate the running capture was actually opened at.
static RATE: AtomicU32 = AtomicU32::new(0);
static REGISTERED: OnceLock<bool> = OnceLock::new();
/// The service class, for static calls from threads the Activity's ClassLoader cannot reach.
static SERVICE_CLASS_REF: OnceLock<(JavaVM, GlobalRef)> = OnceLock::new();

/// Which `MediaRecorder.AudioSource` to record from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MicSource {
    /// Tuned for speech recognition: no gain riding, and where the device has one, a far-field
    /// microphone path.
    #[default]
    VoiceRecognition,
    /// The raw capsule where the device exposes it, otherwise the default source.
    Unprocessed,
    /// The device's default processing.
    Default,
}

impl MicSource {
    fn id(self) -> i32 {
        match self {
            MicSource::Default => 1,
            MicSource::VoiceRecognition => 6,
            MicSource::Unprocessed => 9,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MicConfig {
    pub notification_title: String,
    pub notification_text: String,
    /// The rate to ask for. A device that refuses it records at 48 or 44.1 kHz instead, and
    /// [`MicEvent::Started`] says which.
    pub sample_rate: u32,
    pub source: MicSource,
}

impl Default for MicConfig {
    fn default() -> Self {
        Self { notification_title: String::from("Recording"), notification_text: String::new(), sample_rate: 16_000, source: MicSource::default() }
    }
}

#[derive(Debug, Clone)]
pub enum MicEvent {
    /// Capture is running, at this rate. Samples follow through the sink.
    Started { sample_rate: u32 },
    /// The capture thread ended; no sink call follows this.
    Stopped,
    Failed(String),
    /// The user pressed Stop on the notification. Capture has already ended and the service
    /// stays up until [`release`].
    StopRequested,
}

/// Drain one queued service callback. Poll this from whatever owns the recording.
pub fn poll_event() -> Option<MicEvent> {
    EVENTS.lock().ok()?.pop_front()
}

/// Whether a capture thread is alive: from its first sample until [`MicEvent::Stopped`].
pub fn is_capturing() -> bool {
    CAPTURING.load(Ordering::Relaxed)
}

/// Whether `RECORD_AUDIO` is granted, without asking for it.
pub fn permission_granted() -> bool {
    with_native_activity(|env, activity| {
        let permission = env.new_string("android.permission.RECORD_AUDIO")?;
        Ok(env.call_method(activity, "checkSelfPermission", "(Ljava/lang/String;)I", &[(&permission).into()])?.i()? == 0)
    })
    .unwrap_or(false)
}

/// Start the service and the capture. `sink` receives mono 16-bit PCM in roughly 100 ms pieces,
/// with the rate it was captured at, on the service's capture thread — so it must not block on
/// the UI. `RECORD_AUDIO` must already be granted and the Activity visible.
///
/// Refused while an earlier capture is still winding down, and events of earlier captures
/// still queued are dropped: what [`poll_event`] returns after this is about this capture.
pub fn start(config: &MicConfig, sink: impl FnMut(&[i16], u32) + Send + 'static) -> bool {
    if !register_natives() { return false; }
    // Its samples would land in the new sink, and its end would read as this capture's.
    if CAPTURING.load(Ordering::Relaxed) {
        log::warn!("egui-android mic: start refused, the previous capture has not ended");
        return false;
    }
    if let Ok(mut events) = EVENTS.lock() {
        events.clear();
    }
    if let Ok(mut slot) = SINK.lock() {
        *slot = Some(Box::new(sink));
    }

    let started = with_native_activity(|env, activity| {
        let intent = service_intent(env, activity, ACTION_START)?;
        put_string(env, &intent, "title", &config.notification_title)?;
        put_string(env, &intent, "text", &config.notification_text)?;
        put_int(env, &intent, "rate", config.sample_rate as i32)?;
        put_int(env, &intent, "source", config.source.id())?;
        env.call_method(
            activity,
            "startForegroundService",
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
            &[(&intent).into()],
        )?;
        Ok(true)
    })
    .unwrap_or(false);
    if !started {
        clear_sink();
    }
    started
}

/// Stop capturing but keep the foreground notification, the process and the wake lock.
pub fn hold(title: &str, text: &str) -> bool {
    call_static("hold", title, text)
}

/// Replace the notification's text.
pub fn set_text(title: &str, text: &str) -> bool {
    call_static("text", title, text)
}

/// End the service, taking the notification down. Stops a capture still in progress.
pub fn release() {
    with_service_class(|env, class| {
        env.call_static_method(JClass::from(env.new_local_ref(class.as_obj())?), "release", "()V", &[])?;
        Ok(())
    });
}

fn call_static(method: &str, title: &str, text: &str) -> bool {
    with_service_class(|env, class| {
        let (title, text) = (env.new_string(title)?, env.new_string(text)?);
        env.call_static_method(
            JClass::from(env.new_local_ref(class.as_obj())?),
            method,
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[(&title).into(), (&text).into()],
        )?;
        Ok(())
    })
    .is_some()
}

/// Runs `f` against the cached service class on the current thread, attaching it if needed.
fn with_service_class<R>(
    f: impl FnOnce(&mut jni::JNIEnv, &GlobalRef) -> jni::errors::Result<R>,
) -> Option<R> {
    let (vm, class) = SERVICE_CLASS_REF.get()?;
    let mut env = vm.attach_current_thread().ok()?;
    match env.with_local_frame::<_, R, jni::errors::Error>(8, |env| f(env, class)) {
        Ok(value) => Some(value),
        Err(error) => {
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
            log::warn!("egui-android mic: service call failed: {error:?}");
            None
        }
    }
}

fn clear_sink() {
    if let Ok(mut slot) = SINK.lock() {
        *slot = None;
    }
}

// ── JNI ──────────────────────────────────────────────────────────────────────

fn push(event: MicEvent) {
    if let Ok(mut events) = EVENTS.lock() {
        events.push_back(event);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiMicService_nativeMicStarted(
    _env: jni::JNIEnv,
    _class: JClass,
    rate: jni::sys::jint,
) {
    log::info!("egui-android mic: capturing at {rate} Hz");
    RATE.store(rate.max(1) as u32, Ordering::Relaxed);
    CAPTURING.store(true, Ordering::Relaxed);
    push(MicEvent::Started { sample_rate: rate.max(1) as u32 });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiMicService_nativeMicPcm(
    env: jni::JNIEnv,
    _class: JClass,
    samples: JShortArray,
    count: jni::sys::jint,
) {
    // One capture thread at a time, so the scratch buffer is only ever contended by a restart.
    static SCRATCH: Mutex<Vec<i16>> = Mutex::new(Vec::new());
    let Ok(mut scratch) = SCRATCH.lock() else { return };
    scratch.resize(count.max(0) as usize, 0);
    if env.get_short_array_region(&samples, 0, &mut scratch).is_err() {
        return;
    }
    if let Ok(mut sink) = SINK.lock() {
        if let Some(sink) = sink.as_mut() {
            sink(&scratch, RATE.load(Ordering::Relaxed));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiMicService_nativeMicStopped(
    _env: jni::JNIEnv,
    _class: JClass,
) {
    log::info!("egui-android mic: capture ended");
    // Dropping the sink is how its owner learns no more samples are coming.
    clear_sink();
    push(MicEvent::Stopped);
    // Last: once this reads false, a new `start` may install its sink and clear the queue.
    CAPTURING.store(false, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiMicService_nativeMicFailed(
    mut env: jni::JNIEnv,
    _class: JClass,
    reason: JString,
) {
    let reason: String = env
        .get_string(&reason)
        .map(Into::into)
        .unwrap_or_else(|_| String::from("unknown error"));
    log::warn!("egui-android mic: {reason}");
    // A capture thread that fails still ends with `nativeMicStopped`, which is what clears
    // the capturing flag.
    push(MicEvent::Failed(reason));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_github_egui_1mobile_EguiMicService_nativeMicStopRequested(
    _env: jni::JNIEnv,
    _class: JClass,
) {
    log::info!("egui-android mic: stop requested from the notification");
    push(MicEvent::StopRequested);
}

/// Bind the service natives. `NativeActivity` dlopens the native library, so ART's own symbol
/// lookup may never see them however they are exported.
fn register_natives() -> bool {
    if REGISTERED.get().is_some() {
        return true;
    }

    let ok = with_native_activity(|env, activity| {
        let class = service_class(env, activity)?;
        let native = |name: &str, sig: &str, fn_ptr: *mut std::ffi::c_void| jni::NativeMethod {
            name: jni::strings::JNIString::from(name),
            sig: jni::strings::JNIString::from(sig),
            fn_ptr,
        };
        let methods = [
            native("nativeMicStarted", "(I)V", Java_com_github_egui_1mobile_EguiMicService_nativeMicStarted as *mut std::ffi::c_void),
            native("nativeMicPcm", "([SI)V", Java_com_github_egui_1mobile_EguiMicService_nativeMicPcm as *mut std::ffi::c_void),
            native("nativeMicStopped", "()V", Java_com_github_egui_1mobile_EguiMicService_nativeMicStopped as *mut std::ffi::c_void),
            native("nativeMicFailed", "(Ljava/lang/String;)V", Java_com_github_egui_1mobile_EguiMicService_nativeMicFailed as *mut std::ffi::c_void),
            native("nativeMicStopRequested", "()V", Java_com_github_egui_1mobile_EguiMicService_nativeMicStopRequested as *mut std::ffi::c_void),
        ];
        env.register_native_methods(&class, &methods)?;
        if SERVICE_CLASS_REF.get().is_none() {
            let _ = SERVICE_CLASS_REF.set((env.get_java_vm()?, env.new_global_ref(&class)?));
        }
        Ok(true)
    })
    .unwrap_or(false);

    // A transient Activity/ClassLoader failure must be retryable.
    if ok { let _ = REGISTERED.set(true); }
    log::info!("egui-android mic: register_natives ok={ok}");
    ok
}

fn service_class<'l>(
    env: &mut jni::JNIEnv<'l>,
    activity: &JObject,
) -> jni::errors::Result<JClass<'l>> {
    let loader = env
        .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let name = env.new_string(SERVICE_CLASS)?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[(&name).into()],
        )?
        .l()?;
    Ok(class.into())
}

fn service_intent<'l>(
    env: &mut jni::JNIEnv<'l>,
    activity: &JObject,
    action: &str,
) -> jni::errors::Result<JObject<'l>> {
    let class = service_class(env, activity)?;
    let intent = env.new_object(
        "android/content/Intent",
        "(Landroid/content/Context;Ljava/lang/Class;)V",
        &[JValue::Object(activity), (&class).into()],
    )?;
    let jaction = env.new_string(action)?;
    env.call_method(
        &intent,
        "setAction",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[(&jaction).into()],
    )?;
    Ok(intent)
}

fn put_string(env: &mut jni::JNIEnv, intent: &JObject, key: &str, value: &str) -> jni::errors::Result<()> {
    let jkey = env.new_string(key)?;
    let jvalue = env.new_string(value)?;
    env.call_method(
        intent,
        "putExtra",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[(&jkey).into(), (&jvalue).into()],
    )?;
    Ok(())
}

fn put_int(env: &mut jni::JNIEnv, intent: &JObject, key: &str, value: i32) -> jni::errors::Result<()> {
    let jkey = env.new_string(key)?;
    env.call_method(
        intent,
        "putExtra",
        "(Ljava/lang/String;I)Landroid/content/Intent;",
        &[(&jkey).into(), JValue::Int(value)],
    )?;
    Ok(())
}
