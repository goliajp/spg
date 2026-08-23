//! v7.38.18 — `pg_hba.conf`-style host-based authentication rules.
//!
//! The compatibility matrix listed these as `❌ Single password / role
//! per session`, and that was true: SPG demanded a credential or did
//! not, with nothing in between. A customer arriving from PostgreSQL
//! brings a file that says *this network may connect without a
//! password, that one may not connect at all*, and had nowhere to put
//! it.
//!
//! What is implemented is the shape PostgreSQL 18.4 actually uses, read
//! off a live one:
//!
//! ```text
//! local   all   all                    trust
//! host    all   all   127.0.0.1/32     trust
//! host    all   all   ::1/128          trust
//! host    all   all   all              scram-sha-256
//! ```
//!
//! **The first matching line decides, and a failure under it is a
//! refusal rather than a fallthrough.** That is PostgreSQL's rule and
//! it is the one that makes the file a security control: a `reject`
//! line above a permissive one has to win, or the file says the
//! opposite of what it reads like.
//!
//! No file means no rules, and no rules means exactly the behaviour
//! every deployment has today — the credential is demanded when a user
//! with credentials exists. Nothing changes for anyone who does not
//! write one.

use std::net::IpAddr;

/// What a matching rule says to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Let the connection in with no credential.
    Trust,
    /// Refuse it, with PostgreSQL's own message.
    Reject,
    /// Demand SCRAM-SHA-256.
    Scram,
    /// Demand a cleartext password.
    Password,
}

impl Method {
    fn parse(w: &str) -> Option<Self> {
        match w.to_ascii_lowercase().as_str() {
            "trust" => Some(Self::Trust),
            "reject" => Some(Self::Reject),
            "scram-sha-256" => Some(Self::Scram),
            "password" => Some(Self::Password),
            _ => None,
        }
    }
}

/// Which kind of connection a line applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnType {
    /// A unix socket. SPG has none, so these lines never match — they
    /// are parsed rather than rejected because a customer's file has
    /// them and refusing to load the file over a line that cannot
    /// apply would be worse than ignoring it.
    Local,
    /// Any TCP connection.
    Host,
    /// TCP with TLS, and without.
    HostSsl,
    HostNoSsl,
}

#[derive(Debug, Clone)]
struct Rule {
    conn: ConnType,
    database: String,
    user: String,
    /// `None` for a `local` line; otherwise the network this applies to.
    net: Option<(IpAddr, u8)>,
    method: Method,
}

/// A parsed file. Empty means "no rules", which is not the same as
/// "reject everything" — see the module note.
#[derive(Debug, Clone, Default)]
pub struct Hba {
    rules: Vec<Rule>,
}

fn parse_net(tok: &str) -> Option<(IpAddr, u8)> {
    // `samehost` and `samenet` need the server's own addresses, which
    // this does not have; `all` is everything.
    if tok.eq_ignore_ascii_case("all") {
        return Some((IpAddr::from([0u8, 0, 0, 0]), 0));
    }
    let (addr, bits) = tok.split_once('/')?;
    let ip: IpAddr = addr.parse().ok()?;
    let len: u8 = bits.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (len <= max).then_some((ip, len))
}

fn in_net(peer: IpAddr, (net, bits): (IpAddr, u8)) -> bool {
    // `all` — a zero-length prefix matches every address of any family,
    // which is what PostgreSQL's `all` keyword means.
    if bits == 0 && net == IpAddr::from([0u8, 0, 0, 0]) {
        return true;
    }
    match (peer, net) {
        (IpAddr::V4(p), IpAddr::V4(n)) => {
            let (p, n) = (u32::from(p), u32::from(n));
            bits == 0 || (p ^ n) >> (32 - u32::from(bits)) == 0
        }
        (IpAddr::V6(p), IpAddr::V6(n)) => {
            let (p, n) = (u128::from(p), u128::from(n));
            bits == 0 || (p ^ n) >> (128 - u32::from(bits)) == 0
        }
        // v7.38.18 — an IPv4 client against an IPv4-mapped IPv6 rule and
        // the reverse. A listener bound to `::` reports `::ffff:127.0.0.1`
        // for a client that dialled `127.0.0.1`, and a file that says
        // `127.0.0.1/32` has to match it or the rule silently never fires.
        (IpAddr::V6(p), IpAddr::V4(_)) => p
            .to_ipv4_mapped()
            .is_some_and(|v4| in_net(IpAddr::V4(v4), (net, bits))),
        (IpAddr::V4(_), IpAddr::V6(n)) => n
            .to_ipv4_mapped()
            .is_some_and(|v4| in_net(peer, (IpAddr::V4(v4), bits.saturating_sub(96)))),
    }
}

fn matches_name(pattern: &str, name: &str) -> bool {
    pattern == "all" || pattern.eq_ignore_ascii_case(name)
}

impl Hba {
    /// Parse a file's text. Malformed lines are an ERROR, not a
    /// silent skip: a typo in a security file that reads as "no rule"
    /// is how a `reject` stops rejecting.
    ///
    /// # Errors
    /// Returns the line number and what was wrong with it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut rules = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            let lineno = n + 1;
            let conn = match f.first().map(|s| s.to_ascii_lowercase()) {
                Some(ref s) if s == "local" => ConnType::Local,
                Some(ref s) if s == "host" => ConnType::Host,
                Some(ref s) if s == "hostssl" => ConnType::HostSsl,
                Some(ref s) if s == "hostnossl" => ConnType::HostNoSsl,
                other => {
                    return Err(format!(
                        "line {lineno}: expected local/host/hostssl/hostnossl, found {}",
                        other.unwrap_or_default()
                    ));
                }
            };
            let local = conn == ConnType::Local;
            let want = if local { 4 } else { 5 };
            if f.len() < want {
                return Err(format!(
                    "line {lineno}: expected {want} fields, found {}",
                    f.len()
                ));
            }
            let net =
                if local {
                    None
                } else {
                    Some(parse_net(f[3]).ok_or_else(|| {
                        format!("line {lineno}: {:?} is not an address/prefix", f[3])
                    })?)
                };
            let method_tok = f[if local { 3 } else { 4 }];
            let method = Method::parse(method_tok).ok_or_else(|| {
                format!(
                    "line {lineno}: authentication method {method_tok:?} is not one SPG \
                     performs (trust, reject, scram-sha-256, password)"
                )
            })?;
            rules.push(Rule {
                conn,
                database: f[1].to_string(),
                user: f[2].to_string(),
                net,
                method,
            });
        }
        Ok(Self { rules })
    }

    /// Is the file empty of rules?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The method for this connection, or `None` when no line matches.
    ///
    /// PostgreSQL refuses a connection that matches nothing, and so
    /// does the caller — but the distinction is kept here so the
    /// message can say which happened.
    #[must_use]
    pub fn method_for(
        &self,
        peer: IpAddr,
        database: &str,
        user: &str,
        tls: bool,
    ) -> Option<Method> {
        self.rules
            .iter()
            .find(|r| {
                let conn_ok = match r.conn {
                    // SPG has no unix socket, so a `local` line never
                    // matches a connection that got here.
                    ConnType::Local => false,
                    ConnType::Host => true,
                    ConnType::HostSsl => tls,
                    ConnType::HostNoSsl => !tls,
                };
                conn_ok
                    && matches_name(&r.database, database)
                    && matches_name(&r.user, user)
                    && r.net.is_some_and(|n| in_net(peer, n))
            })
            .map(|r| r.method)
    }
}

#[cfg(test)]
mod tests {
    use super::{Hba, Method};
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("addr")
    }

    /// The file PostgreSQL 18.4 ships, read off a live one, parses and
    /// matches the way it reads.
    #[test]
    fn the_shipped_postgres_file_parses_and_matches() {
        let hba = Hba::parse(
            "# comment\n\
             \n\
             local   all   all                    trust\n\
             host    all   all   127.0.0.1/32     trust\n\
             host    all   all   ::1/128          trust\n\
             host    all   all   all              scram-sha-256\n",
        )
        .expect("parses");
        assert!(!hba.is_empty());
        // The loopback lines win for loopback...
        assert_eq!(
            hba.method_for(ip("127.0.0.1"), "spg", "admin", false),
            Some(Method::Trust)
        );
        assert_eq!(
            hba.method_for(ip("::1"), "spg", "admin", false),
            Some(Method::Trust)
        );
        // ...and everything else falls to the catch-all.
        assert_eq!(
            hba.method_for(ip("10.1.2.3"), "spg", "admin", false),
            Some(Method::Scram)
        );
        // A `local` line never matches: SPG has no unix socket, so a
        // connection that reaches here came over TCP.
        assert_eq!(
            Hba::parse("local all all trust")
                .expect("parses")
                .method_for(ip("127.0.0.1"), "spg", "admin", false),
            None
        );
    }

    /// The first match decides, and a `reject` above a permissive line
    /// wins. This is what makes the file a security control rather than
    /// a suggestion -- fallthrough would make the file say the opposite
    /// of what it reads like.
    #[test]
    fn the_first_matching_line_decides() {
        let hba = Hba::parse(
            "host all all 127.0.0.1/32 reject\n\
             host all all all          trust\n",
        )
        .expect("parses");
        assert_eq!(
            hba.method_for(ip("127.0.0.1"), "spg", "admin", false),
            Some(Method::Reject)
        );
        assert_eq!(
            hba.method_for(ip("10.0.0.1"), "spg", "admin", false),
            Some(Method::Trust)
        );
        // And a connection matching NOTHING gets nothing, which the
        // caller turns into PostgreSQL's "no pg_hba.conf entry" error.
        assert_eq!(
            Hba::parse("host all all 10.0.0.0/8 trust")
                .expect("parses")
                .method_for(ip("127.0.0.1"), "spg", "admin", false),
            None
        );
    }

    /// `hostssl` and `hostnossl` split on whether TLS is up.
    #[test]
    fn the_ssl_variants_split_on_tls() {
        let hba = Hba::parse(
            "hostssl   all all all scram-sha-256\n\
             hostnossl all all all reject\n",
        )
        .expect("parses");
        assert_eq!(
            hba.method_for(ip("10.0.0.1"), "spg", "u", true),
            Some(Method::Scram)
        );
        assert_eq!(
            hba.method_for(ip("10.0.0.1"), "spg", "u", false),
            Some(Method::Reject)
        );
    }

    /// A database or user name narrows the line, and `all` does not.
    #[test]
    fn a_named_database_or_user_narrows_the_line() {
        let hba = Hba::parse(
            "host app  alice 0.0.0.0/0 trust\n\
             host all  all   0.0.0.0/0 reject\n",
        )
        .expect("parses");
        assert_eq!(
            hba.method_for(ip("10.0.0.1"), "app", "alice", false),
            Some(Method::Trust)
        );
        // Right user, wrong database.
        assert_eq!(
            hba.method_for(ip("10.0.0.1"), "other", "alice", false),
            Some(Method::Reject)
        );
        // Right database, wrong user.
        assert_eq!(
            hba.method_for(ip("10.0.0.1"), "app", "bob", false),
            Some(Method::Reject)
        );
    }

    /// An IPv4 client arriving on a dual-stack listener reports
    /// `::ffff:127.0.0.1`. A file that says `127.0.0.1/32` has to match
    /// it, or the rule silently never fires -- which for a `reject`
    /// line means the thing it was written to stop gets in.
    #[test]
    fn an_ipv4_mapped_address_matches_the_ipv4_rule() {
        let hba = Hba::parse("host all all 127.0.0.1/32 reject").expect("parses");
        assert_eq!(
            hba.method_for(ip("::ffff:127.0.0.1"), "spg", "u", false),
            Some(Method::Reject)
        );
        assert_eq!(
            hba.method_for(ip("::ffff:10.0.0.1"), "spg", "u", false),
            None
        );
    }

    /// A file that will not parse is an ERROR with a line number, never
    /// a silent skip. A typo that reads as "no rule" is how a `reject`
    /// stops rejecting, and the caller turns this into a refusal to
    /// start.
    #[test]
    fn a_malformed_line_names_itself() {
        for (text, want) in [
            ("garbage\n", "line 1"),
            ("host all all 127.0.0.1/32 kerberos\n", "kerberos"),
            ("host all all not-an-address trust\n", "not-an-address"),
            ("host all all\n", "expected 5 fields"),
        ] {
            let err = Hba::parse(text).expect_err(text);
            assert!(err.contains(want), "{text:?}: {err}");
        }
        // Comments and blank lines are not malformed.
        assert!(
            Hba::parse("# just a comment\n\n")
                .expect("parses")
                .is_empty()
        );
    }
}
