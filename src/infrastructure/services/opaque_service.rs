//! OPAQUE aPAKE service (Phase 0 substrate).
//!
//! OPAQUE (RFC 9807) is a zero-knowledge password-authenticated key exchange:
//! the passphrase never leaves the client, not on registration and not on
//! login. This module wraps the [`opaque_ke`] crate with a stable ciphersuite
//! type alias ([`OxiCloudSuite`]), a lazily-loaded per-process
//! [`ServerSetup`] persisted via env var, and a small handful of thin
//! wrappers over the four handshake steps.
//!
//! ## Phase 0 scope
//!
//! Endpoints are not yet wired. This module ships the primitives so the
//! subsequent phases can layer registration + login handlers, silent
//! migration hooks, and the eventual `opaque_only` cutover on top without
//! re-designing the type shape.
//!
//! ## Ciphersuite (frozen at v1)
//!
//! | Slot          | Choice                                                 |
//! |---------------|--------------------------------------------------------|
//! | `OprfCs`      | [`Ristretto255`] — SHA-512-backed VOPRF ciphersuite    |
//! | `KeGroup`     | [`Ristretto255`] — same group for the AKE              |
//! | `KeyExchange` | [`TripleDh`] — 3DH mutual auth (opaque-ke default AKE) |
//! | `Ksf`         | [`argon2::Argon2`] — memory-hard client-side stretch   |
//!
//! **Changing any slot invalidates every previously-minted envelope.**
//! Bumped via [`OpaqueConfig::ciphersuite_version`] with a matching
//! DB-level `opaque_ciphersuite_version` column so a future migration can
//! decide per-user whether to re-register or refuse login until the client
//! re-registers.
//!
//! ## What lives where
//!
//! The KSF is applied CLIENT-side — RFC 9807 puts the memory-hard stretch
//! before the OPRF exchange so the server never runs Argon2. The Argon2
//! params in [`OpaqueConfig`] are therefore a CLIENT concern (published to
//! the SPA at page-load); the server binds the type at compile time so the
//! wire shape matches but never invokes it.

use opaque_ke::CipherSuite;
use opaque_ke::Ristretto255;
use opaque_ke::ServerSetup;
use opaque_ke::key_exchange::tripledh::TripleDh;
use rand_core::OsRng;

use crate::common::config::OpaqueConfig;
use crate::common::errors::{DomainError, ErrorKind};

/// OxiCloud's OPAQUE ciphersuite binding. See the module-level table for
/// the slot choices and the invariants around changing them.
///
/// Zero-sized — this type exists only to name the ciphersuite for the
/// generic `opaque_ke` machinery; no instances are ever constructed.
#[derive(Debug, Clone, Copy)]
pub struct OxiCloudSuite;

impl CipherSuite for OxiCloudSuite {
    type OprfCs = Ristretto255;
    type KeGroup = Ristretto255;
    type KeyExchange = TripleDh;
    type Ksf = argon2::Argon2<'static>;
}

/// A configured OPAQUE server. Holds the persistent [`ServerSetup`] plus a
/// clone of the runtime [`OpaqueConfig`] so callers don't have to plumb
/// both. Cheap to clone — [`ServerSetup`] is a small keypair blob.
#[derive(Debug, Clone)]
pub struct OpaqueService {
    setup: ServerSetup<OxiCloudSuite>,
    config: OpaqueConfig,
}

impl OpaqueService {
    /// Build the service from runtime config. Expects the operator to have
    /// persisted the server setup already (via `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP`);
    /// call [`OpaqueService::generate_server_setup_b64`] first-time and print
    /// the value for the operator to paste into their env before enabling
    /// `OXICLOUD_AUTH_OPAQUE_MODE`.
    ///
    /// Rejects with `InternalError` if the setup is missing / malformed, or
    /// with `AccessDenied` if the mode is `off` (guarding against
    /// accidental use before the operator has explicitly opted in).
    pub fn from_config(config: OpaqueConfig) -> Result<Self, DomainError> {
        if config.mode == OpaqueMode::Off {
            return Err(DomainError::access_denied(
                "opaque",
                "OPAQUE is disabled (OXICLOUD_AUTH_OPAQUE_MODE=off)",
            ));
        }
        let setup_b64 = config.server_setup_b64.as_deref().ok_or_else(|| {
            DomainError::new(
                ErrorKind::InternalError,
                "opaque",
                "OXICLOUD_AUTH_OPAQUE_SERVER_SETUP is required when OPAQUE is enabled — \
                 generate one with `oxicloud-cli opaque setup` and persist it in the env",
            )
        })?;
        let setup = decode_server_setup(setup_b64)?;
        Ok(Self { setup, config })
    }

    /// Runtime OPAQUE mode. Handlers can gate behaviour on this without
    /// reaching for the whole config — see the phase plan in
    /// `docs/plan/opaque.md`.
    pub fn mode(&self) -> OpaqueMode {
        self.config.mode
    }

    /// The bound ciphersuite version, stamped into `opaque_ciphersuite_version`
    /// on registration so future migrations can reason per-user.
    pub fn ciphersuite_version(&self) -> i16 {
        self.config.ciphersuite_version
    }

    /// Client-side Argon2id memory cost (KiB) — published to the SPA
    /// via `GET /api/auth/opaque/params` so both sides configure
    /// matching KSF parameters. Values below are read-through from
    /// [`OpaqueConfig`]; individual accessors keep handlers from
    /// having to plumb the whole config struct.
    pub fn config_ksf_memory_kib(&self) -> u32 {
        self.config.ksf_memory_kib
    }

    /// Client-side Argon2id iterations. See [`config_ksf_memory_kib`].
    pub fn config_ksf_iterations(&self) -> u32 {
        self.config.ksf_iterations
    }

    /// Client-side Argon2id parallelism. See [`config_ksf_memory_kib`].
    pub fn config_ksf_parallelism(&self) -> u32 {
        self.config.ksf_parallelism
    }

    /// The persistent server setup — passed to `ServerRegistration::start`
    /// and `ServerLogin::start` in the handler layer. Kept accessible so
    /// callers can hold their own refs to it if they need to (e.g. inside
    /// a Tokio task), avoiding an extra `Arc` layer.
    pub fn setup(&self) -> &ServerSetup<OxiCloudSuite> {
        &self.setup
    }

    /// Generate a fresh server setup and return it as base64. Called once
    /// per deployment; the returned string must be persisted in
    /// `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP` and NEVER rotated (rotating
    /// invalidates every existing envelope — see
    /// `docs/plan/opaque.md` §Phase 0).
    pub fn generate_server_setup_b64() -> String {
        use base64::Engine as _;
        let mut rng = OsRng;
        let setup = ServerSetup::<OxiCloudSuite>::new(&mut rng);
        base64::engine::general_purpose::STANDARD.encode(setup.serialize())
    }
}

fn decode_server_setup(b64: &str) -> Result<ServerSetup<OxiCloudSuite>, DomainError> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InternalError,
                "opaque",
                format!("OXICLOUD_AUTH_OPAQUE_SERVER_SETUP is not valid base64: {e}"),
            )
        })?;
    ServerSetup::<OxiCloudSuite>::deserialize(&bytes).map_err(|e| {
        DomainError::new(
            ErrorKind::InternalError,
            "opaque",
            format!(
                "OXICLOUD_AUTH_OPAQUE_SERVER_SETUP payload does not match ciphersuite v1: {e}. \
                 If you rotated the ciphersuite, every user must re-register."
            ),
        )
    })
}

/// Runtime OPAQUE mode. Drives whether the endpoints exist at all
/// (`Off`), run alongside the legacy password path (`Migrate`), or are
/// the only accepted mechanism for users with an envelope (`OpaqueOnly`).
///
/// Progression matches the phase plan — see `docs/plan/opaque.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueMode {
    /// Endpoints 404; no OPAQUE state ever mints. Default. Phase 0-1.
    Off,
    /// Endpoints live. Legacy login also accepted; successful legacy login
    /// silently mints an envelope. Phase 2-3.
    Migrate,
    /// Endpoints live. Legacy login refused for users with
    /// `opaque_migrated_at IS NOT NULL`. Phase 4+.
    OpaqueOnly,
}

impl OpaqueMode {
    /// Case-insensitive parse. Unknown token returns `None` so callers can
    /// log-and-default (mirrors [`crate::common::config::AuthMethod::parse`]).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "disabled" => Some(Self::Off),
            "migrate" => Some(Self::Migrate),
            "opaque_only" | "opaque-only" => Some(Self::OpaqueOnly),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_round_trip_server_setup() {
        // Fresh setup encodes to base64, decodes back into an equivalent
        // ServerSetup, and yields a usable OpaqueService when threaded
        // through the config layer.
        let b64 = OpaqueService::generate_server_setup_b64();
        assert!(!b64.is_empty(), "setup must be non-empty");
        let cfg = OpaqueConfig {
            mode: OpaqueMode::Migrate,
            server_setup_b64: Some(b64.clone()),
            ..OpaqueConfig::default()
        };
        let svc = OpaqueService::from_config(cfg).expect("service builds from valid config");
        assert_eq!(svc.mode(), OpaqueMode::Migrate);
        // Round-trip check: re-serialising the loaded setup produces the
        // same bytes as the generator emitted.
        use base64::Engine as _;
        let re_encoded = base64::engine::general_purpose::STANDARD.encode(svc.setup().serialize());
        assert_eq!(re_encoded, b64);
    }

    #[test]
    fn from_config_rejects_off_mode() {
        // Guard rail: explicitly refuses to build in Off mode so a stray
        // caller cannot accidentally exercise the primitives when the
        // operator has disabled OPAQUE.
        let cfg = OpaqueConfig {
            mode: OpaqueMode::Off,
            server_setup_b64: Some(OpaqueService::generate_server_setup_b64()),
            ..OpaqueConfig::default()
        };
        let err = OpaqueService::from_config(cfg).expect_err("must reject Off");
        assert_eq!(err.kind, ErrorKind::AccessDenied);
    }

    #[test]
    fn from_config_rejects_missing_setup() {
        // Enabling OPAQUE without persisting the setup is a boot-time
        // misconfiguration — surface it with a clear error rather than
        // silently generating a fresh (and unpersisted) keypair.
        let cfg = OpaqueConfig {
            mode: OpaqueMode::Migrate,
            server_setup_b64: None,
            ..OpaqueConfig::default()
        };
        let err = OpaqueService::from_config(cfg).expect_err("must reject missing setup");
        assert_eq!(err.kind, ErrorKind::InternalError);
        assert!(
            err.to_string()
                .contains("OXICLOUD_AUTH_OPAQUE_SERVER_SETUP")
        );
    }

    #[test]
    fn from_config_rejects_malformed_setup() {
        // Truncated / garbled base64 is caught at boot with a helpful
        // pointer to the ciphersuite-rotation caveat.
        let cfg = OpaqueConfig {
            mode: OpaqueMode::Migrate,
            server_setup_b64: Some("not-base64!".to_string()),
            ..OpaqueConfig::default()
        };
        let err = OpaqueService::from_config(cfg).expect_err("must reject malformed setup");
        assert_eq!(err.kind, ErrorKind::InternalError);
    }

    /// End-to-end round-trip through OPAQUE's four messages, using the
    /// ciphersuite this service actually binds. This is the load-bearing
    /// smoke test for Phase 0: proves the crate is wired correctly, the
    /// ServerSetup we serialise / deserialise is functional, and the client
    /// and server sides negotiate a matching session key given the correct
    /// passphrase (and disagree on the wrong one).
    ///
    /// Fast Argon2 params (8 KiB / 1 iter / 1 lane) keep the test in the
    /// millisecond range — production clients pass their own configured
    /// Argon2 instance via `ClientRegistrationFinishParameters` /
    /// `ClientLoginFinishParameters`, so the test's choice of KSF params
    /// does NOT contaminate the runtime behaviour of the service.
    #[test]
    fn round_trip_register_and_login_matches_session_keys() {
        use opaque_ke::{
            ClientLogin, ClientLoginFinishParameters, ClientRegistration,
            ClientRegistrationFinishParameters, ServerLogin, ServerLoginStartParameters,
            ServerRegistration,
        };
        use rand_core::OsRng;

        // ── Server bootstrap (mirrors the production `from_config` path) ─
        let b64 = OpaqueService::generate_server_setup_b64();
        let svc = OpaqueService::from_config(OpaqueConfig {
            mode: OpaqueMode::Migrate,
            server_setup_b64: Some(b64),
            ..OpaqueConfig::default()
        })
        .expect("service builds");

        // Test-scoped fast KSF — override the client-side Argon2 via the
        // finish-parameters plumb so we don't pay the 256 MiB / 3-iter
        // production defaults for every test run.
        let ksf = argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            argon2::Params::new(8, 1, 1, None).expect("valid test argon2 params"),
        );

        let user_id = b"alice@example.com";
        let passphrase = b"correct horse battery staple";

        // ── Registration ────────────────────────────────────────────────
        let mut client_rng = OsRng;
        let client_reg_start =
            ClientRegistration::<OxiCloudSuite>::start(&mut client_rng, passphrase)
                .expect("client registration start");
        let server_reg_start = ServerRegistration::<OxiCloudSuite>::start(
            svc.setup(),
            client_reg_start.message,
            user_id,
        )
        .expect("server registration start");
        let client_reg_finish = client_reg_start
            .state
            .finish(
                &mut client_rng,
                passphrase,
                server_reg_start.message,
                ClientRegistrationFinishParameters::new(
                    opaque_ke::Identifiers::default(),
                    Some(&ksf),
                ),
            )
            .expect("client registration finish");
        let password_file = ServerRegistration::<OxiCloudSuite>::finish(client_reg_finish.message);
        let password_file_bytes = password_file.serialize();

        // ── Login (correct passphrase → session keys match) ─────────────
        let client_login_start = ClientLogin::<OxiCloudSuite>::start(&mut client_rng, passphrase)
            .expect("client login start");
        let stored = ServerRegistration::<OxiCloudSuite>::deserialize(&password_file_bytes)
            .expect("password file deserialises");
        let mut server_rng = OsRng;
        let server_login_start = ServerLogin::start(
            &mut server_rng,
            svc.setup(),
            Some(stored),
            client_login_start.message,
            user_id,
            ServerLoginStartParameters::default(),
        )
        .expect("server login start");
        let client_login_finish = client_login_start
            .state
            .finish(
                passphrase,
                server_login_start.message,
                ClientLoginFinishParameters::new(
                    None,
                    opaque_ke::Identifiers::default(),
                    Some(&ksf),
                ),
            )
            .expect("client login finish");
        let server_login_finish = server_login_start
            .state
            .finish(client_login_finish.message)
            .expect("server login finish");
        assert_eq!(
            client_login_finish.session_key.as_slice(),
            server_login_finish.session_key.as_slice(),
            "OPAQUE session keys must match on both sides after a successful login"
        );
        assert!(
            !client_login_finish.export_key.as_slice().is_empty(),
            "client export_key must be populated (E2EE KEK bridge input)"
        );

        // ── Login (wrong passphrase → client finish must fail) ──────────
        let bad_login_start =
            ClientLogin::<OxiCloudSuite>::start(&mut client_rng, b"wrong-passphrase")
                .expect("client login start (wrong pass)");
        let stored_again =
            ServerRegistration::<OxiCloudSuite>::deserialize(&password_file_bytes).unwrap();
        let bad_server_login = ServerLogin::start(
            &mut server_rng,
            svc.setup(),
            Some(stored_again),
            bad_login_start.message,
            user_id,
            ServerLoginStartParameters::default(),
        )
        .expect("server login start (wrong pass)");
        let bad_client_finish = bad_login_start.state.finish(
            b"wrong-passphrase",
            bad_server_login.message,
            ClientLoginFinishParameters::new(None, opaque_ke::Identifiers::default(), Some(&ksf)),
        );
        assert!(
            bad_client_finish.is_err(),
            "client finish must reject a wrong passphrase — this is the whole point of OPAQUE"
        );
    }

    #[test]
    fn mode_parse_case_insensitive_and_alias_tolerant() {
        assert_eq!(OpaqueMode::parse("off"), Some(OpaqueMode::Off));
        assert_eq!(OpaqueMode::parse("OFF"), Some(OpaqueMode::Off));
        assert_eq!(OpaqueMode::parse("disabled"), Some(OpaqueMode::Off));
        assert_eq!(OpaqueMode::parse("migrate"), Some(OpaqueMode::Migrate));
        assert_eq!(
            OpaqueMode::parse("opaque_only"),
            Some(OpaqueMode::OpaqueOnly)
        );
        assert_eq!(
            OpaqueMode::parse("opaque-only"),
            Some(OpaqueMode::OpaqueOnly)
        );
        assert_eq!(OpaqueMode::parse("nope"), None);
    }
}
