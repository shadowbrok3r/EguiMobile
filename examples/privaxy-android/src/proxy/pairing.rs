//! Versioned, certificate-pinned desktop relay. Credentials stay in private app storage.
use super::desktop_ca::{self, CaTransfer, STATUS_CA_PROTOCOL, STATUS_PROTOCOL};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Longest desktop name kept from a pairing code.
const MAX_NAME: usize = 64;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopPairing {
    pub version: u8,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub certificate_pem: String,
    /// The computer's own name, sent by desktops that know it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Identifies the computer across revoked pairings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_id: Option<String>,
}
impl DesktopPairing {
    // Address changes preserve the saved CA; a different pinned identity or token does not.
    pub fn identity_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(self.token.as_bytes());
        digest.update(self.certificate_pem.as_bytes());
        format!("{:x}", digest.finalize())
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        use base64::Engine as _;
        let input = text.trim();
        let decoded;
        if input.len() > 16384 {
            return Err("Pairing code is too long".into());
        }
        let json = if let Some(encoded) = input
            .strip_prefix("privaxy://pair?code=")
            .or_else(|| input.strip_prefix("privaxy-pair-v1:"))
        {
            decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| "Invalid pairing code")?;
            std::str::from_utf8(&decoded).map_err(|_| "Invalid pairing code")?
        } else {
            input
        };
        let mut pair: Self = serde_json::from_str(json).map_err(|_| "Invalid pairing code")?;
        pair.name = pair.name.as_deref().and_then(clean_name);
        pair.desktop_id = pair.desktop_id.filter(|id| {
            (1..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        pair.validate()?;
        Ok(pair)
    }
    /// The desktop's own name, or where it is reached.
    pub fn default_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("Desktop at {}", self.host))
    }
    pub fn validate(&self) -> Result<(), String> {
        self.validate_address()?;
        if self.version != 1
            || self.token.len() != 64
            || !self.token.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Unsupported or malformed pairing code".into());
        }
        self.tls_config()?;
        Ok(())
    }
    fn validate_address(&self) -> Result<(), String> {
        let host = self.host.as_str();
        if self.port == 0
            || host.is_empty()
            || host.len() > 253
            || host.chars().any(char::is_whitespace)
            || host.contains(['/', '\\', '@', '?', '#', '[', ']'])
            || (host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err())
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_unspecified() || ip.is_multicast())
        {
            return Err("Enter an IP address or hostname and a port from 1 to 65535. Use the desktop's address, not 0.0.0.0.".into());
        }
        Ok(())
    }
    pub fn with_address(&self, host: &str, port: &str) -> Result<Self, String> {
        let mut pair = self.clone();
        pair.host = host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        pair.port = port
            .trim()
            .parse()
            .map_err(|_| "Enter a port from 1 to 65535")?;
        pair.validate()?;
        Ok(pair)
    }
    pub fn is_tailscale(&self) -> bool {
        self.host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| {
            let octets = ip.octets();
            octets[0] == 100 && (64..=127).contains(&octets[1])
        }) || self.host.ends_with(".ts.net")
    }
    fn tls_config(&self) -> Result<rustls::ClientConfig, String> {
        let der =
            super::ca::pem_to_der(&self.certificate_pem).ok_or("Invalid desktop certificate")?;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der.into()).map_err(|e| e.to_string())?;
        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth())
    }
    pub async fn relay(&self, mut local: tokio::net::TcpStream) -> Result<(), String> {
        let mut remote = self.connect(false).await?;
        let (up, down) = tokio::io::copy_bidirectional(&mut local, &mut remote)
            .await
            .map_err(|e| {
                log::warn!("Desktop traffic relay failed: {e}");
                e.to_string()
            })?;
        log::debug!("Desktop traffic relay closed uploaded={up} downloaded={down}");
        Ok(())
    }
    async fn connect(
        &self,
        status: bool,
    ) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, String> {
        self.validate()?;
        let started = std::time::Instant::now();
        let stage = std::sync::atomic::AtomicUsize::new(0);
        log::debug!(
            "Desktop connect endpoint={}:{} status_channel={status}",
            self.host,
            self.port
        );
        let connect = async {
            let tcp = tokio::net::TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|e| self.connection_error(&e))?;
            log::debug!(
                "Desktop TCP connected local={:?} peer={:?} elapsed_ms={}",
                tcp.local_addr().ok(),
                tcp.peer_addr().ok(),
                started.elapsed().as_millis()
            );
            stage.store(1, std::sync::atomic::Ordering::Relaxed);
            let name = rustls_pki_types::ServerName::try_from("privaxy-desktop").unwrap();
            let mut config = self.tls_config()?;
            if status {
                config.alpn_protocols = vec![STATUS_CA_PROTOCOL.to_vec(), STATUS_PROTOCOL.to_vec()];
            }
            let mut remote = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("Desktop TLS handshake: {e}"))?;
            log::debug!(
                "Desktop TLS verified elapsed_ms={}",
                started.elapsed().as_millis()
            );
            stage.store(2, std::sync::atomic::Ordering::Relaxed);
            if status
                && !matches!(remote.get_ref().1.alpn_protocol(), Some(protocol) if protocol == STATUS_PROTOCOL || protocol == STATUS_CA_PROTOCOL)
            {
                return Err("Update Privaxy desktop to enable connection status".into());
            }
            remote
                .write_all(self.token.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let mut ack = [0; 2];
            remote
                .read_exact(&mut ack)
                .await
                .map_err(|e| e.to_string())?;
            if ack != *b"OK" {
                return Err("Desktop rejected pairing; pair again".into());
            }
            log::debug!(
                "Desktop pairing authenticated elapsed_ms={}",
                started.elapsed().as_millis()
            );
            Ok::<_, String>(remote)
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(if status { 5 } else { 15 }),
            connect,
        )
        .await
        .map_err(|_| format!("Desktop {}:{} timed out during {}. Check that the relay is running and reachable. Your pairing is still saved.", self.host, self.port, ["TCP connection", "TLS handshake", "Pairing authentication"][stage.load(std::sync::atomic::Ordering::Relaxed)]))
        .and_then(|result| result);
        if let Err(error) = &result {
            log::warn!(
                "Desktop connection failed endpoint={}:{} stage={} elapsed_ms={}: {error}",
                self.host,
                self.port,
                ["TCP connection", "TLS handshake", "Pairing authentication"]
                    [stage.load(std::sync::atomic::Ordering::Relaxed)],
                started.elapsed().as_millis()
            );
        }
        result
    }

    fn connection_error(&self, error: &std::io::Error) -> String {
        let help = match error.kind() {
            std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable => {
                "The network cannot reach this desktop. Check Wi-Fi client/guest isolation and the desktop firewall. If the desktop uses a Tailscale exit node, enable Allow LAN access there."
            }
            std::io::ErrorKind::ConnectionRefused => {
                "The connection was refused. Start Android relay on the desktop and check the relay port."
            }
            _ => "Check the desktop address, relay, and network connection.",
        };
        format!(
            "Cannot connect to {}:{}: {error}. {help} Your pairing is still saved.",
            self.host, self.port
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConnectionStatus {
    #[default]
    Unpaired,
    Checking,
    Connected,
    Offline(String),
}
impl ConnectionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unpaired => "Desktop: not paired",
            Self::Checking => "Desktop: checking",
            Self::Connected => "Desktop: connected · paired",
            Self::Offline(_) => "Desktop: unreachable · pairing saved",
        }
    }
}

/// The desktop relayed traffic goes to. The monitor moves it when it switches desktops.
pub type DesktopRoute = Arc<std::sync::RwLock<Option<DesktopPairing>>>;

#[derive(Default)]
pub struct ConnectionMonitor {
    /// The desktops to try, preferred first.
    desktops: Vec<DesktopPairing>,
    failover: bool,
    runtime: Option<tokio::runtime::Runtime>,
    status: Arc<std::sync::Mutex<ConnectionStatus>>,
    ca: Arc<std::sync::Mutex<CaTransfer>>,
    refresh_ca: Arc<std::sync::atomic::AtomicBool>,
    route: DesktopRoute,
    /// Identity of a desktop the monitor moved to because the preferred one was unreachable.
    switched: Arc<std::sync::Mutex<Option<String>>>,
}
impl ConnectionMonitor {
    pub fn status(&self) -> ConnectionStatus {
        self.status.lock().unwrap().clone()
    }
    pub fn ca_transfer(&self) -> CaTransfer {
        self.ca.lock().unwrap().clone()
    }
    pub fn refresh_ca(&self) {
        self.refresh_ca
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    /// The route the proxy relays through; it lives as long as the monitor.
    pub fn route(&self) -> DesktopRoute {
        self.route.clone()
    }
    /// The desktop in use: the one connected, or the preferred one while none is.
    pub fn current(&self) -> Option<DesktopPairing> {
        self.route.read().unwrap().clone()
    }
    /// The identity of a desktop the monitor switched to since the last call.
    pub fn take_switch(&self) -> Option<String> {
        self.switched.lock().unwrap().take()
    }
    pub fn retry(&mut self) {
        log::info!("Desktop connection retry requested");
        let (desktops, failover) = (std::mem::take(&mut self.desktops), self.failover);
        self.sync(&desktops, failover);
    }
    /// Watches `desktops`, the preferred one first, moving to the next reachable one when
    /// `failover` is set.
    pub fn sync(&mut self, desktops: &[DesktopPairing], failover: bool) {
        let same_desktops = self.desktops.len() == desktops.len()
            && desktops.iter().all(|pair| self.desktops.contains(pair));
        // A switch the app has since recorded puts the connected desktop first: keep the connection.
        if same_desktops && self.failover == failover && desktops.first() == self.current().as_ref()
        {
            if self.desktops != desktops {
                self.desktops = desktops.to_vec();
            }
            return;
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
        log::info!(
            "Desktop monitor configuration changed desktops={} failover={failover}",
            desktops.len()
        );
        self.desktops = desktops.to_vec();
        self.failover = failover;
        *self.route.write().unwrap() = desktops.first().cloned();
        // Use fresh cells so a cancelled old connection cannot overwrite the new one.
        self.status = Arc::default();
        self.ca = Arc::default();
        self.refresh_ca = Arc::default();
        self.switched = Arc::default();
        if desktops.is_empty() {
            return;
        }
        *self.status.lock().unwrap() = ConnectionStatus::Checking;
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("privaxy-pairing")
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                *self.status.lock().unwrap() = ConnectionStatus::Offline(e.to_string());
                return;
            }
        };
        let status = self.status.clone();
        let ca = self.ca.clone();
        let refresh_ca = self.refresh_ca.clone();
        let route = self.route.clone();
        let switched = self.switched.clone();
        let desktops = self.desktops.clone();
        runtime.spawn(async move {
            let mut index = 0;
            loop {
                let pair = desktops[index].clone();
                let result = async {
                    let mut stream = pair.connect(true).await?;
                    if route.read().unwrap().as_ref() != Some(&pair) {
                        log::info!(
                            "Desktop switched to {} at {}:{}",
                            pair.default_name(),
                            pair.host,
                            pair.port
                        );
                        *route.write().unwrap() = Some(pair.clone());
                        *ca.lock().unwrap() = CaTransfer::Waiting;
                        *switched.lock().unwrap() = Some(pair.identity_key());
                    }
                    let supports_ca =
                        stream.get_ref().1.alpn_protocol() == Some(STATUS_CA_PROTOCOL);
                    if !supports_ca {
                        *ca.lock().unwrap() = CaTransfer::Unsupported;
                    }
                    let mut ca_checked: Option<std::time::Instant> = None;
                    let connected_at = std::time::Instant::now();
                    log::info!(
                        "Desktop connected and pairing verified endpoint={}:{}",
                        pair.host,
                        pair.port
                    );
                    loop {
                        let ping_at = std::time::Instant::now();
                        let ping = async {
                            stream.write_all(b"P").await?;
                            stream.flush().await?;
                            if stream.read_u8().await? != b'P' {
                                return Err(std::io::Error::other("Invalid desktop heartbeat"));
                            }
                            Ok::<_, std::io::Error>(())
                        };
                        tokio::time::timeout(std::time::Duration::from_secs(5), ping)
                            .await
                            .map_err(|_| "Desktop stopped responding".to_owned())?
                            .map_err(|e| e.to_string())?;
                        log::trace!(
                            "Desktop heartbeat endpoint={}:{} rtt_ms={} connected_secs={}",
                            pair.host,
                            pair.port,
                            ping_at.elapsed().as_millis(),
                            connected_at.elapsed().as_secs()
                        );
                        *status.lock().unwrap() = ConnectionStatus::Connected;
                        if supports_ca
                            && (refresh_ca.swap(false, std::sync::atomic::Ordering::Relaxed)
                                || ca_checked.is_none_or(|at| {
                                    at.elapsed() >= std::time::Duration::from_secs(60)
                                }))
                        {
                            match desktop_ca::receive(&mut stream, &pair).await {
                                Ok(received) => {
                                    let mut current = ca.lock().unwrap();
                                    if *current != CaTransfer::Received(received.clone()) {
                                        log::info!(
                                            "Desktop inspection CA received sha256={}",
                                            received.fingerprint()
                                        );
                                        *current = CaTransfer::Received(received);
                                    }
                                    ca_checked = Some(std::time::Instant::now());
                                }
                                Err(error) => {
                                    log::warn!("Desktop inspection CA transfer failed: {error}");
                                    *ca.lock().unwrap() = CaTransfer::Failed(error.clone());
                                    // Reconnect rather than reusing a partially read protocol frame.
                                    return Err(format!("Desktop CA transfer: {error}"));
                                }
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), String>(())
                }
                .await;
                let error = result.unwrap_err();
                *status.lock().unwrap() = ConnectionStatus::Offline(error.clone());
                index = if failover {
                    (index + 1) % desktops.len()
                } else {
                    index
                };
                if index == 0 || !failover {
                    log::warn!("Desktop monitor offline: {error}; retry in 3s");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                } else {
                    log::warn!(
                        "Desktop monitor offline: {error}; trying {}",
                        desktops[index].default_name()
                    );
                }
            }
        });
        self.runtime = Some(runtime);
    }
}
impl Drop for ConnectionMonitor {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

/// `name` on one line, trimmed and shortened; `None` when nothing printable is left.
pub fn clean_name(name: &str) -> Option<String> {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

impl std::fmt::Debug for DesktopPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopPairing")
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    #[tokio::test]
    async fn status_negotiates_ca_transfer_and_falls_back_to_legacy_heartbeat() {
        crate::proxy::install_crypto_provider();
        for protocol in [STATUS_PROTOCOL, STATUS_CA_PROTOCOL] {
            let rcgen::CertifiedKey { cert, signing_key } =
                rcgen::generate_simple_self_signed(vec!["privaxy-desktop".into()]).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let pair = DesktopPairing {
                version: 1,
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
                token: "a".repeat(64),
                certificate_pem: cert.pem(),
                name: None,
                desktop_id: None,
            };
            let mut config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    rustls_pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
                )
                .unwrap();
            config.alpn_protocols = vec![protocol.to_vec()];
            let pem = crate::proxy::ca::CertAuthority::generate()
                .unwrap()
                .certificate_pem;
            let expected = pem.clone();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut tls = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                    .accept(tcp)
                    .await
                    .unwrap();
                let mut token = [0; 64];
                tls.read_exact(&mut token).await.unwrap();
                assert_eq!(token, [b'a'; 64]);
                tls.write_all(b"OK").await.unwrap();
                assert_eq!(tls.read_u8().await.unwrap(), b'P');
                tls.write_all(b"P").await.unwrap();
                if protocol == STATUS_CA_PROTOCOL {
                    assert_eq!(tls.read_u8().await.unwrap(), b'C');
                    tls.write_u32(pem.len() as u32).await.unwrap();
                    tls.write_all(pem.as_bytes()).await.unwrap();
                }
                // v1 must receive another heartbeat, never an unsupported CA command.
                assert_eq!(tls.read_u8().await.unwrap(), b'P');
                tls.write_all(b"P").await.unwrap();
                tls.shutdown().await.unwrap();
            });
            let mut monitor = ConnectionMonitor::default();
            monitor.sync(std::slice::from_ref(&pair), true);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    match monitor.ca_transfer() {
                        CaTransfer::Received(ca) if protocol == STATUS_CA_PROTOCOL => {
                            assert_eq!(ca.pem(), expected);
                            assert!(ca.belongs_to(&pair));
                            break;
                        }
                        CaTransfer::Unsupported if protocol == STATUS_PROTOCOL => break,
                        _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                    }
                }
            })
            .await
            .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            monitor.sync(&[], true);
            assert_eq!(monitor.ca_transfer(), CaTransfer::Waiting);
            assert!(monitor.current().is_none());
        }
    }

    #[tokio::test]
    async fn pairing_code_and_pinned_relay_round_trip() {
        crate::proxy::install_crypto_provider();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["privaxy-desktop".into()]).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pair = DesktopPairing {
            version: 1,
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            token: "a".repeat(64),
            certificate_pem: cert.pem(),
            name: None,
            desktop_id: None,
        };
        let code = format!(
            "privaxy-pair-v1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&pair).unwrap())
        );
        assert_eq!(DesktopPairing::parse(&code).unwrap(), pair);
        let uri = code.replace("privaxy-pair-v1:", "privaxy://pair?code=");
        assert_eq!(DesktopPairing::parse(&uri).unwrap(), pair);
        let mut named = serde_json::to_value(&pair).unwrap();
        named["name"] = " WORK\u{7}-PC\n".into();
        let long = "a".repeat(65);
        for (id, kept) in [
            ("0123abcd", true),
            ("not hex", false),
            (long.as_str(), false),
        ] {
            named["desktop_id"] = id.into();
            let code = format!(
                "privaxy-pair-v1:{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(named.to_string())
            );
            let parsed = DesktopPairing::parse(&code).unwrap();
            assert_eq!(parsed.name.as_deref(), Some("WORK-PC"));
            assert_eq!(parsed.desktop_id.as_deref(), kept.then_some(id));
        }
        let moved = pair.with_address(" 100.64.0.2 ", "8101").unwrap();
        assert!(moved.is_tailscale());
        assert_eq!(moved.token, pair.token);
        assert_eq!(moved.certificate_pem, pair.certificate_pem);
        for host in [
            "",
            "0.0.0.0",
            "::",
            "http://desktop",
            "192.168.1.2:8101",
            "bad\thost",
        ] {
            assert!(pair.with_address(host, "8101").is_err());
        }
        assert!(pair.with_address("192.168.1.2", "0").is_err());
        assert!(pair.with_address("192.168.1.2", "65536").is_err());
        assert!(DesktopPairing::parse(&(uri + "&unexpected=1")).is_err());
        assert!(!format!("{pair:?}").contains(&pair.token));
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls_pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            )
            .unwrap();
        let remote = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = tokio_rustls::TlsAcceptor::from(Arc::new(server))
                .accept(tcp)
                .await
                .unwrap();
            let mut token = [0; 64];
            tls.read_exact(&mut token).await.unwrap();
            assert_eq!(token, [b'a'; 64]);
            tls.write_all(b"OK").await.unwrap();
            let (mut r, mut w) = tokio::io::split(tls);
            tokio::io::copy(&mut r, &mut w).await.unwrap();
            w.shutdown().await.unwrap();
        });
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(local.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, _) = local.accept().await.unwrap();
        let relay = tokio::spawn(async move { pair.relay(accepted).await });
        client
            .write_all(b"HTTP through paired desktop")
            .await
            .unwrap();
        let mut out = [0; 27];
        client.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"HTTP through paired desktop");
        client.shutdown().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), relay)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        remote.await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_desktop_fails_over_to_the_next_reachable_one() {
        crate::proxy::install_crypto_provider();
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = closed.local_addr().unwrap().port();
        drop(closed);
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["privaxy-desktop".into()]).unwrap();
        let home = DesktopPairing {
            version: 1,
            host: "127.0.0.1".into(),
            port: dead_port,
            token: "a".repeat(64),
            certificate_pem: cert.pem(),
            name: Some("Home".into()),
            desktop_id: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let work = DesktopPairing {
            port: listener.local_addr().unwrap().port(),
            token: "b".repeat(64),
            name: Some("Work".into()),
            ..home.clone()
        };
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls_pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            )
            .unwrap();
        config.alpn_protocols = vec![STATUS_PROTOCOL.to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let mut tls = acceptor.accept(tcp).await.unwrap();
                    let mut token = [0; 64];
                    tls.read_exact(&mut token).await.unwrap();
                    assert_eq!(token, [b'b'; 64]);
                    tls.write_all(b"OK").await.unwrap();
                    while tls.read_u8().await.is_ok_and(|byte| byte == b'P') {
                        tls.write_all(b"P").await.unwrap();
                    }
                });
            }
        });
        async fn until(mut condition: impl FnMut() -> bool) {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !condition() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("timed out");
        }

        let mut monitor = ConnectionMonitor::default();
        let route = monitor.route();
        monitor.sync(&[home.clone(), work.clone()], true);
        assert_eq!(monitor.current(), Some(home.clone()));
        let mut switched = None;
        until(|| {
            switched = switched.take().or_else(|| monitor.take_switch());
            switched.is_some() && monitor.status() == ConnectionStatus::Connected
        })
        .await;
        assert_eq!(switched, Some(work.identity_key()));
        assert_eq!(route.read().unwrap().as_ref(), Some(&work));

        // Recording the switch, with the reachable desktop first, keeps the connection.
        monitor.sync(&[work.clone(), home.clone()], true);
        assert_eq!(monitor.current(), Some(work.clone()));
        assert_eq!(monitor.status(), ConnectionStatus::Connected);

        // Without failover, only the preferred desktop is tried.
        monitor.sync(&[home.clone(), work.clone()], false);
        assert_eq!(route.read().unwrap().as_ref(), Some(&home));
        until(|| matches!(monitor.status(), ConnectionStatus::Offline(_))).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(monitor.take_switch().is_none());
        assert_eq!(monitor.current(), Some(home));
    }
}
