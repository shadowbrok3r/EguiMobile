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
            name: None,
            desktop_id: None,
        }
    }
    fn scratch() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "privaxy-ca-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn public_ca_survives_reload_and_address_changes_but_not_repairing() {
        let ca = crate::proxy::ca::CertAuthority::generate().unwrap();
        let pair = pair();
        let received = DesktopCa::from_pem(&pair, ca.certificate_pem.clone()).unwrap();
        assert_eq!(received.fingerprint(), ca.fingerprint());
        let root = scratch();
        let paths = crate::proxy::config::Paths::new(&root);
        let mut config = paths.load_or_create().unwrap();
        let local_ca = config.ca.clone();
        assert!(config.add_desktop(pair.clone()));
        assert_eq!(config.store_desktop_ca(received.clone()), Some(false));
        assert_eq!(config.store_desktop_ca(received.clone()), None);
        paths.save(&config).unwrap();
        let mut restored = paths.load_or_create().unwrap();
        assert_eq!(restored.paired_ca(), Some(&received));
        assert_eq!(restored.ca, local_ca);
        restored.desktops[0].pairing.host = "100.64.0.1".into();
        assert_eq!(restored.paired_ca(), Some(&received));
        restored.desktops[0].pairing.token = "b".repeat(64);
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
        old.as_object_mut().unwrap().remove("desktops");
        let old: crate::proxy::config::Config = serde_json::from_value(old).unwrap();
        assert!(old.desktops.is_empty() && old.desktop_failover);
        assert_eq!(old.ca, local_ca);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_single_saved_pairing_becomes_the_first_desktop_with_its_ca() {
        let root = scratch();
        let paths = crate::proxy::config::Paths::new(&root);
        let config = paths.load_or_create().unwrap();
        let pair = pair();
        let received = DesktopCa::from_pem(
            &pair,
            crate::proxy::ca::CertAuthority::generate()
                .unwrap()
                .certificate_pem,
        )
        .unwrap();
        let mut legacy = serde_json::to_value(&config).unwrap();
        let fields = legacy.as_object_mut().unwrap();
        fields.remove("desktops");
        fields.remove("desktop_failover");
        fields.insert(
            "desktop_pairing".into(),
            serde_json::to_value(&pair).unwrap(),
        );
        fields.insert(
            "desktop_ca".into(),
            serde_json::to_value(&received).unwrap(),
        );
        fields.insert("desktop_enabled".into(), true.into());
        std::fs::write(
            root.join("config.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let migrated = paths.load_or_create().unwrap();
        assert_eq!(migrated.desktop_pairings(), std::slice::from_ref(&pair));
        assert_eq!(migrated.desktops[0].name, "Desktop at 192.168.0.110");
        assert_eq!(migrated.paired_ca(), Some(&received));
        assert!(migrated.desktop_enabled && migrated.desktop_failover);
        paths.save(&migrated).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("config.json")).unwrap()).unwrap();
        assert!(saved.get("desktop_pairing").is_none() && saved.get("desktop_ca").is_none());
        assert_eq!(paths.load_or_create().unwrap(), migrated);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn several_desktops_are_kept_switched_between_and_forgotten() {
        let root = scratch();
        let mut config = crate::proxy::config::Paths::new(&root)
            .load_or_create()
            .unwrap();
        let home = pair();
        let work = DesktopPairing {
            host: "10.1.2.3".into(),
            token: "c".repeat(64),
            name: Some("WORK-PC".into()),
            desktop_id: Some("1f".repeat(16)),
            ..pair()
        };
        assert!(config.add_desktop(home.clone()));
        assert!(config.add_desktop(work.clone()));
        assert_eq!(config.desktop_pairings(), [work.clone(), home.clone()]);
        assert_eq!(config.active_desktop().unwrap().name, "WORK-PC");

        // Re-pairing a desktop updates it in place, keeping a name the user chose.
        config.desktops[1].name = "Home PC".into();
        let moved = DesktopPairing {
            host: "192.168.0.111".into(),
            ..home.clone()
        };
        assert!(!config.add_desktop(moved.clone()));
        assert_eq!(config.desktop_pairings(), [moved.clone(), work.clone()]);
        assert_eq!(config.active_desktop().unwrap().name, "Home PC");

        // A desktop that revoked its pairings returns with a new identity and the same ID.
        let rekeyed = DesktopPairing {
            token: "d".repeat(64),
            ..work.clone()
        };
        assert!(!config.add_desktop(rekeyed.clone()));
        assert_eq!(config.desktop_pairings(), [rekeyed.clone(), moved.clone()]);

        // Another computer with the same name is a separate desktop.
        let namesake = DesktopPairing {
            token: "e".repeat(64),
            desktop_id: Some("2e".repeat(16)),
            ..work.clone()
        };
        assert!(config.add_desktop(namesake.clone()));
        assert_eq!(
            config.desktop_pairings(),
            [namesake.clone(), rekeyed.clone(), moved.clone()]
        );

        // Each CA stays with the desktop that sent it.
        let home_ca = DesktopCa::from_pem(
            &home,
            crate::proxy::ca::CertAuthority::generate()
                .unwrap()
                .certificate_pem,
        )
        .unwrap();
        assert_eq!(config.store_desktop_ca(home_ca.clone()), Some(false));
        assert!(config.paired_ca().is_none());
        assert!(config.use_desktop(&home.identity_key()));
        assert_eq!(config.paired_ca(), Some(&home_ca));
        assert!(!config.use_desktop("unknown"));

        config.desktop_enabled = true;
        assert_eq!(
            config.forget_desktop(&home.identity_key()).unwrap().name,
            "Home PC"
        );
        assert!(config.desktop_enabled);
        assert_eq!(
            config.desktop_pairings(),
            [namesake.clone(), rekeyed.clone()]
        );
        config.forget_desktop(&rekeyed.identity_key()).unwrap();
        config.forget_desktop(&namesake.identity_key()).unwrap();
        assert!(config.desktops.is_empty() && !config.desktop_enabled);
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
