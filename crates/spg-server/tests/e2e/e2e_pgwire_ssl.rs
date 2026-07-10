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
fn plaintext_still_works_without_ssl() {
    // Regression: a client that connects without SSLRequest reaches
    // ReadyForQuery the old way (peek left the StartupMessage intact).
    let (_guard, addr) = spawn();
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    drive_to_ready(&mut s);
}
