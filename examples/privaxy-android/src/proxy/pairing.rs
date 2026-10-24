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
        let json = if let Some(encoded) = input.strip_prefix("privaxy-pair-v1:") {
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
        if self.version != 1
            || self.port == 0
            || self.host.is_empty()
            || self.host.contains(['/', '\r', '\n', ' '])
            || self.token.len() != 64
            || !self.token.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Unsupported or malformed pairing code".into());
        }
        self.tls_config()?;
        Ok(())
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
        self.validate()?;
        let connect = async {
            let tcp = tokio::net::TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|e| e.to_string())?;
            let name = rustls_pki_types::ServerName::try_from("privaxy-desktop").unwrap();
            let mut remote = tokio_rustls::TlsConnector::from(Arc::new(self.tls_config()?))
                .connect(name, tcp)
                .await
                .map_err(|e| e.to_string())?;
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
        let mut remote = tokio::time::timeout(std::time::Duration::from_secs(15), connect)
            .await
            .map_err(|_| "Desktop connection timed out")??;
        tokio::io::copy_bidirectional(&mut local, &mut remote)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
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
