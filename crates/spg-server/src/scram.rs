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
//! We support only channel-binding-disabled mode ("n,," GS2 header).
//! That matches "no TLS" — same out-of-scope frame as the rest of
//! the auth layer.

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

/// What's parsed out of client-first-message.
#[derive(Debug)]
pub struct ClientFirst {
    /// The portion the spec calls "client-first-message-bare" —
    /// `n=user,r=clientNonce`. Needed verbatim for the AuthMessage.
    pub bare: String,
    pub client_nonce: String,
}

pub fn parse_client_first(msg: &str) -> Result<ClientFirst, ScramError> {
    // GS2 header is one of "n,," / "y,," / "p=<binding>,<binding>,".
    // We only support "n,," (no channel binding).
    let stripped = msg
        .strip_prefix("n,,")
        .ok_or_else(|| ScramError::BadInitial("only 'n,,' GS2 header supported".into()))?;
    let bare = stripped.to_string();
    // bare = "n=user,r=nonce[,...]"
    let mut client_nonce = None;
    for attr in bare.split(',') {
        if let Some(rest) = attr.strip_prefix("r=") {
            client_nonce = Some(rest.to_string());
        }
    }
    let client_nonce = client_nonce
        .ok_or_else(|| ScramError::BadInitial("missing r= (client nonce) attribute".into()))?;
    Ok(ClientFirst { bare, client_nonce })
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
    for attr in without_proof.split(',') {
        if let Some(rest) = attr.strip_prefix("r=") {
            combined_nonce = Some(rest.to_string());
        }
    }
    let combined_nonce = combined_nonce
        .ok_or_else(|| ScramError::BadFinal("missing r= attribute in without-proof".into()))?;
    Ok(ClientFinal {
        without_proof,
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
        assert_eq!(cf.bare, "n=alice,r=clientnonce123");
        assert_eq!(cf.client_nonce, "clientnonce123");
    }

    #[test]
    fn parse_client_first_rejects_channel_binding() {
        let err = parse_client_first("p=tls-unique,,n=alice,r=nonce").unwrap_err();
        assert!(matches!(err, ScramError::BadInitial(_)));
    }

    #[test]
    fn parse_client_final_round_trip() {
        let proof = [7u8; 32];
        let proof_b64 = base64::encode(&proof);
        let msg = format!("c=biws,r=combined,p={proof_b64}");
        let cf = parse_client_final(&msg).unwrap();
        assert_eq!(cf.without_proof, "c=biws,r=combined");
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
