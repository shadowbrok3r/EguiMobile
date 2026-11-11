//! Everything that touches Android, behind functions that also exist on the host — where they
//! do nothing — so the rest of the app builds and tests there.

use crate::settings::MicPath;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub enum MicEvent {
    Started(u32),
    Stopped,
    Failed(String),
    StopRequested,
}

/// Samples and the rate they were captured at.
pub type MicSink = Box<dyn FnMut(&[i16], u32) + Send>;

#[cfg(target_os = "android")]
mod imp {
    use super::*;
    use egui_mobile::mic;

    pub fn mic_permission() -> bool {
        mic::permission_granted()
    }

    pub fn mic_start(title: &str, text: &str, path: MicPath, mut sink: MicSink) -> bool {
        let config = mic::MicConfig {
            notification_title: title.into(),
            notification_text: text.into(),
            sample_rate: 16_000,
            source: match path {
                MicPath::Speech => mic::MicSource::VoiceRecognition,
                MicPath::Raw => mic::MicSource::Unprocessed,
                MicPath::Standard => mic::MicSource::Default,
            },
        };
        mic::start(&config, move |pcm, rate| sink(pcm, rate))
    }

    /// Whether an earlier capture's thread is still alive.
    pub fn mic_capturing() -> bool {
        mic::is_capturing()
    }

    pub fn mic_hold(title: &str, text: &str) {
        mic::hold(title, text);
    }

    pub fn mic_release() {
        mic::release();
    }

    pub fn mic_event() -> Option<MicEvent> {
        Some(match mic::poll_event()? {
            mic::MicEvent::Started { sample_rate } => MicEvent::Started(sample_rate),
            mic::MicEvent::Stopped => MicEvent::Stopped,
            mic::MicEvent::Failed(reason) => MicEvent::Failed(reason),
            mic::MicEvent::StopRequested => MicEvent::StopRequested,
        })
    }

    /// FastRPC to the compute DSP is what every HTP call rides on. A device where its vendor
    /// library does not load has no NPU to reach, and loading the QNN backend there only fails
    /// later and louder — under an emulator's binary translation, not always cleanly.
    pub fn has_npu() -> bool {
        local_whisper::fastrpc_available()
    }

    pub fn native_lib_dir(host: &egui_mobile::Host) -> Option<PathBuf> {
        use egui_mobile::HostExt;
        host.native_lib_dir().map(PathBuf::from)
    }

    pub fn request_notifications(host: &egui_mobile::Host) {
        use egui_mobile::HostExt;
        host.request_notification_permission();
    }

    pub fn open_app_settings(host: &egui_mobile::Host) {
        use egui_mobile::HostExt;
        host.open_app_settings();
    }

    /// Hand a recording to the share sheet as audio.
    pub fn share_audio(host: &egui_mobile::Host, path: &std::path::Path, name: &str) {
        use egui_mobile::HostExt;
        host.share_media(path.to_string_lossy().into_owned(), name, "audio/wav");
    }

    /// Why the last share did not open, if it did not.
    pub fn share_error(host: &egui_mobile::Host) -> Option<String> {
        use egui_mobile::HostExt;
        host.take_share_outcome()?.err()
    }

    /// Hand a text file to the share sheet by way of a copy in the phone's Downloads, which
    /// is how the framework shares a file that Android has no address for.
    pub fn share_copy(host: &egui_mobile::Host, path: &std::path::Path, name: &str) {
        use egui_mobile::HostExt;
        host.share_media(path.to_string_lossy().into_owned(), name, "text/plain");
    }

    /// Hand a file in the phone's Documents to the share sheet as itself.
    ///
    /// Another app cannot be given a path. It is given the address Android's media index
    /// has for the file, with leave to read it; the file is one this app wrote there, so the
    /// index lets it look that address up and pass it on. Nothing is copied.
    pub fn share_stored(folder: &str, name: &str) -> Result<(), String> {
        use jni::objects::{JObject, JValue};
        let shared = egui_mobile::with_native_activity(|env, activity| {
            let resolver = env.call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?.l()?;
            let volume = env.new_string("external")?;
            let files = env.call_static_method("android/provider/MediaStore$Files", "getContentUri", "(Ljava/lang/String;)Landroid/net/Uri;", &[(&volume).into()])?.l()?;

            // Found by folder and name, pending rows included: a file written a moment ago
            // is listed as pending until its scan lands.
            let query = env.new_object("android/os/Bundle", "()V", &[])?;
            let (key, selection) = (env.new_string("android:query-arg-sql-selection")?, env.new_string("relative_path=? AND _display_name=?")?);
            env.call_method(&query, "putString", "(Ljava/lang/String;Ljava/lang/String;)V", &[(&key).into(), (&selection).into()])?;
            let (within, called) = (env.new_string(format!("{folder}/"))?, env.new_string(name)?);
            let wanted = env.new_object_array(2, "java/lang/String", &within)?;
            env.set_object_array_element(&wanted, 1, &called)?;
            let key = env.new_string("android:query-arg-sql-selection-args")?;
            env.call_method(&query, "putStringArray", "(Ljava/lang/String;[Ljava/lang/String;)V", &[(&key).into(), (&wanted).into()])?;
            let key = env.new_string("android:query-arg-match-pending")?;
            env.call_method(&query, "putInt", "(Ljava/lang/String;I)V", &[(&key).into(), JValue::Int(1)])?;
            let id = env.new_string("_id")?;
            let columns = env.new_object_array(1, "java/lang/String", &id)?;
            let none = JObject::null();
            let cursor = env
                .call_method(&resolver, "query", "(Landroid/net/Uri;[Ljava/lang/String;Landroid/os/Bundle;Landroid/os/CancellationSignal;)Landroid/database/Cursor;", &[(&files).into(), (&columns).into(), (&query).into(), (&none).into()])?
                .l()?;
            if cursor.is_null() {
                return Ok(Err("Android's file index did not answer.".to_string()));
            }
            let row = if env.call_method(&cursor, "moveToFirst", "()Z", &[])?.z()? { Some(env.call_method(&cursor, "getLong", "(I)J", &[JValue::Int(0)])?.j()?) } else { None };
            env.call_method(&cursor, "close", "()V", &[])?;
            let Some(row) = row else { return Ok(Err("Android has not indexed the file.".to_string())) };
            let uri = env.call_static_method("android/content/ContentUris", "withAppendedId", "(Landroid/net/Uri;J)Landroid/net/Uri;", &[(&files).into(), JValue::Long(row)])?.l()?;

            // Listed as finished now, not when the scan gets round to it: a pending file is
            // hidden from the very app it is being handed to.
            let finished = env.new_object("android/content/ContentValues", "()V", &[])?;
            let (key, zero) = (env.new_string("is_pending")?, env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(0)])?);
            env.call_method(&finished, "put", "(Ljava/lang/String;Ljava/lang/Integer;)V", &[(&key).into(), (&zero).into()])?;
            let listed = env.call_method(&resolver, "update", "(Landroid/net/Uri;Landroid/content/ContentValues;Ljava/lang/String;[Ljava/lang/String;)I", &[(&uri).into(), (&finished).into(), (&none).into(), (&none).into()]);
            if listed.is_err() {
                // The scan will do it; the share goes ahead.
                let _ = env.exception_clear();
            }

            let action = env.new_string("android.intent.action.SEND")?;
            let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;)V", &[(&action).into()])?;
            let mime = env.new_string("text/plain")?;
            env.call_method(&intent, "setType", "(Ljava/lang/String;)Landroid/content/Intent;", &[(&mime).into()])?;
            let key = env.new_string("android.intent.extra.STREAM")?;
            env.call_method(&intent, "putExtra", "(Ljava/lang/String;Landroid/os/Parcelable;)Landroid/content/Intent;", &[(&key).into(), (&uri).into()])?;
            // The clip is what carries the leave to read through the sheet to whoever is
            // picked, and what the sheet shows as the file's name.
            let item = env.new_object("android/content/ClipData$Item", "(Landroid/net/Uri;)V", &[(&uri).into()])?;
            let kinds = env.new_object_array(1, "java/lang/String", &mime)?;
            let clip = env.new_object("android/content/ClipData", "(Ljava/lang/CharSequence;[Ljava/lang/String;Landroid/content/ClipData$Item;)V", &[(&called).into(), (&kinds).into(), (&item).into()])?;
            env.call_method(&intent, "setClipData", "(Landroid/content/ClipData;)V", &[(&clip).into()])?;
            // FLAG_GRANT_READ_URI_PERMISSION and FLAG_ACTIVITY_NEW_TASK.
            env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x1000_0001)])?;
            let chooser = env.call_static_method("android/content/Intent", "createChooser", "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;", &[(&intent).into(), (&called).into()])?.l()?;
            env.call_method(&chooser, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x1000_0000)])?;
            env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[(&chooser).into()])?;
            Ok(Ok(()))
        });
        shared.unwrap_or_else(|| Err("The share sheet could not be opened.".into()))
    }

    /// Have Android index a file or folder this app wrote straight to shared storage. A file
    /// made through the filesystem is listed as pending — on disk, but missing from every
    /// other app's document picker — until a scan finds it complete.
    pub fn index_shared(path: &std::path::Path) {
        use jni::objects::{JObject, JValue};
        let path = path.to_string_lossy().into_owned();
        let asked = egui_mobile::with_native_activity(|env, activity| {
            let file = env.new_string(&path)?;
            let files = env.new_object_array(1, "java/lang/String", &file)?;
            let none = JObject::null();
            env.call_static_method(
                "android/media/MediaScannerConnection",
                "scanFile",
                "(Landroid/content/Context;[Ljava/lang/String;[Ljava/lang/String;Landroid/media/MediaScannerConnection$OnScanCompletedListener;)V",
                &[JValue::Object(activity), (&files).into(), (&none).into(), (&none).into()],
            )?;
            Ok(())
        });
        if asked.is_none() {
            log::warn!("{path} was not handed to the media scanner");
        }
    }

    static APP: std::sync::OnceLock<android_activity::AndroidApp> = std::sync::OnceLock::new();

    /// Keep the handle the APK's assets are read through.
    pub fn set_app(app: &android_activity::AndroidApp) {
        let _ = APP.set(app.clone());
    }

    /// The speaker network, unpacked from the APK into `private/speaker` the first time each
    /// version runs. It ships inside the APK because, unlike the Whisper packs, nobody hosts
    /// it: it is compiled for this chip by `scripts/speaker-export`. `None` if this build was
    /// made without it.
    pub fn speaker_pack(private: &std::path::Path, install: Option<&std::path::Path>) -> Option<PathBuf> {
        use local_speaker::{SpeakerPack, MARKER, MODEL};
        let mut asset = APP.get()?.asset_manager().open(c"speaker/model.bin")?;
        let dir = private.join("speaker");
        // The marker carries the version and the install that unpacked it — Android gives
        // every install its own directory — so an update, or a rebuild of the same version
        // around another network, brings its own copy.
        let stamp = format!("{} {} {}", env!("CARGO_PKG_VERSION"), asset.length(), install.map_or_else(String::new, |dir| dir.display().to_string()));
        let current = std::fs::read_to_string(dir.join(MARKER)).is_ok_and(|have| have == stamp);
        if current && SpeakerPack::open(&dir).is_ok_and(|pack| pack.size_bytes() == asset.length() as u64) {
            return Some(dir);
        }
        let unpacked = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let _ = std::fs::remove_file(dir.join(MARKER));
            let part = dir.join("model.part");
            let mut out = std::fs::File::create(&part)?;
            std::io::copy(&mut asset, &mut out)?;
            out.sync_all()?;
            std::fs::rename(&part, dir.join(MODEL))?;
            std::fs::write(dir.join(MARKER), &stamp)
        })();
        match unpacked {
            Ok(()) => Some(dir),
            Err(e) => {
                log::warn!("speaker model not unpacked: {e}");
                None
            }
        }
    }

    pub fn store_available(host: &egui_mobile::Host) -> bool {
        use egui_mobile::HostExt;
        host.store_available()
    }

    pub fn check_store_update(host: &egui_mobile::Host) {
        use egui_mobile::HostExt;
        host.check_store_update();
    }

    pub fn open_app_store(host: &egui_mobile::Host) {
        use egui_mobile::HostExt;
        host.open_app_store();
    }

    /// Playback of one note's audio through Android's MediaPlayer.
    #[derive(Default)]
    pub struct Player {
        audio: Option<(String, egui_mobile::video::Audio)>,
    }

    impl Player {
        /// The note whose audio is open.
        pub fn note(&self) -> Option<&str> {
            self.audio.as_ref().map(|(id, _)| id.as_str())
        }

        pub fn open(&mut self, id: &str, path: &std::path::Path) -> bool {
            self.close();
            let Some(mut env) = egui_mobile::video::attach_env() else { return false };
            match egui_mobile::video::Audio::open(&mut env, &path.to_string_lossy()) {
                Some(audio) => {
                    self.audio = Some((id.to_string(), audio));
                    true
                }
                None => false,
            }
        }

        pub fn close(&mut self) {
            if let (Some((_, audio)), Some(mut env)) = (self.audio.take(), egui_mobile::video::attach_env()) {
                audio.release(&mut env);
            }
        }

        pub fn set_playing(&mut self, playing: bool) {
            if let (Some((_, audio)), Some(mut env)) = (self.audio.as_ref(), egui_mobile::video::attach_env()) {
                if playing { audio.play(&mut env) } else { audio.pause(&mut env) }
            }
        }

        pub fn is_playing(&self) -> bool {
            match (self.audio.as_ref(), egui_mobile::video::attach_env()) {
                (Some((_, audio)), Some(mut env)) => audio.is_playing(&mut env),
                _ => false,
            }
        }

        pub fn position(&self) -> f64 {
            match (self.audio.as_ref(), egui_mobile::video::attach_env()) {
                (Some((_, audio)), Some(mut env)) => audio.position_ms(&mut env) as f64 / 1000.0,
                _ => 0.0,
            }
        }

        pub fn seek(&mut self, seconds: f64) {
            if let (Some((_, audio)), Some(mut env)) = (self.audio.as_ref(), egui_mobile::video::attach_env()) {
                audio.seek(&mut env, (seconds.max(0.0) * 1000.0) as i64);
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
mod imp {
    use super::*;

    pub fn mic_permission() -> bool {
        false
    }

    pub fn mic_start(_title: &str, _text: &str, _path: MicPath, _sink: MicSink) -> bool {
        false
    }

    pub fn mic_capturing() -> bool {
        false
    }

    pub fn mic_hold(_title: &str, _text: &str) {}

    pub fn mic_release() {}

    pub fn mic_event() -> Option<MicEvent> {
        None
    }

    pub fn has_npu() -> bool {
        false
    }

    pub fn native_lib_dir(_host: &egui_mobile::Host) -> Option<PathBuf> {
        None
    }

    pub fn request_notifications(_host: &egui_mobile::Host) {}

    pub fn open_app_settings(_host: &egui_mobile::Host) {}

    pub fn share_audio(_host: &egui_mobile::Host, _path: &std::path::Path, _name: &str) {}

    pub fn share_error(_host: &egui_mobile::Host) -> Option<String> {
        None
    }

    pub fn share_copy(_host: &egui_mobile::Host, _path: &std::path::Path, _name: &str) {}

    pub fn share_stored(_folder: &str, _name: &str) -> Result<(), String> {
        Ok(())
    }

    pub fn index_shared(_path: &std::path::Path) {}

    /// There is no APK to unpack a speaker network from; the scripted one stands in.
    pub fn speaker_pack(_private: &std::path::Path, _install: Option<&std::path::Path>) -> Option<PathBuf> {
        None
    }

    pub fn store_available(_host: &egui_mobile::Host) -> bool {
        false
    }

    pub fn check_store_update(_host: &egui_mobile::Host) {}

    pub fn open_app_store(_host: &egui_mobile::Host) {}

    /// The host has no audio output wired up; the screens still lay out around a player.
    #[derive(Default)]
    pub struct Player {
        note: Option<String>,
        playing: bool,
        position: f64,
    }

    impl Player {
        pub fn note(&self) -> Option<&str> {
            self.note.as_deref()
        }

        pub fn open(&mut self, id: &str, _path: &std::path::Path) -> bool {
            *self = Self { note: Some(id.to_string()), playing: false, position: 0.0 };
            true
        }

        pub fn close(&mut self) {
            *self = Self::default();
        }

        pub fn set_playing(&mut self, playing: bool) {
            self.playing = playing && self.note.is_some();
        }

        pub fn is_playing(&self) -> bool {
            self.playing
        }

        pub fn position(&self) -> f64 {
            self.position
        }

        pub fn seek(&mut self, seconds: f64) {
            self.position = seconds.max(0.0);
        }
    }
}

pub use imp::*;

/// The app's own directory on shared storage: no permission needed, reachable over USB and
/// `adb push`, and large enough for model packs. Removed with the app.
///
/// Asked of Android where possible, because asking is also what creates it; the derived path
/// is the fallback, and the answer everywhere else.
pub fn shared_files_dir(documents: &str) -> Option<PathBuf> {
    #[cfg(target_os = "android")]
    {
        let asked = egui_mobile::with_native_activity(|env, activity| {
            let dir = env.call_method(activity, "getExternalFilesDir", "(Ljava/lang/String;)Ljava/io/File;", &[jni::objects::JValue::Object(&jni::objects::JObject::null())])?.l()?;
            if dir.is_null() {
                return Ok(None);
            }
            let path = env.call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?;
            let path: String = env.get_string(&jni::objects::JString::from(path))?.into();
            Ok(Some(PathBuf::from(path)))
        })
        .flatten();
        if asked.is_some() {
            return asked;
        }
    }
    external_files_dir(documents)
}

/// Where Android puts an app's shared-storage directory, worked out from its private one.
pub fn external_files_dir(documents: &str) -> Option<PathBuf> {
    // documents is /data/user/0/<package>/files.
    let package = std::path::Path::new(documents).parent()?.file_name()?.to_str()?.to_string();
    Some(PathBuf::from(format!("/storage/emulated/0/Android/data/{package}/files")))
}

/// The folder for text files in the phone's own Documents, on the volume the app's shared
/// directory is on. From Android 11 an app may create and rewrite its own files there with
/// no permission; `None` when the app is not on shared storage at all.
pub fn public_documents(shared_files: &std::path::Path) -> Option<PathBuf> {
    // shared_files is <volume>/Android/data/<package>/files.
    let path = shared_files.to_str()?;
    let volume = &path[..path.find("/Android/data/")?];
    Some(PathBuf::from(volume).join("Documents").join("Scribe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_on_the_volume_the_app_is_on() {
        let shared = std::path::Path::new("/storage/emulated/10/Android/data/com.kingsofalchemy.scribe/files");
        assert_eq!(public_documents(shared).unwrap(), PathBuf::from("/storage/emulated/10/Documents/Scribe"));
        assert!(public_documents(std::path::Path::new("/data/user/0/com.kingsofalchemy.scribe/files")).is_none());
    }

    #[test]
    fn the_external_dir_is_derived_from_the_private_one() {
        assert_eq!(
            external_files_dir("/data/user/0/com.kingsofalchemy.scribe/files").unwrap(),
            PathBuf::from("/storage/emulated/0/Android/data/com.kingsofalchemy.scribe/files")
        );
        assert!(external_files_dir("/").is_none());
    }
}
