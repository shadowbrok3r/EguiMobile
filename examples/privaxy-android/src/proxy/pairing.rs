//! Versioned, certificate-pinned desktop relay. Credentials stay in private app storage.
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopPairing {
    pub version: u8,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub certificate_pem: String,
}
impl DesktopPairing {
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
        let pair: Self = serde_json::from_str(json).map_err(|_| "Invalid pairing code")?;
        pair.validate()?;
        Ok(pair)
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
        tokio::io::copy_bidirectional(&mut local, &mut remote)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    async fn connect(
        &self,
        status: bool,
    ) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, String> {
        self.validate()?;
        let connect = async {
            let tcp = tokio::net::TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|e| self.connection_error(&e))?;
            let name = rustls_pki_types::ServerName::try_from("privaxy-desktop").unwrap();
            let mut config = self.tls_config()?;
            if status {
                config.alpn_protocols = vec![b"privaxy-status-v1".to_vec()];
            }
            let mut remote = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| e.to_string())?;
            if status && remote.get_ref().1.alpn_protocol() != Some(b"privaxy-status-v1") {
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
            Ok::<_, String>(remote)
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(if status { 5 } else { 15 }),
            connect,
        )
        .await
        .map_err(|_| format!("Desktop {}:{} did not respond in time. Check that the relay is running and TCP port {} is allowed from the phone's network. Your pairing is still saved.", self.host, self.port, self.port))?
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

#[derive(Default)]
pub struct ConnectionMonitor {
    pair: Option<DesktopPairing>,
    runtime: Option<tokio::runtime::Runtime>,
    status: Arc<std::sync::Mutex<ConnectionStatus>>,
}
impl ConnectionMonitor {
    pub fn status(&self) -> ConnectionStatus {
        self.status.lock().unwrap().clone()
    }
    pub fn retry(&mut self) {
        let pair = self.pair.take();
        self.sync(pair.as_ref());
    }
    pub fn sync(&mut self, pair: Option<&DesktopPairing>) {
        if self.pair.as_ref() == pair {
            return;
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
        self.pair = pair.cloned();
        // Use a fresh status cell so a cancelled old connection cannot overwrite the new one.
        self.status = Arc::default();
        let Some(pair) = self.pair.clone() else {
            return;
        };
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
        runtime.spawn(async move {
            loop {
                let result = async {
                    let mut stream = pair.connect(true).await?;
                    loop {
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
                        *status.lock().unwrap() = ConnectionStatus::Connected;
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), String>(())
                }
                .await;
                *status.lock().unwrap() = ConnectionStatus::Offline(result.unwrap_err());
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
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

impl std::fmt::Debug for DesktopPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopPairing")
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
        };
        let code = format!(
            "privaxy-pair-v1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&pair).unwrap())
        );
        assert_eq!(DesktopPairing::parse(&code).unwrap(), pair);
        let uri = code.replace("privaxy-pair-v1:", "privaxy://pair?code=");
        assert_eq!(DesktopPairing::parse(&uri).unwrap(), pair);
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
}
