//! Names the Android app behind a captured flow.
//!
//! `ConnectivityManager.getConnectionOwnerUid` answers only the app holding the active VPN, on
//! Android 10 and later; everywhere else flows simply carry no app.

use crate::proxy::state::App;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LOOKUP_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct AppResolver {
    known: Mutex<HashMap<u32, Arc<App>>>,
}

impl AppResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// The app that owns the flow from `local` to `remote`.
    pub async fn resolve(
        self: Arc<Self>,
        udp: bool,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> Option<Arc<App>> {
        if !super::platform::SUPPORTED {
            return None;
        }
        // A binder call per flow; kept off the relay's two async workers, and never waited on long.
        let lookup = tokio::task::spawn_blocking(move || {
            let uid = super::platform::connection_owner(udp, local, remote)?;
            Some(self.describe(uid))
        });
        tokio::time::timeout(LOOKUP_TIMEOUT, lookup)
            .await
            .ok()?
            .ok()
            .flatten()
    }

    fn describe(&self, uid: u32) -> Arc<App> {
        if let Some(app) = self.known.lock().ok().and_then(|known| known.get(&uid).cloned()) {
            return app;
        }
        let (package, label) = match super::platform::describe_uid(uid) {
            Some(owner) => (owner.package, owner.label),
            None => (None, None),
        };
        let app = Arc::new(App {
            uid,
            package,
            label,
        });
        if let Ok(mut known) = self.known.lock() {
            known.insert(uid, app.clone());
        }
        app
    }
}
