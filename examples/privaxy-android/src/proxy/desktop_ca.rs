//! Public inspection certificate received through the pinned, authenticated desktop relay.
use super::pairing::DesktopPairing;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const STATUS_CA_PROTOCOL: &[u8] = b"privaxy-status-v2";
pub const STATUS_PROTOCOL: &[u8] = b"privaxy-status-v1";
const MAX_CA_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesktopCa {
    pairing_id: String,
    certificate_pem: String,
}

impl DesktopCa {
    pub fn from_pem(pair: &DesktopPairing, pem: String) -> Result<Self, String> {
        validate(&pem)?;
        Ok(Self {
            pairing_id: pair.identity_key(),
            certificate_pem: pem,
        })
    }
    pub fn belongs_to(&self, pair: &DesktopPairing) -> bool {
        self.pairing_id == pair.identity_key()
    }
    pub fn pem(&self) -> &str {
        &self.certificate_pem
    }
    pub fn fingerprint(&self) -> String {
        super::ca::pem_to_der(&self.certificate_pem)
            .map(|der| {
                Sha256::digest(der)
                    .iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .unwrap_or_else(|| "Invalid certificate".into())
    }
    pub fn export_filename(&self) -> Result<String, String> {
        validate(self.pem())?;
        let fingerprint = self.fingerprint().replace(':', "").to_ascii_lowercase();
        Ok(format!("privaxy-desktop-ca-{}.crt", &fingerprint[..12]))
    }
}

fn validate(pem: &str) -> Result<(), String> {
    // Accept exactly one public certificate, never a key or certificate bundle.
    if pem.len() > MAX_CA_BYTES
        || !pem.trim().starts_with("-----BEGIN CERTIFICATE-----")
        || !pem.trim().ends_with("-----END CERTIFICATE-----")
        || pem.matches("-----BEGIN").count() != 1
        || pem.matches("-----END").count() != 1
    {
        return Err("Desktop did not send a single public CA certificate".into());
    }
    let der = super::ca::pem_to_der(pem).ok_or("Invalid desktop CA encoding")?;
    let (remaining, cert) =
        x509_parser::parse_x509_certificate(&der).map_err(|_| "Invalid desktop CA certificate")?;
    if !remaining.is_empty()
        || !cert
            .basic_constraints()
            .map_err(|_| "Invalid desktop CA constraints")?
            .is_some_and(|extension| extension.value.ca)
        || cert
            .key_usage()
            .map_err(|_| "Invalid desktop CA key usage")?
            .is_some_and(|extension| !extension.value.key_cert_sign())
    {
        return Err("Desktop certificate is not a certificate authority".into());
    }
    if !cert.validity().is_valid() {
        return Err(
            "Desktop CA is expired or not yet valid. Check the device clocks and desktop CA."
                .into(),
        );
    }
    Ok(())
}

pub async fn receive<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    pair: &DesktopPairing,
) -> Result<DesktopCa, String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        stream.write_all(b"C").await.map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;
        let size = stream.read_u32().await.map_err(|e| e.to_string())? as usize;
        if size == 0 || size > MAX_CA_BYTES {
            return Err("Invalid desktop CA transfer size".into());
        }
        let mut pem = vec![0; size];
        stream
            .read_exact(&mut pem)
            .await
            .map_err(|e| e.to_string())?;
        DesktopCa::from_pem(
            pair,
            String::from_utf8(pem).map_err(|_| "Invalid desktop CA encoding")?,
        )
    })
    .await
    .map_err(|_| "Desktop CA transfer timed out".to_owned())?
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum CaTransfer {
    #[default]
    Waiting,
    Unsupported,
    Received(DesktopCa),
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> DesktopPairing {
        DesktopPairing {
            version: 1,
            host: "192.168.0.110".into(),
            port: 8101,
            token: "a".repeat(64),
            certificate_pem: "pinned relay identity".into(),
        }
    }
    #[test]
    fn public_ca_survives_reload_and_address_changes_but_not_repairing() {
        let ca = crate::proxy::ca::CertAuthority::generate().unwrap();
        let pair = pair();
        let received = DesktopCa::from_pem(&pair, ca.certificate_pem.clone()).unwrap();
        assert_eq!(received.fingerprint(), ca.fingerprint());
        let root = std::env::temp_dir().join(format!(
            "privaxy-ca-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = crate::proxy::config::Paths::new(&root);
        let mut config = paths.load_or_create().unwrap();
        let local_ca = config.ca.clone();
        config.desktop_pairing = Some(pair.clone());
        config.desktop_ca = Some(received.clone());
        paths.save(&config).unwrap();
        let mut restored = paths.load_or_create().unwrap();
        assert_eq!(restored.paired_ca(), Some(&received));
        assert_eq!(restored.ca, local_ca);
        restored.desktop_pairing.as_mut().unwrap().host = "100.64.0.1".into();
        assert_eq!(restored.paired_ca(), Some(&received));
        restored.desktop_pairing.as_mut().unwrap().token = "b".repeat(64);
        assert!(restored.paired_ca().is_none());
        let export = paths.export_desktop_ca(&received).unwrap();
        assert_eq!(std::fs::read_to_string(export).unwrap(), ca.certificate_pem);
        let next = DesktopCa::from_pem(
            &pair,
            crate::proxy::ca::CertAuthority::generate()
                .unwrap()
                .certificate_pem,
        )
        .unwrap();
        assert_ne!(
            received.export_filename().unwrap(),
            next.export_filename().unwrap()
        );
        let mut old = serde_json::to_value(&config).unwrap();
        old.as_object_mut().unwrap().remove("desktop_ca");
        let old: crate::proxy::config::Config = serde_json::from_value(old).unwrap();
        assert!(old.desktop_ca.is_none());
        assert_eq!(old.ca, local_ca);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn refuses_keys_bundles_leaf_certificates_and_expired_cas() {
        let ca = crate::proxy::ca::CertAuthority::generate().unwrap();
        for invalid in [
            String::new(),
            "garbage".into(),
            ca.private_key_pem.clone(),
            format!("{}{}", ca.certificate_pem, ca.private_key_pem),
            format!("{}{}", ca.certificate_pem, ca.certificate_pem),
        ] {
            assert!(DesktopCa::from_pem(&pair(), invalid).is_err());
        }
        let leaf = rcgen::generate_simple_self_signed(vec!["privaxy-desktop".into()]).unwrap();
        assert!(DesktopCa::from_pem(&pair(), leaf.cert.pem()).is_err());
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        let expired = params
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap();
        assert!(
            DesktopCa::from_pem(&pair(), expired.pem())
                .unwrap_err()
                .contains("expired")
        );
    }
    #[tokio::test]
    async fn bounded_transfer_rejects_oversize_truncated_and_invalid_frames() {
        let pem = crate::proxy::ca::CertAuthority::generate()
            .unwrap()
            .certificate_pem;
        for (length, payload, valid) in [
            (pem.len() as u32, pem.as_bytes().to_vec(), true),
            (0, vec![], false),
            (MAX_CA_BYTES as u32 + 1, vec![], false),
            (100, b"short".to_vec(), false),
            (3, b"bad".to_vec(), false),
        ] {
            let (mut phone, mut server) = tokio::io::duplex(32 * 1024);
            let sent = tokio::spawn(async move {
                assert_eq!(server.read_u8().await.unwrap(), b'C');
                server.write_u32(length).await.unwrap();
                server.write_all(&payload).await.unwrap();
                server.shutdown().await.unwrap();
            });
            let result = receive(&mut phone, &pair()).await;
            assert_eq!(result.is_ok(), valid, "frame length={length}");
            if valid {
                assert_eq!(result.unwrap().pem(), pem);
            }
            sent.await.unwrap();
        }
    }
}
