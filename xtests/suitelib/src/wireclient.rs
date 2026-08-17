//! A minimal pgwire client — enough protocol to ask a suite-owned
//! server real questions (S0.10's ironrule probes; later the
//! isolation driver, S4.1). Zero-dep, text format only.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Conn {
    stream: TcpStream,
}

#[derive(Debug, Default)]
pub struct QueryResult {
    pub n_columns: usize,
    pub rows: Vec<Vec<String>>,
    pub command_tags: Vec<String>,
    pub error: Option<String>,
}

fn read_exact(s: &mut TcpStream, n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)
        .map_err(|e| format!("wire read: {e}"))?;
    Ok(buf)
}

fn read_msg(s: &mut TcpStream) -> Result<(u8, Vec<u8>), String> {
    let head = read_exact(s, 5)?;
    let kind = head[0];
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len < 4 {
        return Err(format!("wire: bad length {len}"));
    }
    let body = read_exact(s, len - 4)?;
    Ok((kind, body))
}

impl Conn {
    /// Connect + startup as `user`/`db`; handles trust and cleartext
    /// auth (the suite's servers), refuses anything fancier by name.
    ///
    /// # Errors
    /// Connection, protocol, or an auth method this client does not
    /// speak — each named.
    pub fn connect(port: u16, user: &str, db: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| format!("connect 127.0.0.1:{port}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("timeout: {e}"))?;
        let mut body = Vec::new();
        body.extend_from_slice(&196_608u32.to_be_bytes()); // protocol 3.0
        for (k, v) in [("user", user), ("database", db)] {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut msg = Vec::with_capacity(body.len() + 4);
        msg.extend_from_slice(&(u32::try_from(body.len() + 4).unwrap()).to_be_bytes());
        msg.extend_from_slice(&body);
        stream
            .write_all(&msg)
            .map_err(|e| format!("startup: {e}"))?;
        loop {
            let (kind, body) = read_msg(&mut stream)?;
            match kind {
                b'R' => {
                    let code = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    match code {
                        0 => {}
                        3 => {
                            let mut pw = Vec::new();
                            pw.push(b'p');
                            let payload = format!("{user}\0");
                            pw.extend_from_slice(
                                &(u32::try_from(payload.len() + 4).unwrap()).to_be_bytes(),
                            );
                            pw.extend_from_slice(payload.as_bytes());
                            stream
                                .write_all(&pw)
                                .map_err(|e| format!("password: {e}"))?;
                        }
                        other => {
                            return Err(format!(
                                "wire: auth method {other} not spoken by this client"
                            ));
                        }
                    }
                }
                b'S' | b'K' | b'N' => {} // parameter status / key data / notice
                b'Z' => return Ok(Self { stream }),
                b'E' => return Err(format!("startup error: {}", err_text(&body))),
                other => return Err(format!("wire: unexpected {} in startup", other as char)),
            }
        }
    }

    /// Simple-query protocol: one `Q`, collect until ReadyForQuery.
    ///
    /// # Errors
    /// Transport only — a server-side SQL error lands in `.error`.
    pub fn simple_query(&mut self, sql: &str) -> Result<QueryResult, String> {
        let mut msg = vec![b'Q'];
        msg.extend_from_slice(&(u32::try_from(sql.len() + 5).unwrap()).to_be_bytes());
        msg.extend_from_slice(sql.as_bytes());
        msg.push(0);
        self.stream
            .write_all(&msg)
            .map_err(|e| format!("query: {e}"))?;
        let mut out = QueryResult::default();
        loop {
            let (kind, body) = read_msg(&mut self.stream)?;
            match kind {
                b'T' => {
                    out.n_columns = u16::from_be_bytes([body[0], body[1]]) as usize;
                }
                b'D' => {
                    let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                    let mut row = Vec::with_capacity(n);
                    let mut cur = 2usize;
                    for _ in 0..n {
                        let len = i32::from_be_bytes([
                            body[cur],
                            body[cur + 1],
                            body[cur + 2],
                            body[cur + 3],
                        ]);
                        cur += 4;
                        if len < 0 {
                            row.push(String::from("NULL"));
                        } else {
                            let end = cur + len as usize;
                            row.push(String::from_utf8_lossy(&body[cur..end]).into_owned());
                            cur = end;
                        }
                    }
                    out.rows.push(row);
                }
                b'C' => out.command_tags.push(
                    String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string(),
                ),
                b'E' => out.error = Some(err_text(&body)),
                b'Z' => return Ok(out),
                _ => {}
            }
        }
    }
}

fn err_text(body: &[u8]) -> String {
    // Severity/message fields: take the 'M' field.
    let mut cur = 0usize;
    while cur < body.len() && body[cur] != 0 {
        let code = body[cur];
        let end = body[cur + 1..]
            .iter()
            .position(|&b| b == 0)
            .map_or(body.len(), |p| cur + 1 + p);
        if code == b'M' {
            return String::from_utf8_lossy(&body[cur + 1..end]).into_owned();
        }
        cur = end + 1;
    }
    String::from("(no message)")
}

/// The SSLRequest probe: 8 bytes, expect a single-byte answer. What the
/// answer IS ('S' or 'N') is the server's choice; that it ANSWERS is
/// the pinned contract (a v7.37 ironrule — psql's first packet).
///
/// # Errors
/// Transport, or no answer byte.
pub fn ssl_request_answered(port: u16) -> Result<u8, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("timeout: {e}"))?;
    let mut req = Vec::new();
    req.extend_from_slice(&8u32.to_be_bytes());
    req.extend_from_slice(&80_877_103u32.to_be_bytes());
    s.write_all(&req).map_err(|e| format!("sslreq: {e}"))?;
    let mut answer = [0u8; 1];
    s.read_exact(&mut answer)
        .map_err(|e| format!("no answer to SSLRequest: {e}"))?;
    Ok(answer[0])
}
