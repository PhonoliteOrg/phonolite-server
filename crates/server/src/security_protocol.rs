//! Phonolite authentication profile v1. Keep in sync with the native client.
use argon2::{Algorithm, Argon2, Params, Version};
use opaque_ke::{CipherSuite, Ristretto255, TripleDh};
use sha2::Sha512;

pub const PROFILE: &str = "phonolite-opaque-v1";
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-Channel-Binding";
pub const SERVER_IDENTITY: &[u8] = b"phonolite-server-v1";

pub struct Suite;
impl CipherSuite for Suite {
    type OprfCs = Ristretto255;
    type KeyExchange = TripleDh<Ristretto255, Sha512>;
    type Ksf = Argon2<'static>;
}

pub fn ksf() -> Argon2<'static> {
    // Fixed profile: 64 MiB, three passes, one lane. Never accept network overrides.
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(65536, 3, 1, None).unwrap(),
    )
}

pub fn context(binding: &[u8; 32], username: &str) -> Vec<u8> {
    let mut value = PROFILE.as_bytes().to_vec();
    value.push(0);
    value.extend_from_slice(binding);
    value.extend_from_slice(username.trim().to_ascii_lowercase().as_bytes());
    value
}

pub fn enroll(
    setup: &opaque_ke::ServerSetup<Suite>,
    id: &[u8],
    password: &[u8],
) -> Result<Vec<u8>, String> {
    use opaque_ke::{
        ClientRegistration, ClientRegistrationFinishParameters, Identifiers, ServerRegistration,
    };
    let mut rng = rand08::rngs::OsRng;
    let first = ClientRegistration::<Suite>::start(&mut rng, password)
        .map_err(|_| "registration failed")?;
    let response =
        ServerRegistration::start(setup, first.message, id).map_err(|_| "registration failed")?;
    let hash = ksf();
    let upload = first
        .state
        .finish(
            &mut rng,
            password,
            response.message,
            ClientRegistrationFinishParameters::new(
                Identifiers {
                    client: None,
                    server: Some(SERVER_IDENTITY),
                },
                Some(&hash),
            ),
        )
        .map_err(|_| "registration failed")?;
    Ok(ServerRegistration::<Suite>::finish(upload.message)
        .serialize()
        .to_vec())
}
