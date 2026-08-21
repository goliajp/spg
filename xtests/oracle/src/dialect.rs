//! DialectAdapter — one variant per reference master.
//!
//! Each adapter knows:
//! - how to connect (URI shape, default port, credentials)
//! - the textual suffix used for `expected/<stem>.<suffix>.out`
//! - the dialect's quirks that the corresponding `adjust_*` step is
//!   expected to absorb
//!
//! v7.38 C ships the enum and a `connect_uri()` resolver; live
//! execution paths land during P1 fill alongside the sqlx dep
//! upgrade (`features = ["postgres", "mysql"]`).

use clap::ValueEnum;

/// One reference master.
///
/// `Pg18` is the primary differential target — SPG positions as a
/// PG drop-in. `Mysql` and `Mariadb` are the other two contracts
/// SPG claims to satisfy (see `memory/vision-*` / project memory).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Oracle {
    Pg18,
    Mysql,
    Mariadb,
}

impl Oracle {
    /// Suffix used in `expected/<stem>.<suffix>.out` to disambiguate
    /// the three masters' captured baselines.
    pub fn suffix(self) -> &'static str {
        match self {
            Oracle::Pg18 => "pg",
            Oracle::Mysql => "mysql",
            Oracle::Mariadb => "mariadb",
        }
    }

    /// Default localhost connection URI for the docker-compose
    /// service of this oracle. Picks the matching ported host port
    /// (PG `15432` / MySQL `15433` / MariaDB `15434`) so a developer
    /// can run `psql -p 15432` without colliding with system PG.
    ///
    /// v7.38.14 — moved off 55432 / 53306 / 53307. Those sit inside
    /// macOS's EPHEMERAL port range, so any process on the machine can
    /// take one transiently, and one had: another project's
    /// `portal-dev-postgres` was listening on 55432, which means this
    /// harness would have opened a connection to a stranger's database
    /// and used its answers as the oracle. The replacements are
    /// registered in the shared port registry rather than picked, which
    /// is what stops the next collision being found the same way.
    pub fn connect_uri(self) -> &'static str {
        match self {
            Oracle::Pg18 => "postgres://testuser:testpass@127.0.0.1:15432/testdb",
            Oracle::Mysql => "mysql://root:testpass@127.0.0.1:15433/testdb",
            Oracle::Mariadb => "mysql://root:testpass@127.0.0.1:15434/testdb",
        }
    }

    /// Family. PG uses extended-query semantics; MySQL/MariaDB share
    /// the MySQL wire family. Useful for picking adjust_*() steps
    /// that group by family.
    pub fn family(self) -> Family {
        match self {
            Oracle::Pg18 => Family::Postgres,
            Oracle::Mysql | Oracle::Mariadb => Family::Mysql,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Family {
    Postgres,
    Mysql,
}
