//! v7.39 (TLS/SCRAM Phase 1) — pgwire SSLRequest → TLS termination. A client
//! sends the SSLRequest (Int32(8), Int32(80877103)); the server replies 'S' and
//! runs a rustls handshake, then the StartupMessage + every message runs over
//! the encrypted stream. A client that skips SSL still connects in cleartext.

use crate::common;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const SSL_REQUEST_CODE: u32 = 80_877_103;
const PROTOCOL_V3: u32 = 196_608; // 0x0003_0000

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let p = std::env::temp_dir().join(format!("spg-e2e-pgwire-ssl-{label}-{pid}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn() -> (common::ChildGuard, String) {
    let dir = unique_tmpdir("svc");
    let db = dir.join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .spawn();
    (
        common::ChildGuard(child),
        addrs.pgwire.expect("pgwire addr"),
    )
}

fn ssl_request() -> Vec<u8> {
    let mut v = Vec::with_capacity(8);
    v.extend_from_slice(&8u32.to_be_bytes());
    v.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    v
}

/// StartupMessage for `user=admin` (open mode accepts any user as admin).
fn startup_message() -> Vec<u8> {
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0admin\0");
    params.push(0); // terminating empty key
    let total = 4 + 4 + params.len();
    let mut v = Vec::with_capacity(total);
    v.extend_from_slice(&(total as u32).to_be_bytes());
    v.extend_from_slice(&PROTOCOL_V3.to_be_bytes());
    v.extend_from_slice(&params);
    v
}

/// A pgwire message: 1 type byte, Int32 length (incl. itself), body.
fn read_msg<S: Read>(s: &mut S) -> (u8, Vec<u8>) {
    let mut ty = [0u8; 1];
    s.read_exact(&mut ty).unwrap();
    let mut len = [0u8; 4];
    s.read_exact(&mut len).unwrap();
    let n = u32::from_be_bytes(len) as usize;
    let mut body = vec![0u8; n - 4];
    s.read_exact(&mut body).unwrap();
    (ty[0], body)
}

/// Drive the startup handshake to ReadyForQuery over `s`, returning after 'Z'.
fn drive_to_ready<S: Read + Write>(s: &mut S) {
    s.write_all(&startup_message()).unwrap();
    // First reply must be AuthenticationOk ('R' + Int32 0) in open mode.
    let (ty, body) = read_msg(s);
    assert_eq!(ty, b'R', "expected AuthenticationOk");
    assert_eq!(u32::from_be_bytes(body[..4].try_into().unwrap()), 0);
    // Consume ParameterStatus / BackendKeyData until ReadyForQuery.
    loop {
        let (ty, _) = read_msg(s);
        if ty == b'Z' {
            break;
        }
    }
}

#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _e: &rustls::pki_types::CertificateDer<'_>,
        _i: &[rustls::pki_types::CertificateDer<'_>],
        _n: &rustls::pki_types::ServerName<'_>,
        _o: &[u8],
        _t: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

fn client_config() -> Arc<rustls::ClientConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
            .with_no_client_auth(),
    )
}

#[test]
fn ssl_request_upgrades_to_tls_and_reaches_ready() {
    let (_guard, addr) = spawn();
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(&ssl_request()).unwrap();
    // Server accepts TLS with a single 'S' byte.
    let mut reply = [0u8; 1];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], b'S', "server should accept TLS");
    // Handshake, then the startup runs over the encrypted stream.
    let cfg = client_config();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = rustls::ClientConnection::new(cfg, name).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut s);
    drive_to_ready(&mut tls);
}

#[test]
fn query_over_tls_returns_a_row() {
    let (_guard, addr) = spawn();
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(&ssl_request()).unwrap();
    let mut reply = [0u8; 1];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], b'S');
    let cfg = client_config();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = rustls::ClientConnection::new(cfg, name).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut s);
    drive_to_ready(&mut tls);
    // Simple Query 'SELECT 1' over TLS.
    let sql = b"SELECT 1\0";
    let mut q = Vec::new();
    q.push(b'Q');
    q.extend_from_slice(&((4 + sql.len()) as u32).to_be_bytes());
    q.extend_from_slice(sql);
    tls.write_all(&q).unwrap();
    // Expect RowDescription 'T', then a DataRow 'D' carrying "1".
    let (t1, _) = read_msg(&mut tls);
    assert_eq!(t1, b'T', "RowDescription");
    let (t2, row) = read_msg(&mut tls);
    assert_eq!(t2, b'D', "DataRow");
    // DataRow: Int16 field count, then per field Int32 len + bytes.
    let cnt = u16::from_be_bytes(row[..2].try_into().unwrap());
    assert_eq!(cnt, 1);
    let flen = i32::from_be_bytes(row[2..6].try_into().unwrap());
    assert_eq!(flen, 1);
    assert_eq!(&row[6..7], b"1");
}

/// Records the leaf cert the server presents, then accepts it.
#[derive(Debug)]
struct CapturingVerifier(std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>);

impl rustls::client::danger::ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _i: &[rustls::pki_types::CertificateDer<'_>],
        _n: &rustls::pki_types::ServerName<'_>,
        _o: &[u8],
        _t: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        *self.0.lock().unwrap() = Some(end_entity.as_ref().to_vec());
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

#[test]
fn operator_supplied_cert_is_served() {
    // Generate an operator cert + key, write PEM, and start the server with
    // SPG_TLS_CERT / SPG_TLS_KEY pointing at them. The TLS handshake must
    // present exactly that cert (not the self-signed default).
    let dir = unique_tmpdir("opcert");
    let db = dir.join("spg.db");
    let cert =
        rcgen::generate_simple_self_signed(vec!["spg-operator.example".to_string()]).unwrap();
    let cert_der = cert.cert.der().to_vec();
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .env("SPG_TLS_CERT", cert_path.to_str().unwrap())
        .env("SPG_TLS_KEY", key_path.to_str().unwrap())
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.pgwire.expect("pgwire addr");

    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(&ssl_request()).unwrap();
    let mut reply = [0u8; 1];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], b'S');

    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(CapturingVerifier(seen.clone())))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("spg-operator.example").unwrap();
    let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(cfg), name).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut s);
    drive_to_ready(&mut tls);

    let presented = seen
        .lock()
        .unwrap()
        .clone()
        .expect("server presented a cert");
    assert_eq!(
        presented, cert_der,
        "server must serve the operator-supplied cert"
    );
}

#[test]
fn require_tls_rejects_plaintext_but_allows_tls() {
    let dir = unique_tmpdir("reqtls");
    let db = dir.join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_pgwire()
        .env("SPG_REQUIRE_TLS", "1")
        .spawn();
    let _guard = common::ChildGuard(child);
    let addr = addrs.pgwire.expect("pgwire addr");

    // Plaintext startup → ErrorResponse ('E'), no AuthenticationOk.
    {
        let mut s = common::connect_to(&addr);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        s.write_all(&startup_message()).unwrap();
        let (ty, _) = read_msg(&mut s);
        assert_eq!(
            ty, b'E',
            "plaintext must be refused when SPG_REQUIRE_TLS is set"
        );
    }

    // The same server still accepts a TLS connection.
    {
        let mut s = common::connect_to(&addr);
        s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        s.write_all(&ssl_request()).unwrap();
        let mut reply = [0u8; 1];
        s.read_exact(&mut reply).unwrap();
        assert_eq!(reply[0], b'S');
        let cfg = client_config();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut conn = rustls::ClientConnection::new(cfg, name).unwrap();
        let mut tls = rustls::Stream::new(&mut conn, &mut s);
        drive_to_ready(&mut tls);
    }
}

#[test]
fn plaintext_still_works_without_ssl() {
    // Regression: a client that connects without SSLRequest reaches
    // ReadyForQuery the old way (peek left the StartupMessage intact).
    let (_guard, addr) = spawn();
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    drive_to_ready(&mut s);
}

// ---- v7.39 (TLS/SCRAM Phase 3) — SCRAM-SHA-256-PLUS channel binding ----

/// StartupMessage for an arbitrary `user`.
fn startup_message_for(user: &str) -> Vec<u8> {
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0");
    params.extend_from_slice(user.as_bytes());
    params.push(0);
    params.push(0); // terminating empty key
    let total = 4 + 4 + params.len();
    let mut v = Vec::with_capacity(total);
    v.extend_from_slice(&(total as u32).to_be_bytes());
    v.extend_from_slice(&PROTOCOL_V3.to_be_bytes());
    v.extend_from_slice(&params);
    v
}

/// Send a Simple Query and drain to ReadyForQuery.
fn simple_query<S: Read + Write>(s: &mut S, sql: &str) {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    let mut q = Vec::new();
    q.push(b'Q');
    q.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    q.extend_from_slice(&body);
    s.write_all(&q).unwrap();
    loop {
        let (ty, _) = read_msg(s);
        if ty == b'Z' {
            break;
        }
    }
}

/// True if `buf` (NUL-delimited mechanism list) contains `needle`.
fn advertises(buf: &[u8], needle: &str) -> bool {
    buf.split(|&b| b == 0).any(|m| m == needle.as_bytes())
}

/// SASLInitialResponse ('p'): mech name (NUL) + Int32 len + client-first.
fn send_sasl_initial<S: Write>(s: &mut S, mech: &str, client_first: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(mech.as_bytes());
    body.push(0);
    body.extend_from_slice(&(client_first.len() as u32).to_be_bytes());
    body.extend_from_slice(client_first.as_bytes());
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    msg.extend_from_slice(&body);
    s.write_all(&msg).unwrap();
}

/// SASLResponse ('p'): just the client-final bytes.
fn send_sasl_response<S: Write>(s: &mut S, client_final: &str) {
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&((4 + client_final.len()) as u32).to_be_bytes());
    msg.extend_from_slice(client_final.as_bytes());
    s.write_all(&msg).unwrap();
}

/// Parse server-first `r=<combined>,s=<salt_b64>,i=<iters>`.
fn parse_server_first(sf: &str) -> (String, String, u32) {
    let mut combined = None;
    let mut salt = None;
    let mut iters = None;
    for attr in sf.split(',') {
        if let Some(r) = attr.strip_prefix("r=") {
            combined = Some(r.to_string());
        } else if let Some(sv) = attr.strip_prefix("s=") {
            salt = Some(sv.to_string());
        } else if let Some(i) = attr.strip_prefix("i=") {
            iters = Some(i.parse().unwrap());
        }
    }
    (combined.unwrap(), salt.unwrap(), iters.unwrap())
}

/// Create a `readonly` SCRAM user over TLS (server is in open mode until the
/// first user exists, so this connection authenticates as admin).
fn create_scram_user(addr: &str, user: &str, password: &str) {
    let mut s = common::connect_to(addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(&ssl_request()).unwrap();
    let mut reply = [0u8; 1];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], b'S');
    let cfg = client_config();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = rustls::ClientConnection::new(cfg, name).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut s);
    drive_to_ready(&mut tls);
    simple_query(
        &mut tls,
        &format!("CREATE USER '{user}' WITH PASSWORD '{password}'"),
    );
}

#[test]
fn scram_sha256_plus_authenticates_over_tls() {
    use spg_crypto::{base64, hmac, pbkdf2, sha256};
    let (_guard, addr) = spawn();
    create_scram_user(&addr, "scramuser", "secret");

    // Reconnect as scramuser over TLS; capture the cert for channel binding.
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(&ssl_request()).unwrap();
    let mut reply = [0u8; 1];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], b'S');
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(CapturingVerifier(seen.clone())))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(cfg), name).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut s);

    tls.write_all(&startup_message_for("scramuser")).unwrap();
    let cert_der = seen.lock().unwrap().clone().expect("cert captured");
    let cert_hash = sha256::hash(&cert_der);

    // AuthenticationSASL advertising -PLUS.
    let (ty, body) = read_msg(&mut tls);
    assert_eq!(ty, b'R');
    assert_eq!(u32::from_be_bytes(body[..4].try_into().unwrap()), 10);
    assert!(
        advertises(&body[4..], "SCRAM-SHA-256-PLUS"),
        "server must advertise -PLUS over TLS"
    );

    // client-first with tls-server-end-point binding.
    let client_nonce = "clientnonce_abcdef0123456789";
    let gs2 = "p=tls-server-end-point,,";
    let client_first_bare = format!("n=,r={client_nonce}");
    send_sasl_initial(
        &mut tls,
        "SCRAM-SHA-256-PLUS",
        &format!("{gs2}{client_first_bare}"),
    );

    // server-first.
    let (ty, body) = read_msg(&mut tls);
    assert_eq!(ty, b'R');
    assert_eq!(u32::from_be_bytes(body[..4].try_into().unwrap()), 11);
    let server_first = std::str::from_utf8(&body[4..]).unwrap().to_string();
    let (combined, salt_b64, iters) = parse_server_first(&server_first);
    let salt = base64::decode(&salt_b64).unwrap();

    // client-final: compute the proof, bind c= to the cert hash.
    let salted = pbkdf2::pbkdf2_sha256_32(b"secret", &salt, iters);
    let client_key = hmac::hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256::hash(&client_key);
    let mut cbind_input = gs2.as_bytes().to_vec();
    cbind_input.extend_from_slice(&cert_hash);
    let c_b64 = base64::encode(&cbind_input);
    let without_proof = format!("c={c_b64},r={combined}");
    let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
    let client_sig = hmac::hmac_sha256(&stored_key, auth_message.as_bytes());
    let mut proof = [0u8; 32];
    for i in 0..32 {
        proof[i] = client_key[i] ^ client_sig[i];
    }
    let client_final = format!("{without_proof},p={}", base64::encode(&proof));
    send_sasl_response(&mut tls, &client_final);

    // SASLFinal (12) → AuthenticationOk (0) → ReadyForQuery.
    let (ty, body) = read_msg(&mut tls);
    assert_eq!(ty, b'R');
    assert_eq!(
        u32::from_be_bytes(body[..4].try_into().unwrap()),
        12,
        "SASLFinal"
    );
    let (ty, body) = read_msg(&mut tls);
    assert_eq!(ty, b'R');
    assert_eq!(
        u32::from_be_bytes(body[..4].try_into().unwrap()),
        0,
        "AuthenticationOk after valid channel-bound proof"
    );
    loop {
        let (t, _) = read_msg(&mut tls);
        if t == b'Z' {
            break;
        }
    }
}

#[test]
fn scram_plus_downgrade_y_flag_is_rejected() {
    let (_guard, addr) = spawn();
    create_scram_user(&addr, "scramuser2", "secret");

    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    s.write_all(&ssl_request()).unwrap();
    let mut reply = [0u8; 1];
    s.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], b'S');
    let cfg = client_config();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = rustls::ClientConnection::new(cfg, name).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut s);

    tls.write_all(&startup_message_for("scramuser2")).unwrap();
    // Consume AuthenticationSASL (server advertises -PLUS over TLS).
    let (ty, body) = read_msg(&mut tls);
    assert_eq!(ty, b'R');
    assert_eq!(u32::from_be_bytes(body[..4].try_into().unwrap()), 10);
    assert!(advertises(&body[4..], "SCRAM-SHA-256-PLUS"));

    // A channel-binding-capable client that picks plain SCRAM-SHA-256 with a
    // `y` flag is claiming "the server didn't offer -PLUS" — but it did, so
    // this is a stripped-advertisement MITM and must be refused.
    send_sasl_initial(&mut tls, "SCRAM-SHA-256", "y,,n=,r=noncenoncenonce123");
    let (ty, _) = read_msg(&mut tls);
    assert_eq!(
        ty, b'E',
        "y-flag against a -PLUS-capable server must be rejected as a downgrade"
    );
}
