//! The QNN libraries, loaded once per process.
//!
//! Unloading them is not worth doing: a `dlclose` of the HTP backend spews vendor warnings and
//! the next load pays for it again. What is released when the model goes idle is the session
//! and its contexts — the gigabytes — not these two handles.

use local_whisper::{prepare_htp_env, Backend, QnnSystem};
use std::path::Path;
use std::sync::OnceLock;

pub struct Libs {
    pub system: QnnSystem,
    pub backend: Backend,
}

static LIBS: OnceLock<Result<Libs, String>> = OnceLock::new();

/// `libQnnSystem.so` and `libQnnHtp.so` from the APK's native library directory, which is also
/// where FastRPC has to find the Hexagon skel.
pub fn libs(dir: &Path) -> Result<&'static Libs, String> {
    LIBS.get_or_init(|| {
        prepare_htp_env(dir);
        let system = QnnSystem::load(dir.join("libQnnSystem.so")).map_err(|e| format!("QNN system library: {e}"))?;
        let backend = Backend::load(dir.join("libQnnHtp.so")).map_err(|e| format!("QNN HTP backend: {e}"))?;
        log::info!("qnn: {} api {:?}, backend {} api {:?}", system.provider_name(), system.api_version(), backend.provider_name(), backend.api_version());
        Ok(Libs { system, backend })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The libraries' own description, once they are loaded.
pub fn describe() -> Option<String> {
    let libs = LIBS.get()?.as_ref().ok()?;
    let (s, b) = (libs.system.api_version(), libs.backend.api_version());
    Some(format!("{} (API {}.{}.{}), system API {}.{}.{}", libs.backend.provider_name(), b.0, b.1, b.2, s.0, s.1, s.2))
}
