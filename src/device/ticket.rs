//! KVRPC/1 tunnel tickets: the pinned key and the verifier.
//!
//! A port of `verifyTicket` in the Node Runtime's `rpcTunnel.ts`, to the letter
//! of `docs/specs/RPC Tunnel Protocol.md` in the platform repository. The checks
//! run in the spec's order and each fails closed. The reason a check failed is
//! for this machine's log only; the wire never says more than "refused".
//!
//! Only the public key is ever on a device. A compromised device can verify
//! tickets and cannot mint them.

use anyhow::{anyhow, Context, Result};
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::DecodePaddingMode;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::net::IpAddr;

/// Clock skew tolerated on `iat` and `exp`, in seconds.
pub const CLOCK_SKEW_S: i64 = 60;

/// The DER prefix of an Ed25519 SubjectPublicKeyInfo: the algorithm identifier
/// (1.3.101.112) and the bit-string header. The 32-byte key follows it.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// base64url, with or without padding. Node's decoder is lenient about padding,
/// and a ticket the reference gateway accepts must not be refused here for it.
const B64URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The control plane's ticket key, as pinned at enrolment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedKey {
    /// First 16 hex characters of SHA-256 over the DER SPKI.
    pub key_id: String,
    raw: [u8; 32],
}

impl PinnedKey {
    /// Parse a PEM SPKI Ed25519 public key, as `ticketKey.publicKeyPem` carries it.
    pub fn from_pem(pem: &str) -> Result<Self> {
        let body: String = pem
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("-----"))
            .collect();
        if !pem.contains("-----BEGIN PUBLIC KEY-----") {
            return Err(anyhow!("the ticket key is not a PEM public key"));
        }
        let der = STANDARD
            .decode(body.as_bytes())
            .context("the ticket key is not valid base64")?;
        Self::from_spki_der(&der)
    }

    fn from_spki_der(der: &[u8]) -> Result<Self> {
        if der.len() != ED25519_SPKI_PREFIX.len() + 32 || der[..12] != ED25519_SPKI_PREFIX {
            return Err(anyhow!("the ticket key is not an Ed25519 public key"));
        }
        let mut raw = [0u8; 32];
        raw.copy_from_slice(&der[12..]);
        let digest = Sha256::digest(der);
        let key_id = hex(&digest)[..16].to_string();
        Ok(Self { key_id, raw })
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &self.raw)
            .verify(message, signature)
            .is_ok()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why a ticket was refused. The names are the spec's, so this machine's log
/// and the reference gateway's read the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Malformed,
    BadSignature,
    UnsupportedVersion,
    UnknownKey,
    WrongDevice,
    Expired,
    NotYetValid,
    WrongPeer,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::Malformed => "malformed",
            Refusal::BadSignature => "bad_signature",
            Refusal::UnsupportedVersion => "unsupported_version",
            Refusal::UnknownKey => "unknown_key",
            Refusal::WrongDevice => "wrong_device",
            Refusal::Expired => "expired",
            Refusal::NotYetValid => "not_yet_valid",
            Refusal::WrongPeer => "wrong_peer",
        }
    }
}

/// The claims this machine logs about an accepted connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub pool: String,
    pub coord: String,
    pub plan_version: Option<i64>,
}

/// Strip the IPv4-mapped prefix so `::ffff:100.64.0.1` compares as `100.64.0.1`.
pub fn normalize_address(address: &str) -> String {
    let lower = address.trim().to_ascii_lowercase();
    match lower.strip_prefix("::ffff:") {
        Some(rest) if !rest.contains(':') => rest.to_string(),
        _ => lower,
    }
}

/// The address a socket's peer presents, normalized the same way.
pub fn normalize_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(),
        },
        IpAddr::V4(v4) => v4.to_string(),
    }
}

/// Verify a ticket for this device, in the spec's order.
pub fn verify_ticket(
    ticket: &str,
    key: &PinnedKey,
    device_id: &str,
    remote_address: &str,
    now_unix: i64,
) -> std::result::Result<Accepted, Refusal> {
    // 1. Exactly one dot, both halves non-empty.
    let mut parts = ticket.split('.');
    let (payload, signature) = match (parts.next(), parts.next(), parts.next()) {
        (Some(p), Some(s), None) if !p.is_empty() && !s.is_empty() => (p, s),
        _ => return Err(Refusal::Malformed),
    };

    // 2. Ed25519 over the ASCII bytes of the base64url payload, not the JSON.
    let valid = match B64URL.decode(signature.as_bytes()) {
        Ok(sig) => key.verify(payload.as_bytes(), &sig),
        Err(_) => false,
    };
    if !valid {
        return Err(Refusal::BadSignature);
    }

    // 3. The payload is JSON.
    let claims: Value = B64URL
        .decode(payload.as_bytes())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(Refusal::Malformed)?;

    // 4 to 6. Version, key, device.
    if claims.get("v").and_then(Value::as_f64) != Some(1.0) {
        return Err(Refusal::UnsupportedVersion);
    }
    if claims.get("kid").and_then(Value::as_str) != Some(key.key_id.as_str()) {
        return Err(Refusal::UnknownKey);
    }
    if claims.get("dev").and_then(Value::as_str) != Some(device_id) {
        return Err(Refusal::WrongDevice);
    }

    // 7. Expiry and issue time, each with the skew allowance.
    let now = now_unix as f64;
    let skew = CLOCK_SKEW_S as f64;
    match claims.get("exp").and_then(Value::as_f64) {
        Some(exp) if exp + skew >= now => {}
        _ => return Err(Refusal::Expired),
    }
    match claims.get("iat").and_then(Value::as_f64) {
        Some(iat) if iat - skew <= now => {}
        _ => return Err(Refusal::NotYetValid),
    }

    // 8. When the ticket names a peer, the connection must come from it.
    if let Some(peer) = claims.get("peer").and_then(Value::as_str) {
        if !peer.is_empty() && normalize_address(remote_address) != normalize_address(peer) {
            return Err(Refusal::WrongPeer);
        }
    }

    Ok(Accepted {
        pool: claims
            .get("pool")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        coord: claims
            .get("coord")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        plan_version: claims.get("ver").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform's vectors (`docs/specs/RPC Tunnel Test Vectors.json`),
    /// vendored byte for byte. The checksum pins the exact file, so a copy
    /// edited here instead of regenerated there fails loudly. When the platform
    /// regenerates them, copy the file over and update the checksum.
    const VECTORS: &str = include_str!("../../tests/fixtures/rpc-tunnel-test-vectors.json");
    const VECTORS_SHA256: &str = "4ccfc0e7e3b136921969f0bf5db533bcad8a52a2dc30010ddda9d31c59f5d7b2";

    #[test]
    fn the_vendored_vectors_are_the_ones_pinned() {
        assert_eq!(hex(&Sha256::digest(VECTORS.as_bytes())), VECTORS_SHA256);
    }

    #[test]
    fn every_conformance_vector_gets_its_verdict() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let key = PinnedKey::from_pem(v["publicKeyPem"].as_str().unwrap()).unwrap();
        assert_eq!(
            key.key_id,
            v["keyId"].as_str().unwrap(),
            "key id derivation"
        );
        let device = v["deviceId"].as_str().unwrap();
        let now = v["verifyAtUnixSeconds"].as_i64().unwrap();
        let cases = v["cases"].as_array().unwrap();
        assert!(cases.len() >= 13);
        for case in cases {
            let got = match verify_ticket(
                case["ticket"].as_str().unwrap(),
                &key,
                device,
                case["remoteAddress"].as_str().unwrap(),
                now,
            ) {
                Ok(_) => "ok",
                Err(r) => r.as_str(),
            };
            assert_eq!(
                got,
                case["expect"].as_str().unwrap(),
                "case: {}",
                case["name"]
            );
        }
    }

    #[test]
    fn an_accepted_ticket_reports_its_pool_and_coordinator() {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let key = PinnedKey::from_pem(v["publicKeyPem"].as_str().unwrap()).unwrap();
        let first = &v["cases"][0];
        let got = verify_ticket(
            first["ticket"].as_str().unwrap(),
            &key,
            v["deviceId"].as_str().unwrap(),
            "100.100.1.1",
            v["verifyAtUnixSeconds"].as_i64().unwrap(),
        )
        .unwrap();
        assert_eq!(got.pool, "11111111-2222-4333-8444-555555555555");
        assert_eq!(got.coord, "55555555-5555-4555-8555-555555555555");
        assert_eq!(got.plan_version, Some(7));
    }

    #[test]
    fn a_key_that_is_not_ed25519_is_refused() {
        // An RSA-shaped SPKI header is not the Ed25519 one.
        assert!(PinnedKey::from_pem("-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE\n-----END PUBLIC KEY-----\n").is_err());
        assert!(PinnedKey::from_pem("not a key").is_err());
    }

    #[test]
    fn mapped_addresses_compare_as_their_ipv4_form() {
        assert_eq!(normalize_address("::ffff:100.64.0.1"), "100.64.0.1");
        assert_eq!(normalize_address("::FFFF:100.64.0.1"), "100.64.0.1");
        assert_eq!(normalize_address("fd7a:115c:a1e0::1"), "fd7a:115c:a1e0::1");
        let mapped: IpAddr = "::ffff:100.64.0.1".parse().unwrap();
        assert_eq!(normalize_ip(mapped), "100.64.0.1");
    }
}
