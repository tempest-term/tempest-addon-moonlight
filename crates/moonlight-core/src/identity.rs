//! Moonlight client identity.
//!
//! A persistent self-signed RSA X.509 certificate + private key that identifies
//! this client to a GameStream/Sunshine host. It is:
//!   - sent (hex-encoded PEM) in the NVHTTP `/pair` handshake, and
//!   - presented as the TLS *client* certificate on every subsequent HTTPS call
//!     (`/serverinfo`, `/applist`, `/launch`, the `pairchallenge` phase).
//!
//! The host remembers this certificate after pairing, so the identity MUST be
//! generated once and persisted — re-generating it un-pairs the client. We
//! mirror what moonlight-qt's `IdentityManager` produces: RSA-2048, subject/
//! issuer `CN=NVIDIA GameStream Client`, SHA-256 self-signature, ~20-year
//! validity. The caller owns persistence (PEM bytes + the unique id).

use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::error::ErrorStack;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::x509::{X509, X509NameBuilder};

/// A Moonlight client identity. `cert_pem` / `key_pem` are PEM-encoded; the
/// caller persists all three fields (e.g. under the Tempest config dir) and
/// rebuilds via [`MoonlightIdentity::from_pem`] on the next launch.
#[derive(Debug, Clone)]
pub struct MoonlightIdentity {
    /// Stable per-client id sent as the `uniqueid` query param (16 hex chars).
    pub unique_id: String,
    /// Self-signed client certificate, PEM.
    pub cert_pem: Vec<u8>,
    /// Matching private key, PKCS#8 PEM.
    pub key_pem: Vec<u8>,
}

impl MoonlightIdentity {
    /// Generate a fresh identity. Call once, then persist + reload — see the
    /// module docs (re-generating un-pairs the client).
    pub fn generate() -> Result<Self, String> {
        Self::generate_inner().map_err(|e| format!("moonlight identity generation failed: {e}"))
    }

    fn generate_inner() -> Result<Self, ErrorStack> {
        let rsa = Rsa::generate(2048)?;
        let pkey = PKey::from_rsa(rsa)?;

        // Self-signed: subject == issuer, the exact CN moonlight-qt uses.
        let mut name = X509NameBuilder::new()?;
        name.append_entry_by_text("CN", "NVIDIA GameStream Client")?;
        let name = name.build();

        let mut builder = X509::builder()?;
        builder.set_version(2)?; // X.509 v3 (0-indexed)

        // Random positive serial.
        let serial = {
            let mut bn = BigNum::new()?;
            bn.rand(159, MsbOption::MAYBE_ZERO, false)?;
            bn.to_asn1_integer()?
        };
        builder.set_serial_number(&serial)?;
        builder.set_subject_name(&name)?;
        builder.set_issuer_name(&name)?;
        builder.set_pubkey(&pkey)?;
        let not_before = Asn1Time::days_from_now(0)?;
        let not_after = Asn1Time::days_from_now(365 * 20)?;
        builder.set_not_before(&not_before)?;
        builder.set_not_after(&not_after)?;
        builder.sign(&pkey, MessageDigest::sha256())?;
        let cert = builder.build();

        let cert_pem = cert.to_pem()?;
        let key_pem = pkey.private_key_to_pem_pkcs8()?;

        // 8 random bytes → 16 hex chars.
        let mut id = [0u8; 8];
        openssl::rand::rand_bytes(&mut id)?;
        let unique_id = id.iter().map(|b| format!("{b:02x}")).collect();

        Ok(Self {
            unique_id,
            cert_pem,
            key_pem,
        })
    }

    /// Rebuild a persisted identity from its stored PEM bytes + unique id.
    pub fn from_pem(unique_id: String, cert_pem: Vec<u8>, key_pem: Vec<u8>) -> Self {
        Self {
            unique_id,
            cert_pem,
            key_pem,
        }
    }

    /// Parse the certificate (for the TLS client identity + signature bytes).
    pub fn certificate(&self) -> Result<X509, String> {
        X509::from_pem(&self.cert_pem).map_err(|e| format!("bad client cert PEM: {e}"))
    }

    /// Parse the private key (for RSA signing during pairing).
    pub fn private_key(&self) -> Result<PKey<Private>, String> {
        PKey::private_key_from_pem(&self.key_pem).map_err(|e| format!("bad client key PEM: {e}"))
    }

    /// The certificate's PEM bytes as an uppercase hex string — the wire form
    /// of the `clientcert` query parameter in the `/pair` handshake.
    pub fn cert_hex(&self) -> String {
        self.cert_pem.iter().map(|b| format!("{b:02X}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_usable_self_signed_identity() {
        let id = MoonlightIdentity::generate().expect("generate");
        assert_eq!(id.unique_id.len(), 16, "unique_id should be 16 hex chars");

        // Round-trips through PEM into a real cert + key.
        let cert = id.certificate().expect("parse cert");
        let _key = id.private_key().expect("parse key");

        // Self-signed: subject == issuer, CN as moonlight-qt expects.
        let subject = cert.subject_name();
        let cn = subject
            .entries()
            .next()
            .map(|e| String::from_utf8_lossy(e.data().as_slice()).into_owned());
        assert_eq!(cn.as_deref(), Some("NVIDIA GameStream Client"));

        // The wire form is non-empty uppercase hex of the PEM.
        assert!(id.cert_hex().starts_with("2D2D2D2D2D")); // "-----" in hex
    }

    #[test]
    fn from_pem_round_trips() {
        let id = MoonlightIdentity::generate().expect("generate");
        let rebuilt =
            MoonlightIdentity::from_pem(id.unique_id.clone(), id.cert_pem.clone(), id.key_pem.clone());
        assert!(rebuilt.certificate().is_ok());
        assert_eq!(rebuilt.unique_id, id.unique_id);
    }
}
