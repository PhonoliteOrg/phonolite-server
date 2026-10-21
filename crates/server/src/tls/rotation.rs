//! Stage a replacement identity without changing a running listener. The pending
//! record remains until both files and session revocation succeed at startup,
//! so interruption between replacing the two files is recoverable on restart.
use super::{harden_file, replace_file, write_secret};
use crate::config::{resolve_path, ServerConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingIdentity {
    certificate: String,
    private_key: String,
}

impl Drop for PendingIdentity {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

fn pending_path(config_path: &Path) -> PathBuf {
    let mut path = config_path.as_os_str().to_os_string();
    path.push(".tls-rotation.json");
    PathBuf::from(path)
}

pub(crate) fn prepare_rotation(
    config_path: &Path,
    revoke_sessions: impl FnOnce() -> Result<(), String>,
) -> Result<String, String> {
    let pending_path = pending_path(config_path);
    let pair = rcgen::generate_simple_self_signed(vec![
        "phonolite".into(),
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
    ])
    .map_err(|_| "Could not generate a new server identity.")?;
    let fingerprint = format!("{:x}", Sha256::digest(pair.key_pair.public_key_der()));
    let pending = PendingIdentity {
        certificate: pair.cert.pem(),
        private_key: pair.key_pair.serialize_pem(),
    };
    let bytes = Zeroizing::new(
        serde_json::to_vec(&pending).map_err(|_| "Could not prepare the new server identity.")?,
    );
    if let Err(error) = write_secret(&pending_path, &bytes) {
        if error.kind() == io::ErrorKind::AlreadyExists {
            return Err(
                "A key rotation is already pending. Restart the server to finish it.".into(),
            );
        }
        // create_new protects an existing rotation; only our incomplete file
        // can exist here after a write/permission failure.
        let _ = fs::remove_file(&pending_path);
        return Err(
            "Could not save the new server identity. The current keys are unchanged.".into(),
        );
    }
    if revoke_sessions().is_err() {
        if fs::remove_file(&pending_path).is_err() {
            return Err("New keys are staged, but sessions could not be revoked. Stop and restart the server manually to finish rotation.".into());
        }
        return Err("Could not revoke sessions. Key rotation was not activated.".into());
    }
    Ok(fingerprint)
}

pub(crate) fn activate_pending_rotation(
    config: &ServerConfig,
    config_path: &Path,
    revoke_sessions: impl FnOnce() -> Result<(), String>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let pending_path = pending_path(config_path);
    let metadata = match fs::metadata(&pending_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() > 64 * 1024 {
        return Err("invalid pending server identity".into());
    }
    harden_file(&pending_path)?;
    let bytes = Zeroizing::new(fs::read(&pending_path)?);
    let pending: PendingIdentity = serde_json::from_slice(&bytes).map_err(|_| {
        "invalid pending server identity; restore its pending file or remove it to cancel rotation"
    })?;
    let certificates =
        rustls_pemfile::certs(&mut io::BufReader::new(pending.certificate.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut io::BufReader::new(pending.private_key.as_bytes()))?
        .ok_or("missing pending TLS key")?;
    // Check that the key and certificate match before touching either live file.
    rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(certificates, key)?;
    // Revoke again at startup, including sessions created while restart was pending.
    revoke_sessions()?;
    let certificate_path = resolve_path(config_path, &config.quic_cert_path);
    let key_path = resolve_path(config_path, &config.quic_key_path);
    if certificate_path == key_path {
        let combined = Zeroizing::new(format!("{}{}", pending.certificate, pending.private_key));
        replace_secret(&key_path, combined.as_bytes())?;
    } else {
        replace_secret(&key_path, pending.private_key.as_bytes())?;
        replace_secret(&certificate_path, pending.certificate.as_bytes())?;
    }
    fs::remove_file(pending_path)?;
    Ok(true)
}

fn replace_secret(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.rotation.pem", uuid::Uuid::new_v4()));
    let result = write_secret(&temporary, bytes).and_then(|()| replace_file(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> (ServerConfig, PathBuf, Directory) {
        let root =
            std::env::temp_dir().join(format!("phonolite-rotation-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("config.yaml");
        let config = ServerConfig::default();
        super::super::identity(&config, &path, false).unwrap();
        (config, path, Directory(root))
    }

    #[test]
    fn rotation_changes_both_keys_only_at_startup_and_revokes_sessions_twice() {
        let (config, path, _directory) = fixture();
        let cert = resolve_path(&path, &config.quic_cert_path);
        let key = resolve_path(&path, &config.quic_key_path);
        let original_cert = fs::read(&cert).unwrap();
        let original_key = fs::read(&key).unwrap();
        let mut revocations = 0;
        let fingerprint = prepare_rotation(&path, || {
            revocations += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(fingerprint.len(), 64);
        assert_eq!(original_cert, fs::read(&cert).unwrap());
        assert_eq!(original_key, fs::read(&key).unwrap());
        assert!(activate_pending_rotation(&config, &path, || {
            revocations += 1;
            Ok(())
        })
        .unwrap());
        assert_eq!(revocations, 2);
        assert_ne!(original_cert, fs::read(&cert).unwrap());
        assert_ne!(original_key, fs::read(&key).unwrap());
        let pair = rcgen::KeyPair::from_pem(&fs::read_to_string(&key).unwrap()).unwrap();
        assert_eq!(
            fingerprint,
            format!("{:x}", Sha256::digest(pair.public_key_der()))
        );
        assert!(
            !activate_pending_rotation(&config, &path, || panic!("already activated")).unwrap()
        );
    }

    #[test]
    fn failed_revocation_preserves_the_current_identity() {
        let (config, path, _directory) = fixture();
        let key = resolve_path(&path, &config.quic_key_path);
        let original = fs::read(&key).unwrap();
        assert!(prepare_rotation(&path, || Err("database unavailable".into())).is_err());
        assert!(!pending_path(&path).exists());
        assert_eq!(original, fs::read(key).unwrap());
    }

    #[test]
    fn startup_revocation_failure_keeps_the_old_pair_and_pending_rotation_for_retry() {
        let (config, path, _directory) = fixture();
        let key = resolve_path(&path, &config.quic_key_path);
        let original = fs::read(&key).unwrap();
        prepare_rotation(&path, || Ok(())).unwrap();
        assert!(
            activate_pending_rotation(&config, &path, || Err("database unavailable".into()))
                .is_err()
        );
        assert_eq!(original, fs::read(key).unwrap());
        assert!(pending_path(&path).exists());
        assert!(activate_pending_rotation(&config, &path, || Ok(())).unwrap());
    }

    #[test]
    fn repeated_requests_cannot_replace_a_pending_rotation() {
        let (_config, path, _directory) = fixture();
        prepare_rotation(&path, || Ok(())).unwrap();
        let original = fs::read(pending_path(&path)).unwrap();
        assert!(prepare_rotation(&path, || panic!("must not revoke again")).is_err());
        assert_eq!(original, fs::read(pending_path(&path)).unwrap());
    }

    #[test]
    fn interrupted_activation_is_recoverable_without_serving_a_mixed_pair() {
        let (config, path, _directory) = fixture();
        prepare_rotation(&path, || Ok(())).unwrap();
        let marker = pending_path(&path);
        let pending: PendingIdentity = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        // Simulate a process exit after only the private key was replaced.
        replace_secret(
            &resolve_path(&path, &config.quic_key_path),
            pending.private_key.as_bytes(),
        )
        .unwrap();
        assert!(activate_pending_rotation(&config, &path, || Ok(())).unwrap());
        assert_eq!(
            pending.certificate,
            fs::read_to_string(resolve_path(&path, &config.quic_cert_path)).unwrap()
        );
        assert!(!marker.exists());
    }

    #[test]
    fn invalid_pending_material_never_overwrites_the_live_pair() {
        let (config, path, _directory) = fixture();
        let key = resolve_path(&path, &config.quic_key_path);
        let original = fs::read(&key).unwrap();
        write_secret(
            &pending_path(&path),
            br#"{"certificate":"invalid","private_key":"secret"}"#,
        )
        .unwrap();
        assert!(activate_pending_rotation(&config, &path, || panic!("validate first")).is_err());
        assert_eq!(original, fs::read(key).unwrap());
        assert!(pending_path(&path).exists());
    }
}
