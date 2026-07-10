// SCRAM message strings are RFC 5802 fragments — `AuthMessage`,
// `c=biws`, `r=...,s=...,i=...` etc. clippy::doc_markdown wants
// every such token backticked; not enforced here.
#![allow(clippy::doc_markdown, clippy::uninlined_format_args)]

//! v4.8 SCRAM-SHA-256 server-side state machine per RFC 5802 +
//! PG's SASL framing.
//!
//! Flow:
//! 1. Server sends AuthenticationSASL ('R' subtype 10) advertising
//!    the SCRAM-SHA-256 mechanism.
//! 2. Client sends SASLInitialResponse ('p') carrying client-first.
//! 3. Server sends AuthenticationSASLContinue ('R' subtype 11) with
//!    server-first (combined nonce + base64 salt + iters).
//! 4. Client sends SASLResponse ('p') with client-final (channel
//!    binding token + combined nonce + base64 client-proof).
//! 5. Server verifies the proof; sends AuthenticationSASLFinal
//!    ('R' subtype 12) carrying the server signature; then
//!    AuthenticationOk ('R' subtype 0).
//!
//! Channel binding: over TLS we advertise SCRAM-SHA-256-PLUS and bind
//! to `tls-server-end-point` (RFC 5929) — the GS2 header is then
//! `p=tls-server-end-point,,` and the client-final `c=` carries the
//! cert hash. Without TLS (or a non-SHA-256 cert) we advertise only
//! SCRAM-SHA-256 and the GS2 header is `n,,` / `y,,`.

use spg_crypto::{base64, hmac, sha256};
use spg_engine::ScramSecrets;

#[derive(Debug)]
#[allow(dead_code)] // NonceMismatch is reachable via the nonce-check arm in the helper
pub enum ScramError {
    BadInitial(String),
    BadFinal(String),
    NonceMismatch,
    ProofMismatch,
}

impl core::fmt::Display for ScramError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadInitial(s) => write!(f, "SCRAM: bad client-first ({s})"),
            Self::BadFinal(s) => write!(f, "SCRAM: bad client-final ({s})"),
            Self::NonceMismatch => f.write_str("SCRAM: server nonce mismatch"),
            Self::ProofMismatch => f.write_str("SCRAM: invalid client proof"),
        }
    }
}

/// The GS2 channel-binding flag from the client-first GS2 header.
#[derive(Debug, PartialEq, Eq)]
pub enum Gs2CbindFlag {
    /// `n` — the client does not support channel binding.
    NotSupported,
    /// `y` — the client supports channel binding but believes the
    /// server did not advertise a `-PLUS` mechanism. If the server
    /// *did* advertise it, that's a downgrade attack → reject.
    SupportedNotUsed,
    /// `p=tls-server-end-point` — the client is binding this SCRAM
    /// exchange to the TLS channel.
    Required,
}

/// What's parsed out of client-first-message.
#[derive(Debug)]
pub struct ClientFirst {
    /// The GS2 header verbatim, e.g. `n,,` or `p=tls-server-end-point,,`.
    /// Needed to reconstruct the expected client-final `c=` value.
    pub gs2_header: String,
    pub cbind_flag: Gs2CbindFlag,
    /// The portion the spec calls "client-first-message-bare" —
    /// `n=user,r=clientNonce`. Needed verbatim for the AuthMessage.
    pub bare: String,
    pub client_nonce: String,
}

/// The only channel-binding type we support (RFC 5929).
pub const CB_NAME: &str = "tls-server-end-point";

pub fn parse_client_first(msg: &str) -> Result<ClientFirst, ScramError> {
    // gs2-header = gs2-cbind-flag "," [ authzid ] "," — the flag is one of
    // "n" / "y" / "p=<cb-name>". Neither a cb-name nor an authzid may contain
    // an unescaped comma, so the first two commas delimit the header.
    let c1 = msg
        .find(',')
        .ok_or_else(|| ScramError::BadInitial("gs2 header missing first comma".into()))?;
    let c2 = msg[c1 + 1..]
        .find(',')
        .map(|i| c1 + 1 + i)
        .ok_or_else(|| ScramError::BadInitial("gs2 header missing authzid terminator".into()))?;
    let gs2_header = msg[..=c2].to_string();
    let flag_str = &msg[..c1];
    let cbind_flag = if flag_str == "n" {
        Gs2CbindFlag::NotSupported
    } else if flag_str == "y" {
        Gs2CbindFlag::SupportedNotUsed
    } else if let Some(name) = flag_str.strip_prefix("p=") {
        if name != CB_NAME {
            return Err(ScramError::BadInitial(format!(
                "unsupported channel binding {name:?}"
            )));
        }
        Gs2CbindFlag::Required
    } else {
        return Err(ScramError::BadInitial(format!(
            "unrecognized gs2 cbind flag {flag_str:?}"
        )));
    };
    let bare = msg[c2 + 1..].to_string();
    // bare = "n=user,r=nonce[,...]"
    let mut client_nonce = None;
    for attr in bare.split(',') {
        if let Some(rest) = attr.strip_prefix("r=") {
            client_nonce = Some(rest.to_string());
        }
    }
    let client_nonce = client_nonce
        .ok_or_else(|| ScramError::BadInitial("missing r= (client nonce) attribute".into()))?;
    Ok(ClientFirst {
        gs2_header,
        cbind_flag,
        bare,
        client_nonce,
    })
}

/// The expected client-final `c=` value: base64(gs2-header || cbind-data).
/// For a `-PLUS` exchange `cbind_data` is the `tls-server-end-point` hash;
/// for a non-PLUS exchange it is empty (so `n,,` → `biws`).
pub fn channel_binding_c_value(gs2_header: &str, cbind_data: &[u8]) -> String {
    let mut buf = gs2_header.as_bytes().to_vec();
    buf.extend_from_slice(cbind_data);
    base64::encode(&buf)
}

/// Build server-first-message: `r=combinedNonce,s=base64Salt,i=iters`.
pub fn build_server_first(combined_nonce: &str, secrets: &ScramSecrets) -> String {
    let salt_b64 = base64::encode(&secrets.salt);
    format!("r={combined_nonce},s={salt_b64},i={}", secrets.iters)
}

/// What's parsed out of client-final-message.
#[derive(Debug)]
pub struct ClientFinal {
    /// The portion the spec calls "client-final-message-without-proof"
    /// — `c=biws,r=combinedNonce`. Needed verbatim for the
    /// AuthMessage.
    pub without_proof: String,
    /// The raw base64 value of the `c=` (channel-binding) attribute —
    /// validated against `channel_binding_c_value`.
    pub channel_binding: String,
    pub combined_nonce: String,
    pub client_proof: [u8; sha256::OUT_LEN],
}

pub fn parse_client_final(msg: &str) -> Result<ClientFinal, ScramError> {
    // msg = "c=...,r=...,p=<proof>"
    // Split off "p=..." at the last comma. The rest is without_proof.
    let p_idx = msg
        .rfind(",p=")
        .ok_or_else(|| ScramError::BadFinal("missing p= (proof) attribute".into()))?;
    let without_proof = msg[..p_idx].to_string();
    let proof_b64 = &msg[p_idx + 3..];
    let decoded = base64::decode(proof_b64)
        .map_err(|_| ScramError::BadFinal("proof not valid base64".into()))?;
    if decoded.len() != sha256::OUT_LEN {
        return Err(ScramError::BadFinal(format!(
            "proof length {} ≠ expected {}",
            decoded.len(),
            sha256::OUT_LEN
        )));
    }
    let mut client_proof = [0u8; sha256::OUT_LEN];
    client_proof.copy_from_slice(&decoded);
    let mut combined_nonce = None;
    let mut channel_binding = None;
    for attr in without_proof.split(',') {
        if let Some(rest) = attr.strip_prefix("r=") {
            combined_nonce = Some(rest.to_string());
        } else if let Some(rest) = attr.strip_prefix("c=") {
            channel_binding = Some(rest.to_string());
        }
    }
    let combined_nonce = combined_nonce
        .ok_or_else(|| ScramError::BadFinal("missing r= attribute in without-proof".into()))?;
    let channel_binding = channel_binding
        .ok_or_else(|| ScramError::BadFinal("missing c= attribute in without-proof".into()))?;
    Ok(ClientFinal {
        without_proof,
        channel_binding,
        combined_nonce,
        client_proof,
    })
}

/// Verify the client's proof and return the base64-encoded server
/// signature to be sent in SASLFinal. The AuthMessage construction
/// is RFC 5802 §3:
///
///   AuthMessage = client-first-bare + "," +
///                 server-first        + "," +
///                 client-final-without-proof
///
///   ClientSignature = HMAC(StoredKey, AuthMessage)
///   ClientKey       = ClientProof XOR ClientSignature
///   Verify          : SHA-256(ClientKey) == StoredKey
///   ServerSignature = HMAC(ServerKey, AuthMessage)
pub fn verify_and_sign(
    secrets: &ScramSecrets,
    client_first_bare: &str,
    server_first: &str,
    client_final_without_proof: &str,
    client_proof: &[u8; sha256::OUT_LEN],
) -> Result<String, ScramError> {
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");
    let client_signature = hmac::hmac_sha256(&secrets.stored_key, auth_message.as_bytes());
    let mut client_key = [0u8; sha256::OUT_LEN];
    for i in 0..sha256::OUT_LEN {
        client_key[i] = client_proof[i] ^ client_signature[i];
    }
    let computed_stored = sha256::hash(&client_key);
    if !constant_time_eq(&computed_stored, &secrets.stored_key) {
        return Err(ScramError::ProofMismatch);
    }
    let server_signature = hmac::hmac_sha256(&secrets.server_key, auth_message.as_bytes());
    Ok(format!("v={}", base64::encode(&server_signature)))
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff: u8 = 0;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use spg_engine::users::compute_scram_secrets;

    #[test]
    fn parse_client_first_extracts_bare_and_nonce() {
        let cf = parse_client_first("n,,n=alice,r=clientnonce123").unwrap();
        assert_eq!(cf.gs2_header, "n,,");
        assert_eq!(cf.cbind_flag, Gs2CbindFlag::NotSupported);
        assert_eq!(cf.bare, "n=alice,r=clientnonce123");
        assert_eq!(cf.client_nonce, "clientnonce123");
    }

    #[test]
    fn parse_client_first_y_flag() {
        let cf = parse_client_first("y,,n=bob,r=nonce").unwrap();
        assert_eq!(cf.gs2_header, "y,,");
        assert_eq!(cf.cbind_flag, Gs2CbindFlag::SupportedNotUsed);
        assert_eq!(cf.bare, "n=bob,r=nonce");
    }

    #[test]
    fn parse_client_first_tls_server_end_point() {
        let cf = parse_client_first("p=tls-server-end-point,,n=eve,r=xyz").unwrap();
        assert_eq!(cf.gs2_header, "p=tls-server-end-point,,");
        assert_eq!(cf.cbind_flag, Gs2CbindFlag::Required);
        assert_eq!(cf.bare, "n=eve,r=xyz");
        assert_eq!(cf.client_nonce, "xyz");
    }

    #[test]
    fn parse_client_first_rejects_unknown_channel_binding() {
        // A binding type we don't implement (`tls-unique`) is rejected.
        let err = parse_client_first("p=tls-unique,,n=alice,r=nonce").unwrap_err();
        assert!(matches!(err, ScramError::BadInitial(_)));
    }

    #[test]
    fn channel_binding_c_value_matches_biws_for_no_binding() {
        // base64("n,,") == "biws" — the classic non-PLUS c= value.
        assert_eq!(channel_binding_c_value("n,,", &[]), "biws");
    }

    #[test]
    fn channel_binding_c_value_includes_cbind_data() {
        // For -PLUS the decoded c= is the GS2 header followed by the cert hash.
        let hash = [0xABu8; 32];
        let c = channel_binding_c_value("p=tls-server-end-point,,", &hash);
        let decoded = base64::decode(&c).unwrap();
        assert_eq!(&decoded[..24], b"p=tls-server-end-point,,");
        assert_eq!(&decoded[24..], &hash);
    }

    #[test]
    fn parse_client_final_round_trip() {
        let proof = [7u8; 32];
        let proof_b64 = base64::encode(&proof);
        let msg = format!("c=biws,r=combined,p={proof_b64}");
        let cf = parse_client_final(&msg).unwrap();
        assert_eq!(cf.without_proof, "c=biws,r=combined");
        assert_eq!(cf.channel_binding, "biws");
        assert_eq!(cf.combined_nonce, "combined");
        assert_eq!(cf.client_proof, proof);
    }

    #[test]
    fn full_exchange_round_trip() {
        // Server side: pretend we already stored these for user
        // "alice" with password "hunter2".
        let salt = [11u8; 16];
        let secrets = compute_scram_secrets("hunter2", salt, 4096);
        let client_nonce = "client-noncenoncenonce";
        let server_nonce = "server-noncenoncenonce";
        let combined_nonce = format!("{client_nonce}{server_nonce}");

        let client_first_bare = format!("n=alice,r={client_nonce}");
        let server_first = build_server_first(&combined_nonce, &secrets);
        let client_final_without_proof = format!("c=biws,r={combined_nonce}");
        let auth_message =
            format!("{client_first_bare},{server_first},{client_final_without_proof}");

        // Client computes proof the way RFC 5802 §3 says it does:
        let salted = spg_crypto::pbkdf2::pbkdf2_sha256_32(b"hunter2", &salt, 4096);
        let client_key = hmac::hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256::hash(&client_key);
        let client_signature = hmac::hmac_sha256(&stored_key, auth_message.as_bytes());
        let mut client_proof = [0u8; 32];
        for i in 0..32 {
            client_proof[i] = client_key[i] ^ client_signature[i];
        }

        // Server verifies and signs.
        let server_signature = verify_and_sign(
            &secrets,
            &client_first_bare,
            &server_first,
            &client_final_without_proof,
            &client_proof,
        )
        .expect("verify must succeed for a real proof");
        assert!(server_signature.starts_with("v="));
    }

    #[test]
    fn wrong_password_fails_verify() {
        let salt = [5u8; 16];
        let secrets = compute_scram_secrets("correct", salt, 4096);
        // Client uses wrong password.
        let salted = spg_crypto::pbkdf2::pbkdf2_sha256_32(b"wrong", &salt, 4096);
        let client_key = hmac::hmac_sha256(&salted, b"Client Key");
        let auth_message = "n=u,r=x,r=x,c=biws,r=x".to_string();
        let client_signature =
            hmac::hmac_sha256(&sha256::hash(&client_key), auth_message.as_bytes());
        let mut client_proof = [0u8; 32];
        for i in 0..32 {
            client_proof[i] = client_key[i] ^ client_signature[i];
        }
        let result = verify_and_sign(&secrets, "n=u,r=x", "r=x", "c=biws,r=x", &client_proof);
        assert!(matches!(result, Err(ScramError::ProofMismatch)));
    }
}
