//! Device identity: an Ed25519 keypair generated on first run (stored in the
//! encrypted secret store by the daemon; here we hold it in memory). Signs the
//! WS handshake exactly as the server verifies it (§9.1 server).
//!
//! Also holds the other direction: verifying the gateway's quota grants against
//! a key **pinned into this build**, so a publisher only ever burns its own
//! subscription quota on work the platform actually authorized and will pay
//! for. See [`pinned_quota_verifier`] for why the key cannot come from the
//! connection it is meant to authenticate.

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A device signing identity.
pub struct DeviceIdentity {
    signing: SigningKey,
}

impl DeviceIdentity {
    /// Generate a fresh identity (first run).
    pub fn generate() -> DeviceIdentity {
        use rand::RngCore;
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        DeviceIdentity {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// Restore from a stored 32-byte seed (base64).
    pub fn from_seed_b64(seed_b64: &str) -> anyhow::Result<DeviceIdentity> {
        let bytes = B64.decode(seed_b64)?;
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| anyhow::anyhow!("seed must be 32 bytes"))?;
        Ok(DeviceIdentity {
            signing: SigningKey::from_bytes(&arr),
        })
    }

    /// Export the seed for keychain persistence (base64).
    pub fn seed_b64(&self) -> String {
        B64.encode(self.signing.to_bytes())
    }

    /// The public key (base64) to register with the server.
    pub fn public_key_b64(&self) -> String {
        B64.encode(self.verifying_key().to_bytes())
    }

    fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// Sign the handshake; returns base64.
    ///
    /// `audience` is the gateway host this client means to reach and
    /// `challenge` is the one-shot value that gateway just issued — see
    /// [`asale_protocol::frame::handshake_signing_body`] for why both are in
    /// the signature.
    pub fn sign_handshake(
        &self,
        device_id: &str,
        ts_ms: i64,
        nonce: &str,
        audience: &str,
        challenge: &str,
    ) -> String {
        let msg = asale_protocol::frame::handshake_signing_body(device_id, ts_ms, nonce, audience, challenge);
        let sig = self.signing.sign(msg.as_bytes());
        B64.encode(sig.to_bytes())
    }
}

/// Verifies the gateway's quota grants. Built from the pinned key — never from
/// anything a peer sent on the connection being checked.
#[derive(Debug, Clone)]
pub struct QuotaVerifier {
    key: VerifyingKey,
}

/// Why a quota grant was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaError {
    /// The signature does not match the grant body under the server's key.
    BadSignature,
    /// The grant's expiry has passed (a replayed old dispatch).
    Expired,
    /// The frame carries no model/exp, so the signature body cannot be rebuilt.
    Unverifiable,
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            QuotaError::BadSignature => "quota grant signature does not verify",
            QuotaError::Expired => "quota grant has expired",
            QuotaError::Unverifiable => "quota grant is missing model/exp",
        })
    }
}

/// Environment variable naming the gateway's quota-grant public key, read at
/// **compile time** by the release pipeline and at run time by a self-built
/// client pointing at its own gateway.
pub const QUOTA_PUBKEY_ENV: &str = "ASALE_QUOTA_PUBKEY";

/// The gateway public key this build trusts, or `None` if it was never pinned.
///
/// Resolution order: the value baked in at compile time, then the same variable
/// in the environment. Deliberately **not** the key the gateway offers in
/// `hello.ack` — that key arrives over the very connection it is supposed to
/// vouch for, so anyone able to sit in the middle could hand over their own and
/// sign whatever grants they liked. A pin is only worth something if its source
/// is outside the channel being authenticated.
///
/// The compile-time value is supplied by the packaging step and is not in the
/// open-source tree. (It is a *public* key, so keeping it out of the repo is
/// not what provides the security — pinning it is. Anyone can read it out of a
/// shipped binary, and that is fine: knowing the key does not let you sign with
/// it.)
pub fn pinned_quota_pubkey() -> Option<String> {
    option_env!("ASALE_QUOTA_PUBKEY")
        .map(str::to_string)
        .or_else(|| std::env::var(QUOTA_PUBKEY_ENV).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The verifier this build will use, built from [`pinned_quota_pubkey`].
///
/// `Err` when no key is pinned: the publisher must then refuse to serve rather
/// than fall back to trusting the gateway, because "we could not check the
/// grant" and "the grant is good" are not the same answer, and only one of them
/// is safe to act on when acting means spending the user's own subscription.
pub fn pinned_quota_verifier() -> anyhow::Result<QuotaVerifier> {
    let key = pinned_quota_pubkey().ok_or_else(|| {
        anyhow::anyhow!(
            "no gateway quota public key is pinned in this build — set {QUOTA_PUBKEY_ENV} \
             at build time (release packaging) or in the environment. Without it a quota \
             grant cannot be verified, and this device will not sell."
        )
    })?;
    QuotaVerifier::from_pubkey_b64(&key)
        .map_err(|e| anyhow::anyhow!("pinned {QUOTA_PUBKEY_ENV} is not a valid Ed25519 public key: {e}"))
}

impl QuotaVerifier {
    /// Build from a base64 Ed25519 public key.
    pub fn from_pubkey_b64(pubkey_b64: &str) -> anyhow::Result<QuotaVerifier> {
        let bytes = B64.decode(pubkey_b64)?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("quota pubkey must be 32 bytes"))?;
        Ok(QuotaVerifier {
            key: VerifyingKey::from_bytes(&arr)?,
        })
    }

    /// Check a grant. `now_secs` is unix time; the signed body is
    /// `task_id|model|budget_tokens|exp`, matching the server's `sign_quota`.
    pub fn verify(
        &self,
        task_id: &str,
        model: &str,
        budget_tokens: i64,
        exp: i64,
        sig_b64: &str,
        now_secs: i64,
    ) -> Result<(), QuotaError> {
        if model.is_empty() || exp <= 0 {
            return Err(QuotaError::Unverifiable);
        }
        if exp < now_secs {
            return Err(QuotaError::Expired);
        }
        let sig_bytes = B64.decode(sig_b64).map_err(|_| QuotaError::BadSignature)?;
        let sig = Signature::from_slice(&sig_bytes).map_err(|_| QuotaError::BadSignature)?;
        let msg = format!("{task_id}|{model}|{budget_tokens}|{exp}");
        self.key
            .verify(msg.as_bytes(), &sig)
            .map_err(|_| QuotaError::BadSignature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;

    /// Mirrors the server's `Security::sign_quota`.
    fn sign_quota(sk: &SigningKey, task_id: &str, model: &str, budget: i64, exp: i64) -> String {
        let msg = format!("{task_id}|{model}|{budget}|{exp}");
        B64.encode(sk.sign(msg.as_bytes()).to_bytes())
    }

    #[test]
    fn quota_grant_verifies_against_the_server_key() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let v = QuotaVerifier::from_pubkey_b64(&B64.encode(sk.verifying_key().to_bytes())).unwrap();
        let sig = sign_quota(&sk, "t_1", "claude-fable-5", 8192, 2_000);
        assert_eq!(v.verify("t_1", "claude-fable-5", 8192, 2_000, &sig, 1_000), Ok(()));
    }

    #[test]
    fn quota_grant_rejects_tampering_replay_and_forgery() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let v = QuotaVerifier::from_pubkey_b64(&B64.encode(sk.verifying_key().to_bytes())).unwrap();
        let sig = sign_quota(&sk, "t_1", "claude-fable-5", 8192, 2_000);

        // A budget raised after signing must not verify.
        assert_eq!(
            v.verify("t_1", "claude-fable-5", 999_999, 2_000, &sig, 1_000),
            Err(QuotaError::BadSignature)
        );
        // Neither may the grant be reused for another task.
        assert_eq!(
            v.verify("t_2", "claude-fable-5", 8192, 2_000, &sig, 1_000),
            Err(QuotaError::BadSignature)
        );
        // An expired grant is a replay of an old dispatch.
        assert_eq!(
            v.verify("t_1", "claude-fable-5", 8192, 2_000, &sig, 3_000),
            Err(QuotaError::Expired)
        );
        // A grant signed by anyone else is worthless.
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let forged = sign_quota(&other, "t_1", "claude-fable-5", 8192, 2_000);
        assert_eq!(
            v.verify("t_1", "claude-fable-5", 8192, 2_000, &forged, 1_000),
            Err(QuotaError::BadSignature)
        );
    }

    #[test]
    fn grants_without_model_or_exp_are_unverifiable() {
        let sk = SigningKey::from_bytes(&[3u8; 32]);
        let v = QuotaVerifier::from_pubkey_b64(&B64.encode(sk.verifying_key().to_bytes())).unwrap();
        let sig = sign_quota(&sk, "t_1", "", 8192, 0);
        assert_eq!(v.verify("t_1", "", 8192, 0, &sig, 1_000), Err(QuotaError::Unverifiable));
    }

    #[test]
    fn handshake_signature_verifies() {
        // This mirrors the server's verify_device_sig: reconstruct the message,
        // decode the stored pubkey, and verify — proving client↔server compat.
        let id = DeviceIdentity::generate();
        let device_id = "dev-1";
        let ts = 1_720_000_000_000i64;
        let nonce = "n1";
        let sig_b64 = id.sign_handshake(device_id, ts, nonce, "gw.asale.app", "chal-1");

        let pk_bytes = B64.decode(id.public_key_b64()).unwrap();
        let vk = VerifyingKey::from_bytes(&pk_bytes.try_into().unwrap()).unwrap();
        let sig = Signature::from_slice(&B64.decode(&sig_b64).unwrap()).unwrap();
        let msg = asale_protocol::frame::handshake_signing_body(
            device_id, ts, nonce, "gw.asale.app", "chal-1",
        );
        assert!(vk.verify(msg.as_bytes(), &sig).is_ok());

        // The same signature must not verify for a different gateway: that is
        // what stops one gateway from forwarding a publisher's handshake to
        // another and borrowing its identity.
        let elsewhere = asale_protocol::frame::handshake_signing_body(
            device_id, ts, nonce, "evil.example", "chal-1",
        );
        assert!(vk.verify(elsewhere.as_bytes(), &sig).is_err());
        // Nor for a different challenge, which is what makes a captured
        // handshake single-use.
        let replayed = asale_protocol::frame::handshake_signing_body(
            device_id, ts, nonce, "gw.asale.app", "chal-2",
        );
        assert!(vk.verify(replayed.as_bytes(), &sig).is_err());
    }

    #[test]
    fn an_unpinned_build_refuses_to_verify_rather_than_trusting_the_gateway() {
        // No key pinned at compile time and none in the environment: the only
        // safe answer is "cannot verify", never "assume it is fine". A device
        // that reached the serving path here would be spending its owner's
        // subscription on grants nobody checked.
        if option_env!("ASALE_QUOTA_PUBKEY").is_some() {
            return; // a packaged build pins one; nothing to assert
        }
        let prev = std::env::var(QUOTA_PUBKEY_ENV).ok();
        std::env::remove_var(QUOTA_PUBKEY_ENV);
        assert!(pinned_quota_pubkey().is_none());
        let err = pinned_quota_verifier().unwrap_err().to_string();
        assert!(err.contains("will not sell"), "unhelpful message: {err}");
        if let Some(p) = prev {
            std::env::set_var(QUOTA_PUBKEY_ENV, p);
        }
    }

    #[test]
    fn seed_roundtrip() {
        let id = DeviceIdentity::generate();
        let seed = id.seed_b64();
        let restored = DeviceIdentity::from_seed_b64(&seed).unwrap();
        assert_eq!(id.public_key_b64(), restored.public_key_b64());
    }
}
