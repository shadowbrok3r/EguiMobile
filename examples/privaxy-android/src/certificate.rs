//! Check the exact current CA against Android's enabled certificate store.
//! This proves installation for this Android profile, not trust by every client app.

use crate::proxy::ca::CertAuthority;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CertificateStatus {
    #[default]
    Checking,
    Installed,
    Missing,
    Invalid,
    Unavailable,
}

impl CertificateStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Checking => "Checking the current certificate…",
            Self::Installed => "Current CA installed and enabled in Android.",
            Self::Missing => {
                "Current CA not found in Android's enabled certificates. Install it from Settings."
            }
            Self::Invalid => {
                "The current CA is invalid, expired or not yet valid. Check the device date or regenerate it."
            }
            Self::Unavailable => "Certificate installation could not be verified on this device.",
        }
    }

    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Missing | Self::Invalid)
    }
}

#[derive(Default)]
pub struct CertificateCheck {
    pub status: CertificateStatus,
    pem: String,
    pending: Option<Receiver<CertificateStatus>>,
    checked_at: Option<Instant>,
}

impl CertificateCheck {
    pub fn invalidate(&mut self) {
        self.pending = None;
        self.status = CertificateStatus::Checking;
        self.checked_at = None;
    }

    pub fn update(&mut self, ca: &CertAuthority) {
        if self.pem != ca.certificate_pem {
            // Drop an old in-flight result when the CA changes; it cannot verify the new CA.
            self.pending = None;
            self.pem.clone_from(&ca.certificate_pem);
            self.checked_at = None;
            self.status = CertificateStatus::Checking;
        }
        if let Some(pending) = &self.pending {
            match pending.try_recv() {
                Ok(status) => {
                    self.status = status;
                    self.pending = None;
                    self.checked_at = Some(Instant::now());
                }
                Err(TryRecvError::Disconnected) => {
                    self.status = CertificateStatus::Unavailable;
                    self.pending = None;
                    self.checked_at = Some(Instant::now());
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.pending.is_none()
            && self
                .checked_at
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
        {
            let der = crate::proxy::ca::pem_to_der(&self.pem);
            let (tx, rx) = channel();
            self.pending = Some(rx);
            self.status = CertificateStatus::Checking;
            std::thread::spawn(move || {
                let status = der.map_or(CertificateStatus::Invalid, |der| check_android(&der));
                let _ = tx.send(status);
            });
        }
    }
}

#[cfg(not(target_os = "android"))]
fn check_android(_: &[u8]) -> CertificateStatus {
    CertificateStatus::Unavailable
}

#[cfg(target_os = "android")]
fn check_android(der: &[u8]) -> CertificateStatus {
    use jni::objects::{JObject, JValue};
    let context = ndk_context::android_context();
    // Android owns the VM for the process lifetime. Only local JNI references are allocated.
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(context.vm().cast()) }) else {
        return CertificateStatus::Unavailable;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return CertificateStatus::Unavailable;
    };
    let result = env.with_local_frame::<_, _, jni::errors::Error>(16, |env| {
        let kind = env.new_string("X.509")?;
        let factory = env
            .call_static_method(
                "java/security/cert/CertificateFactory",
                "getInstance",
                "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
                &[(&kind).into()],
            )?
            .l()?;
        let bytes = env.byte_array_from_slice(der)?;
        let stream = env.new_object("java/io/ByteArrayInputStream", "([B)V", &[(&bytes).into()])?;
        let cert = env
            .call_method(
                &factory,
                "generateCertificate",
                "(Ljava/io/InputStream;)Ljava/security/cert/Certificate;",
                &[(&stream).into()],
            )?
            .l()?;
        if let Err(error) = env.call_method(&cert, "checkValidity", "()V", &[]) {
            if env.exception_check()? {
                let exception = env.exception_occurred()?;
                env.exception_clear()?;
                if env
                    .is_instance_of(&exception, "java/security/cert/CertificateExpiredException")?
                    || env.is_instance_of(
                        &exception,
                        "java/security/cert/CertificateNotYetValidException",
                    )?
                {
                    return Ok(CertificateStatus::Invalid);
                }
            }
            return Err(error);
        }
        let store_name = env.new_string("AndroidCAStore")?;
        let store = env
            .call_static_method(
                "java/security/KeyStore",
                "getInstance",
                "(Ljava/lang/String;)Ljava/security/KeyStore;",
                &[(&store_name).into()],
            )?
            .l()?;
        let null = JObject::null();
        env.call_method(
            &store,
            "load",
            "(Ljava/io/InputStream;[C)V",
            &[JValue::Object(&null), JValue::Object(&null)],
        )?;
        // Android matches the entire encoded certificate, and excludes disabled system CAs.
        let alias = env
            .call_method(
                &store,
                "getCertificateAlias",
                "(Ljava/security/cert/Certificate;)Ljava/lang/String;",
                &[(&cert).into()],
            )?
            .l()?;
        if alias.is_null() {
            return Ok(CertificateStatus::Missing);
        }
        let installed = env
            .call_method(
                &store,
                "getCertificate",
                "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
                &[(&alias).into()],
            )?
            .l()?;
        let enabled = !installed.is_null()
            && env
                .call_method(
                    &cert,
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    &[(&installed).into()],
                )?
                .z()?;
        Ok(if enabled {
            CertificateStatus::Installed
        } else {
            CertificateStatus::Missing
        })
    });
    if result.is_err() {
        let _ = env.exception_clear();
        log::warn!("Could not check Android's certificate store");
    }
    result.unwrap_or(CertificateStatus::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_ca_discards_a_successful_check_of_the_old_certificate() {
        let old = CertAuthority::generate().unwrap();
        let replacement = CertAuthority::generate().unwrap();
        let (tx, rx) = channel();
        tx.send(CertificateStatus::Installed).unwrap();
        let mut check = CertificateCheck {
            pem: old.certificate_pem,
            status: CertificateStatus::Installed,
            pending: Some(rx),
            checked_at: Some(Instant::now()),
        };
        check.update(&replacement);
        assert_eq!(check.pem, replacement.certificate_pem);
        assert_eq!(check.status, CertificateStatus::Checking);
        assert!(check.pending.is_some());
    }
}
