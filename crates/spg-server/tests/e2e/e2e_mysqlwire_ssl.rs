//! v7.17.0 Phase 3.P0-77 — `Protocol::SSLRequest` mid-handshake
//! upgrade. Client sends a 32-byte SSLRequest with CLIENT_SSL
//! in its capability flags; server initiates a TLS handshake on
//! the same TCP socket. The post-TLS HandshakeResponse41 plus
//! every subsequent command runs through the encrypted stream.

use crate::common;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn unique_tmpdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let p = std::env::temp_dir().join(format!("spg-e2e-mysqlwire-ssl-{label}-{pid}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn spawn() -> (common::ChildGuard, String) {
    let dir = unique_tmpdir("svc");
    let db = dir.join("spg.db");
    let (child, addrs) = common::ServerBuilder::new()
        .arg_path(&db)
        .with_mysqlwire()
        .spawn();
    let addr = addrs.mysqlwire.expect("mysql-wire addr");
    (common::ChildGuard(child), addr)
}

fn write_packet<S: Write>(s: &mut S, seqno: u8, payload: &[u8]) {
    let len = payload.len() as u32;
    let hdr = [len as u8, (len >> 8) as u8, (len >> 16) as u8, seqno];
    s.write_all(&hdr).unwrap();
    s.write_all(payload).unwrap();
}

fn read_packet<S: Read>(s: &mut S) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr).unwrap();
    let len = u32::from(hdr[0]) | (u32::from(hdr[1]) << 8) | (u32::from(hdr[2]) << 16);
    let seqno = hdr[3];
    let mut payload = vec![0u8; len as usize];
    s.read_exact(&mut payload).unwrap();
    (seqno, payload)
}

const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
const CLIENT_SSL: u32 = 0x0000_0800;

fn build_ssl_request() -> Vec<u8> {
    let caps: u32 = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_SSL;
    let mut payload = Vec::with_capacity(32);
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes()); // max packet
    payload.push(0xff); // utf8mb4
    payload.extend_from_slice(&[0u8; 23]); // reserved filler
    assert_eq!(payload.len(), 32);
    payload
}

fn build_handshake_response() -> Vec<u8> {
    let caps: u32 = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH;
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&16_777_215u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0u8; 23]);
    payload.extend_from_slice(b"anyone\0");
    payload.push(0); // empty auth_response — open mode accepts
    payload.extend_from_slice(b"mysql_native_password\0");
    payload
}

/// rustls verifier that accepts the server's self-signed cert
/// unconditionally — fine for the e2e since the cert is minted
/// by the server we just spawned. Production deployments hook a
/// real trust root.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
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
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    Arc::new(cfg)
}

#[test]
fn ssl_upgrade_completes_handshake_and_reaches_command_phase() {
    let (_guard, addr) = spawn();
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seq, greeting) = read_packet(&mut s);
    // Confirm the server advertised CLIENT_SSL in the greeting
    // (capability hi at the documented offset).
    let nul = greeting[1..].iter().position(|b| *b == 0).unwrap();
    let pos_after_version = 1 + nul + 1 + 4 + 8 + 1;
    let cap_lo = u16::from_le_bytes(
        greeting[pos_after_version..pos_after_version + 2]
            .try_into()
            .unwrap(),
    ) as u32;
    let cap_hi = u16::from_le_bytes(
        greeting[pos_after_version + 5..pos_after_version + 7]
            .try_into()
            .unwrap(),
    ) as u32;
    let caps = cap_lo | (cap_hi << 16);
    assert!(caps & CLIENT_SSL != 0, "server should advertise CLIENT_SSL");

    // Send SSLRequest at seqno=1.
    write_packet(&mut s, 1, &build_ssl_request());

    // Drive the TLS handshake.
    let cfg = client_config();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut client_conn = rustls::ClientConnection::new(cfg, server_name).unwrap();
    let mut tls = rustls::Stream::new(&mut client_conn, &mut s);

    // Send HandshakeResponse41 over the encrypted stream at seqno=2.
    write_packet(&mut tls, 2, &build_handshake_response());
    let (seqno, ok) = read_packet(&mut tls);
    assert_eq!(seqno, 3);
    assert_eq!(ok[0], 0x00, "OK after SSL handshake auth");
}

#[test]
fn query_works_over_tls() {
    let (_guard, addr) = spawn();
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seq, _greet) = read_packet(&mut s);
    write_packet(&mut s, 1, &build_ssl_request());

    let cfg = client_config();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut client_conn = rustls::ClientConnection::new(cfg, server_name).unwrap();
    let mut tls = rustls::Stream::new(&mut client_conn, &mut s);
    write_packet(&mut tls, 2, &build_handshake_response());
    let _ = read_packet(&mut tls); // OK

    // COM_QUERY over TLS.
    let mut q = vec![0x03];
    q.extend_from_slice(b"SELECT 'tls-roundtrip' AS s");
    write_packet(&mut tls, 0, &q);
    // column_count
    let (_seq, _cc) = read_packet(&mut tls);
    // column_def
    let (_seq, _col) = read_packet(&mut tls);
    // v7.39 (round 504) — EOF closing the column definitions; this client
    // takes no CLIENT_DEPRECATE_EOF.
    let (_seq, cols_eof) = read_packet(&mut tls);
    assert_eq!(cols_eof[0], 0xfe, "EOF closes the column definitions");
    // row
    let (_seq, row) = read_packet(&mut tls);
    let n = row[0] as usize; // single-byte lenenc length < 251
    let body = &row[1..1 + n];
    assert_eq!(body, b"tls-roundtrip");
    // trailing EOF
    let (_seq, eof) = read_packet(&mut tls);
    assert_eq!(eof[0], 0xfe);
}

#[test]
fn plaintext_still_works_when_client_skips_ssl() {
    // Negative regression — clients that don't speak TLS still
    // reach the command phase the old way.
    let (_guard, addr) = spawn();
    let mut s = common::connect_to(&addr);
    s.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let (_seq, _greet) = read_packet(&mut s);
    write_packet(&mut s, 1, &build_handshake_response());
    let (_seq, ok) = read_packet(&mut s);
    assert_eq!(ok[0], 0x00);
}
