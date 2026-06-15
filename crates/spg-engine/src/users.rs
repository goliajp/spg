//! User table + RBAC types for v4.1.
//!
//! Three roles, narrow on purpose:
//!
//! - `Admin` — full read+write + can manage other users
//! - `ReadWrite` — full read+write, no user-mgmt
//! - `ReadOnly` — SELECT / SHOW only
//!
//! Passwords stored as BLAKE3(salt || password) — the salt is a
//! random 16-byte value per user, kept inline with the record so we
//! never need to hash twice. The hash is not designed to resist a
//! determined offline attack on the snapshot file (that's what file
//! perms are for in the docker-compose deployment shape); it's
//! enough that the snapshot itself doesn't leak plaintext, and that
//! an in-memory dump can't trivially reverse a typed password.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_storage::{ColumnSchema, DataType, Row, Value};

use crate::{Engine, QueryResult};

const SALT_LEN: usize = 16;
const HASH_LEN: usize = 32;
/// v7.17.0 Phase 3.P0-71 — length of SHA1(SHA1(password)) stored
/// per user for `mysql_native_password` auth verification.
pub const MYSQL_NATIVE_HASH_LEN: usize = 20;
/// v7.17.0 Phase 3.P0-72 — length of SHA256(SHA256(password))
/// stored per user for `caching_sha2_password` auth
/// verification (the MySQL 8.0 default plugin).
pub const CACHING_SHA2_HASH_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    ReadWrite,
    ReadOnly,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::ReadWrite => "readwrite",
            Self::ReadOnly => "readonly",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "admin" => Some(Self::Admin),
            "readwrite" | "rw" => Some(Self::ReadWrite),
            "readonly" | "ro" => Some(Self::ReadOnly),
            _ => None,
        }
    }

    /// Read access — every role qualifies.
    pub const fn can_read(self) -> bool {
        true
    }

    /// Write access (INSERT / DDL on user tables).
    pub const fn can_write(self) -> bool {
        matches!(self, Self::Admin | Self::ReadWrite)
    }

    /// User-management DDL (`CREATE USER`, `DROP USER`).
    pub const fn can_manage_users(self) -> bool {
        matches!(self, Self::Admin)
    }
}

#[derive(Debug, Clone)]
pub struct UserRecord {
    pub role: Role,
    salt: [u8; SALT_LEN],
    hash: [u8; HASH_LEN],
    /// v4.8: SCRAM-SHA-256 verifier. Computed alongside the
    /// BLAKE3 hash at user creation so PG-wire SASL auth can
    /// verify without re-running PBKDF2 per attempt. `None`
    /// means the user predates v4.8 (loaded from an older
    /// snapshot); the PG-wire layer falls back to
    /// `CleartextPassword` for those users.
    scram: Option<ScramSecrets>,
    /// v7.17.0 Phase 3.P0-71: SHA1(SHA1(password)) — the
    /// `mysql.user.authentication_string` shape for the
    /// `mysql_native_password` plugin. Computed at create /
    /// set_password time alongside the BLAKE3 hash and the
    /// SCRAM verifier so the MySQL-wire shim doesn't need
    /// plaintext to verify. `None` for users loaded from a
    /// pre-v7.17.0 snapshot — the MySQL-wire shim rejects
    /// those with Access Denied until the password is reset.
    mysql_native: Option<[u8; MYSQL_NATIVE_HASH_LEN]>,
    /// v7.17.0 Phase 3.P0-72: SHA256(SHA256(password)) — the
    /// `mysql.user.authentication_string` shape for MySQL 8.0+'s
    /// default `caching_sha2_password` plugin. Computed at the
    /// same time as `mysql_native`. The MySQL-wire shim's fast
    /// path uses this for the SHA256-XOR proof verification —
    /// the public-key-RSA full-auth path is a v7.18 carve-out.
    caching_sha2: Option<[u8; CACHING_SHA2_HASH_LEN]>,
}

/// SCRAM-SHA-256 stored credentials per RFC 5802 §5.
/// `salt` and `iters` are sent to the client in server-first;
/// `stored_key` and `server_key` are kept secret and used in the
/// final-message verification.
#[derive(Debug, Clone)]
pub struct ScramSecrets {
    pub iters: u32,
    pub salt: [u8; SCRAM_SALT_LEN],
    pub stored_key: [u8; HASH_LEN],
    pub server_key: [u8; HASH_LEN],
}

pub const SCRAM_SALT_LEN: usize = 16;
pub const SCRAM_DEFAULT_ITERS: u32 = 4096;

impl UserRecord {
    pub fn verify(&self, password: &str) -> bool {
        let candidate = derive_hash(&self.salt, password);
        constant_time_eq(&candidate, &self.hash)
    }

    pub const fn scram(&self) -> Option<&ScramSecrets> {
        self.scram.as_ref()
    }

    /// v7.17.0 Phase 3.P0-71: borrow the stored
    /// `mysql_native_password` verifier (SHA1(SHA1(password)))
    /// for the MySQL-wire shim.
    pub const fn mysql_native(&self) -> Option<&[u8; MYSQL_NATIVE_HASH_LEN]> {
        self.mysql_native.as_ref()
    }

    /// v7.17.0 Phase 3.P0-72: borrow the stored
    /// `caching_sha2_password` verifier (SHA256(SHA256(password)))
    /// for the MySQL-wire shim's fast-path auth.
    pub const fn caching_sha2(&self) -> Option<&[u8; CACHING_SHA2_HASH_LEN]> {
        self.caching_sha2.as_ref()
    }

    /// v7.17.0 Phase 3.P0-72 — verify a client
    /// `caching_sha2_password` fast-path response.
    ///
    /// Protocol: same XOR shape as `mysql_native_password` but
    /// with SHA-256 instead of SHA-1:
    /// `client_response = SHA256(password) XOR SHA256(scramble
    /// || SHA256(SHA256(password)))`. Server reconstructs and
    /// checks `SHA256(reconstructed) == stored_hash`.
    ///
    /// Full-auth RSA fallback (when the cache misses) is a
    /// v7.18 carve-out — clients connecting over plaintext
    /// without a cached entry will see Access Denied from the
    /// shim until that lands.
    pub fn verify_caching_sha2_password(&self, scramble: &[u8], client_response: &[u8]) -> bool {
        let Some(stored) = self.caching_sha2 else {
            return false;
        };
        if client_response.len() != CACHING_SHA2_HASH_LEN {
            return false;
        }
        if scramble.len() != 20 {
            return false;
        }
        let mut buf = [0u8; 20 + CACHING_SHA2_HASH_LEN];
        buf[..20].copy_from_slice(scramble);
        buf[20..].copy_from_slice(&stored);
        let mask = sha256_bytes(&buf);
        let mut recovered = [0u8; CACHING_SHA2_HASH_LEN];
        for i in 0..CACHING_SHA2_HASH_LEN {
            recovered[i] = client_response[i] ^ mask[i];
        }
        let candidate = sha256_bytes(&recovered);
        constant_time_eq(&candidate, &stored)
    }

    /// v7.17.0 Phase 3.P0-71 — verify a client
    /// `mysql_native_password` auth response.
    ///
    /// Protocol: the client sends a 20-byte response
    /// `client_proof = SHA1(password) XOR SHA1(scramble ||
    /// SHA1(SHA1(password)))`. The server reconstructs
    /// `SHA1(password) = client_proof XOR SHA1(scramble ||
    /// stored_hash)` and verifies `SHA1(reconstructed) ==
    /// stored_hash`. Returns false if the user has no stored
    /// hash (loaded from a pre-v7.17 snapshot — the operator
    /// has to reset the password to re-populate it).
    pub fn verify_mysql_native_password(&self, scramble: &[u8], client_response: &[u8]) -> bool {
        let Some(stored) = self.mysql_native else {
            return false;
        };
        if client_response.len() != MYSQL_NATIVE_HASH_LEN {
            return false;
        }
        if scramble.len() != 20 {
            return false;
        }
        let mut buf = [0u8; 40];
        buf[..20].copy_from_slice(scramble);
        buf[20..].copy_from_slice(&stored);
        let mask = sha1_bytes(&buf);
        let mut recovered = [0u8; MYSQL_NATIVE_HASH_LEN];
        for i in 0..MYSQL_NATIVE_HASH_LEN {
            recovered[i] = client_response[i] ^ mask[i];
        }
        let candidate = sha1_bytes(&recovered);
        constant_time_eq_sha1(&candidate, &stored)
    }
}

/// Compute the `mysql_native_password` stored hash =
/// SHA1(SHA1(password)). Public so user-creation paths can
/// populate the field at the same moment they have cleartext.
#[must_use]
pub fn compute_mysql_native_hash(password: &str) -> [u8; MYSQL_NATIVE_HASH_LEN] {
    let inner = sha1_bytes(password.as_bytes());
    sha1_bytes(&inner)
}

/// v7.17.0 Phase 3.P0-72 — compute the `caching_sha2_password`
/// stored hash = SHA256(SHA256(password)). Public for the same
/// reason as the mysql_native variant.
#[must_use]
pub fn compute_caching_sha2_hash(password: &str) -> [u8; CACHING_SHA2_HASH_LEN] {
    let inner = sha256_bytes(password.as_bytes());
    sha256_bytes(&inner)
}

fn sha1_bytes(input: &[u8]) -> [u8; MYSQL_NATIVE_HASH_LEN] {
    use sha1::Digest;
    let digest = sha1::Sha1::digest(input);
    let mut out = [0u8; MYSQL_NATIVE_HASH_LEN];
    out.copy_from_slice(&digest);
    out
}

fn sha256_bytes(input: &[u8]) -> [u8; CACHING_SHA2_HASH_LEN] {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(input);
    let mut out = [0u8; CACHING_SHA2_HASH_LEN];
    out.copy_from_slice(&digest);
    out
}

#[derive(Debug, Clone, Default)]
pub struct UserStore {
    users: BTreeMap<String, UserRecord>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UserError {
    Exists,
    NotFound,
    InvalidRole,
    EmptyName,
    EmptyPassword,
}

impl core::fmt::Display for UserError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exists => f.write_str("user already exists"),
            Self::NotFound => f.write_str("user not found"),
            Self::InvalidRole => {
                f.write_str("invalid role (expected admin / readwrite / readonly)")
            }
            Self::EmptyName => f.write_str("username must be non-empty"),
            Self::EmptyPassword => f.write_str("password must be non-empty"),
        }
    }
}

impl UserStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.users.contains_key(name)
    }

    /// Stable iteration in name order — used by SHOW USERS and the
    /// snapshot writer.
    /// v7.17.0 Phase 3.P0-71: look up a user by name. Returns
    /// `None` for unknown names; the caller decides whether to
    /// surface "Access Denied" or "User not found" (the
    /// MySQL-wire shim picks the former to avoid leaking user
    /// existence to unauthenticated clients).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&UserRecord> {
        self.users.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &UserRecord)> {
        self.users.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn create(
        &mut self,
        name: &str,
        password: &str,
        role: Role,
        salt: [u8; SALT_LEN],
    ) -> Result<(), UserError> {
        if name.is_empty() {
            return Err(UserError::EmptyName);
        }
        if password.is_empty() {
            return Err(UserError::EmptyPassword);
        }
        if self.users.contains_key(name) {
            return Err(UserError::Exists);
        }
        let hash = derive_hash(&salt, password);
        let mysql_native = Some(compute_mysql_native_hash(password));
        let caching_sha2 = Some(compute_caching_sha2_hash(password));
        self.users.insert(
            name.to_string(),
            UserRecord {
                role,
                salt,
                hash,
                scram: None,
                mysql_native,
                caching_sha2,
            },
        );
        Ok(())
    }

    pub fn drop(&mut self, name: &str) -> Result<(), UserError> {
        self.users
            .remove(name)
            .map(|_| ())
            .ok_or(UserError::NotFound)
    }

    /// v4.8: attach SCRAM-SHA-256 verifier to an existing user.
    /// Called by the engine right after `create` so new users have
    /// both auth paths (legacy BLAKE3 + SCRAM) available. The salt
    /// here is independent of the BLAKE3 hash salt — they serve
    /// different purposes.
    pub fn enable_scram(
        &mut self,
        name: &str,
        password: &str,
        salt: [u8; SCRAM_SALT_LEN],
        iters: u32,
    ) -> Result<(), UserError> {
        let rec = self.users.get_mut(name).ok_or(UserError::NotFound)?;
        rec.scram = Some(compute_scram_secrets(password, salt, iters));
        Ok(())
    }

    pub fn verify(&self, name: &str, password: &str) -> Option<Role> {
        let rec = self.users.get(name)?;
        if rec.verify(password) {
            Some(rec.role)
        } else {
            None
        }
    }
}

fn derive_hash(salt: &[u8; SALT_LEN], password: &str) -> [u8; HASH_LEN] {
    let mut buf = Vec::with_capacity(SALT_LEN + password.len());
    buf.extend_from_slice(salt);
    buf.extend_from_slice(password.as_bytes());
    spg_crypto::hash(&buf)
}

/// v4.8: derive SCRAM-SHA-256 stored credentials per RFC 5802 §3.
///
/// `SaltedPassword` = `PBKDF2(password, salt, iters)`
/// `ClientKey`      = `HMAC(SaltedPassword, "Client Key")`
/// `StoredKey`      = `SHA-256(ClientKey)`
/// `ServerKey`      = `HMAC(SaltedPassword, "Server Key")`
///
/// PG-wire keeps the `StoredKey` + `ServerKey` on disk; verifying a
/// client SCRAM proof needs only the `StoredKey` (no plaintext
/// password ever stored).
pub fn compute_scram_secrets(
    password: &str,
    salt: [u8; SCRAM_SALT_LEN],
    iters: u32,
) -> ScramSecrets {
    let salted = spg_crypto::pbkdf2::pbkdf2_sha256_32(password.as_bytes(), &salt, iters);
    let client_key = spg_crypto::hmac::hmac_sha256(&salted, b"Client Key");
    let stored_key = spg_crypto::sha256::hash(&client_key);
    let server_key = spg_crypto::hmac::hmac_sha256(&salted, b"Server Key");
    ScramSecrets {
        iters,
        salt,
        stored_key,
        server_key,
    }
}

/// Branch-free byte compare so verify timing doesn't leak whether
/// a prefix matched.
fn constant_time_eq(a: &[u8; HASH_LEN], b: &[u8; HASH_LEN]) -> bool {
    let mut diff: u8 = 0;
    for i in 0..HASH_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// v7.17.0 Phase 3.P0-71 — same idea, sized for the SHA-1 digest
/// used by `mysql_native_password`.
fn constant_time_eq_sha1(a: &[u8; MYSQL_NATIVE_HASH_LEN], b: &[u8; MYSQL_NATIVE_HASH_LEN]) -> bool {
    let mut diff: u8 = 0;
    for i in 0..MYSQL_NATIVE_HASH_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---- snapshot encoding ----
//
// Layout (after a magic + version envelope handled at Engine level):
//
// v1 (v4.1.0 — original):
//   [u32 user_count]
//   for each user:
//     [u16 name_len][name][u8 role][16 salt][32 hash]
//
// v2 (v4.8.0 — adds SCRAM):
//   [u8 format_version = 2]    // distinguishes from v1 (where the
//                                 first byte is the LO of user_count
//                                 u32, never 0xff)
//   [u32 user_count]
//   for each user:
//     [u16 name_len][name][u8 role][16 salt][32 hash]
//     [u8 scram_present]       // 0 or 1
//     if scram_present:
//       [u32 iters][16 scram_salt][32 stored_key][32 server_key]
//
// We use byte 0xff as the v2 marker — v1 would have to have ≥
// 4 billion users for its first byte to be 0xff, so the version
// switch is unambiguous.

const SCRAM_FORMAT_MARKER: u8 = 0xff;
/// v7.17.0 Phase 3.P0-71 — v3 format marker. v3 extends v2 by
/// appending an optional `mysql_native_password` SHA1(SHA1(pwd))
/// per user.
const MYSQL_NATIVE_FORMAT_MARKER: u8 = 0xfe;
/// v7.17.0 Phase 3.P0-72 — v4 format marker. v4 extends v3 by
/// also appending an optional `caching_sha2_password`
/// SHA256(SHA256(pwd)) per user. Writer always emits v4;
/// reader understands v1 / v2 / v3 / v4.
const CACHING_SHA2_FORMAT_MARKER: u8 = 0xfd;

pub(crate) fn serialize_users(store: &UserStore) -> Vec<u8> {
    let per_user_floor = 2 + 16 + 1 + SALT_LEN + HASH_LEN + 1 + 1;
    let mut out = Vec::with_capacity(1 + 4 + store.len() * per_user_floor);
    // v7.17.0 Phase 3.P0-72 — bump on-disk format to v4 so the
    // per-user `caching_sha2_password` hash trails the
    // mysql_native block.
    out.push(CACHING_SHA2_FORMAT_MARKER);
    out.extend_from_slice(
        &u32::try_from(store.users.len())
            .expect("≤ 4G users")
            .to_le_bytes(),
    );
    for (name, rec) in &store.users {
        let nl = u16::try_from(name.len()).expect("≤ 65k name");
        out.extend_from_slice(&nl.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(match rec.role {
            Role::Admin => 0,
            Role::ReadWrite => 1,
            Role::ReadOnly => 2,
        });
        out.extend_from_slice(&rec.salt);
        out.extend_from_slice(&rec.hash);
        match &rec.scram {
            None => out.push(0),
            Some(s) => {
                out.push(1);
                out.extend_from_slice(&s.iters.to_le_bytes());
                out.extend_from_slice(&s.salt);
                out.extend_from_slice(&s.stored_key);
                out.extend_from_slice(&s.server_key);
            }
        }
        match &rec.mysql_native {
            None => out.push(0),
            Some(h) => {
                out.push(1);
                out.extend_from_slice(h);
            }
        }
        match &rec.caching_sha2 {
            None => out.push(0),
            Some(h) => {
                out.push(1);
                out.extend_from_slice(h);
            }
        }
    }
    out
}

#[derive(Debug)]
pub enum UserDeserializeError {
    Truncated,
    BadRole(u8),
    InvalidUtf8,
}

impl core::fmt::Display for UserDeserializeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("user blob truncated"),
            Self::BadRole(b) => write!(f, "unknown role byte: {b}"),
            Self::InvalidUtf8 => f.write_str("username not valid UTF-8"),
        }
    }
}

fn take<'a>(p: &mut usize, n: usize, buf: &'a [u8]) -> Result<&'a [u8], UserDeserializeError> {
    if *p + n > buf.len() {
        return Err(UserDeserializeError::Truncated);
    }
    let s = &buf[*p..*p + n];
    *p += n;
    Ok(s)
}

pub(crate) fn deserialize_users(buf: &[u8]) -> Result<UserStore, UserDeserializeError> {
    let mut p = 0usize;
    // v1 → starts with a u32 user_count (LO byte rarely 0xfd /
    // 0xfe / 0xff in practice).
    // v2 → 0xff marker (SCRAM_FORMAT_MARKER) then u32 count.
    // v3 → 0xfe marker (MYSQL_NATIVE_FORMAT_MARKER) then u32
    //      count; per-user payload adds a 1-byte flag + 20-byte
    //      SHA1(SHA1(pwd)) tail for `mysql_native_password`.
    // v4 → 0xfd marker (CACHING_SHA2_FORMAT_MARKER) then u32
    //      count; per-user payload further adds 1-byte flag +
    //      32-byte SHA256(SHA256(pwd)) tail for
    //      `caching_sha2_password`.
    let (scram_present_inline, mysql_native_present_inline, caching_sha2_present_inline) =
        if !buf.is_empty() && buf[0] == CACHING_SHA2_FORMAT_MARKER {
            p += 1;
            (true, true, true)
        } else if !buf.is_empty() && buf[0] == MYSQL_NATIVE_FORMAT_MARKER {
            p += 1;
            (true, true, false)
        } else if !buf.is_empty() && buf[0] == SCRAM_FORMAT_MARKER {
            p += 1;
            (true, false, false)
        } else {
            (false, false, false)
        };
    let count_bytes = take(&mut p, 4, buf)?;
    let count = u32::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
    let mut store = UserStore::new();
    for _ in 0..count {
        let nl_bytes = take(&mut p, 2, buf)?;
        let nl = u16::from_le_bytes(nl_bytes.try_into().unwrap()) as usize;
        let name_bytes = take(&mut p, nl, buf)?;
        let name = core::str::from_utf8(name_bytes)
            .map_err(|_| UserDeserializeError::InvalidUtf8)?
            .to_string();
        let role_byte = take(&mut p, 1, buf)?[0];
        let role = match role_byte {
            0 => Role::Admin,
            1 => Role::ReadWrite,
            2 => Role::ReadOnly,
            b => return Err(UserDeserializeError::BadRole(b)),
        };
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(take(&mut p, SALT_LEN, buf)?);
        let mut hash = [0u8; HASH_LEN];
        hash.copy_from_slice(take(&mut p, HASH_LEN, buf)?);
        let scram = if scram_present_inline {
            let flag = take(&mut p, 1, buf)?[0];
            if flag == 1 {
                let iters_bytes = take(&mut p, 4, buf)?;
                let iters = u32::from_le_bytes(iters_bytes.try_into().unwrap());
                let mut s_salt = [0u8; SCRAM_SALT_LEN];
                s_salt.copy_from_slice(take(&mut p, SCRAM_SALT_LEN, buf)?);
                let mut stored_key = [0u8; HASH_LEN];
                stored_key.copy_from_slice(take(&mut p, HASH_LEN, buf)?);
                let mut server_key = [0u8; HASH_LEN];
                server_key.copy_from_slice(take(&mut p, HASH_LEN, buf)?);
                Some(ScramSecrets {
                    iters,
                    salt: s_salt,
                    stored_key,
                    server_key,
                })
            } else {
                None
            }
        } else {
            None
        };
        let mysql_native = if mysql_native_present_inline {
            let flag = take(&mut p, 1, buf)?[0];
            if flag == 1 {
                let mut h = [0u8; MYSQL_NATIVE_HASH_LEN];
                h.copy_from_slice(take(&mut p, MYSQL_NATIVE_HASH_LEN, buf)?);
                Some(h)
            } else {
                None
            }
        } else {
            None
        };
        let caching_sha2 = if caching_sha2_present_inline {
            let flag = take(&mut p, 1, buf)?[0];
            if flag == 1 {
                let mut h = [0u8; CACHING_SHA2_HASH_LEN];
                h.copy_from_slice(take(&mut p, CACHING_SHA2_HASH_LEN, buf)?);
                Some(h)
            } else {
                None
            }
        } else {
            None
        };
        store.users.insert(
            name,
            UserRecord {
                role,
                salt,
                hash,
                scram,
                mysql_native,
                caching_sha2,
            },
        );
    }
    if p != buf.len() {
        return Err(UserDeserializeError::Truncated);
    }
    Ok(store)
}

impl Engine {
    /// v4.1 `SHOW USERS` — `(name, role)` per row, ordered by name.
    pub(crate) fn exec_show_users(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("role", DataType::Text, false),
        ];
        let rows: Vec<Row> = self
            .users
            .iter()
            .map(|(name, rec)| {
                Row::new(alloc::vec![
                    Value::Text(name.to_string()),
                    Value::Text(rec.role.as_str().to_string()),
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }
    /// `salt` is supplied by the caller (the host has a random
    /// source; the engine is `no_std`). Caller should pass a fresh
    /// 16-byte random value per user.
    pub fn create_user(
        &mut self,
        name: &str,
        password: &str,
        role: Role,
        salt: [u8; 16],
    ) -> Result<(), UserError> {
        self.users.create(name, password, role, salt)?;
        // v4.8: also derive SCRAM-SHA-256 secrets so PG-wire SASL
        // auth can verify without re-running PBKDF2 per attempt.
        // Uses a fresh salt from the host RNG (falls back to a
        // deterministic per-username salt when no RNG is wired, same
        // as the legacy hash path).
        let scram_salt = self.salt_fn.map_or_else(
            || {
                let mut s = [0u8; SCRAM_SALT_LEN];
                let digest = spg_crypto::hash(name.as_bytes());
                // Use bytes 16..32 of BLAKE3 so we don't reuse the
                // exact same fallback salt as the BLAKE3 hash path.
                s.copy_from_slice(&digest[16..32]);
                s
            },
            |f| f(),
        );
        self.users
            .enable_scram(name, password, scram_salt, SCRAM_DEFAULT_ITERS)?;
        Ok(())
    }

    pub fn drop_user(&mut self, name: &str) -> Result<(), UserError> {
        self.users.drop(name)
    }

    pub fn verify_user(&self, name: &str, password: &str) -> Option<Role> {
        self.users.verify(name, password)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_verify_succeeds_with_right_password_only() {
        let mut s = UserStore::new();
        s.create("alice", "hunter2", Role::Admin, [1; SALT_LEN])
            .unwrap();
        assert_eq!(s.verify("alice", "hunter2"), Some(Role::Admin));
        assert_eq!(s.verify("alice", "wrong"), None);
        assert_eq!(s.verify("bob", "hunter2"), None);
    }

    #[test]
    fn create_duplicate_user_is_rejected() {
        let mut s = UserStore::new();
        s.create("a", "p", Role::ReadOnly, [0; SALT_LEN]).unwrap();
        assert_eq!(
            s.create("a", "p2", Role::Admin, [0; SALT_LEN]),
            Err(UserError::Exists)
        );
    }

    #[test]
    fn drop_user_removes_them() {
        let mut s = UserStore::new();
        s.create("a", "p", Role::Admin, [0; SALT_LEN]).unwrap();
        s.drop("a").unwrap();
        assert!(s.is_empty());
        assert_eq!(s.drop("a"), Err(UserError::NotFound));
    }

    #[test]
    fn role_parse_accepts_aliases() {
        assert_eq!(Role::parse("ADMIN"), Some(Role::Admin));
        assert_eq!(Role::parse("rw"), Some(Role::ReadWrite));
        assert_eq!(Role::parse("ro"), Some(Role::ReadOnly));
        assert_eq!(Role::parse("god"), None);
    }

    #[test]
    fn snapshot_round_trip_preserves_users_and_verify() {
        let mut s = UserStore::new();
        s.create("alice", "pw1", Role::Admin, [7; SALT_LEN])
            .unwrap();
        s.create("bob", "pw2", Role::ReadOnly, [13; SALT_LEN])
            .unwrap();
        let bytes = serialize_users(&s);
        let s2 = deserialize_users(&bytes).unwrap();
        assert_eq!(s2.len(), 2);
        assert_eq!(s2.verify("alice", "pw1"), Some(Role::Admin));
        assert_eq!(s2.verify("bob", "pw2"), Some(Role::ReadOnly));
        assert_eq!(s2.verify("bob", "wrong"), None);
    }

    #[test]
    fn empty_store_round_trip() {
        // v7.17.0 Phase 3.P0-72: writer flipped to v4 marker (0xfd).
        let s = UserStore::new();
        let bytes = serialize_users(&s);
        assert_eq!(bytes, [0xfd, 0, 0, 0, 0]);
        let s2 = deserialize_users(&bytes).unwrap();
        assert!(s2.is_empty());
    }

    #[test]
    fn v2_blob_still_loads_with_mysql_native_none() {
        // v7.17.0 Phase 3.P0-71: cross-version compat — readers
        // must still parse v2-shaped blobs written before the v3
        // bump and surface `mysql_native = None` for those
        // users so the operator knows to reset the password.
        let mut buf = Vec::new();
        buf.push(0xff); // v2 marker
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(b"old");
        buf.push(0); // role = admin
        buf.extend_from_slice(&[1u8; SALT_LEN]);
        buf.extend_from_slice(&[2u8; HASH_LEN]);
        buf.push(0); // no SCRAM
        let s = deserialize_users(&buf).unwrap();
        let rec = s.get("old").expect("v2 user loads");
        assert!(rec.mysql_native().is_none());
    }
}

#[cfg(test)]
mod p0_71_tests {
    use super::*;

    #[test]
    fn create_populates_mysql_native_hash() {
        let mut s = UserStore::new();
        s.create("alice", "wonderland", Role::Admin, [9u8; SALT_LEN])
            .unwrap();
        let rec = s.get("alice").unwrap();
        let expected = compute_mysql_native_hash("wonderland");
        assert_eq!(rec.mysql_native(), Some(&expected));
    }

    #[test]
    fn verify_mysql_native_password_accepts_correct_response() {
        // Build a fake scramble + the canonical client response.
        let mut s = UserStore::new();
        s.create("bob", "secret", Role::Admin, [3u8; SALT_LEN])
            .unwrap();
        let rec = s.get("bob").unwrap();
        let scramble: [u8; 20] = core::array::from_fn(|i| (i as u8).wrapping_mul(7));
        // Compute the same response the client would.
        let sha1_pwd = sha1_bytes(b"secret");
        let sha1_sha1_pwd = sha1_bytes(&sha1_pwd);
        let mut concat = [0u8; 40];
        concat[..20].copy_from_slice(&scramble);
        concat[20..].copy_from_slice(&sha1_sha1_pwd);
        let mask = sha1_bytes(&concat);
        let response: [u8; MYSQL_NATIVE_HASH_LEN] = core::array::from_fn(|i| sha1_pwd[i] ^ mask[i]);
        assert!(rec.verify_mysql_native_password(&scramble, &response));
        // Tamper with one byte → rejected.
        let mut bad = response;
        bad[0] ^= 1;
        assert!(!rec.verify_mysql_native_password(&scramble, &bad));
    }

    #[test]
    fn v4_serialise_round_trips_both_mysql_native_and_caching_sha2() {
        let mut s = UserStore::new();
        s.create("alice", "wonderland", Role::Admin, [4u8; SALT_LEN])
            .unwrap();
        let bytes = serialize_users(&s);
        assert_eq!(bytes[0], 0xfd, "v4 marker advertised");
        let s2 = deserialize_users(&bytes).unwrap();
        let r1 = s.get("alice").unwrap();
        let r2 = s2.get("alice").unwrap();
        assert_eq!(r1.mysql_native(), r2.mysql_native());
        assert_eq!(r1.caching_sha2(), r2.caching_sha2());
        // Sanity: both verifiers are populated for a fresh user.
        assert!(r1.mysql_native().is_some());
        assert!(r1.caching_sha2().is_some());
    }

    #[test]
    fn v3_blob_still_loads_with_caching_sha2_none() {
        // v7.17.0 Phase 3.P0-72: backward compat — readers must
        // still parse v3-shaped blobs (mysql_native present,
        // caching_sha2 missing) written before the v4 bump.
        let mut buf = Vec::new();
        buf.push(0xfe); // v3 marker
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&5u16.to_le_bytes());
        buf.extend_from_slice(b"older");
        buf.push(0); // role = admin
        buf.extend_from_slice(&[1u8; SALT_LEN]);
        buf.extend_from_slice(&[2u8; HASH_LEN]);
        buf.push(0); // no SCRAM
        buf.push(1); // mysql_native flag = present
        buf.extend_from_slice(&[3u8; MYSQL_NATIVE_HASH_LEN]);
        let s = deserialize_users(&buf).unwrap();
        let rec = s.get("older").unwrap();
        assert!(rec.mysql_native().is_some());
        assert!(rec.caching_sha2().is_none());
    }

    #[test]
    fn verify_caching_sha2_password_accepts_correct_response() {
        let mut s = UserStore::new();
        s.create("bob", "secret", Role::Admin, [3u8; SALT_LEN])
            .unwrap();
        let rec = s.get("bob").unwrap();
        let scramble: [u8; 20] = core::array::from_fn(|i| (i as u8).wrapping_mul(11));
        let sha_pwd = sha256_bytes(b"secret");
        let sha_sha_pwd = sha256_bytes(&sha_pwd);
        let mut concat = [0u8; 20 + CACHING_SHA2_HASH_LEN];
        concat[..20].copy_from_slice(&scramble);
        concat[20..].copy_from_slice(&sha_sha_pwd);
        let mask = sha256_bytes(&concat);
        let response: [u8; CACHING_SHA2_HASH_LEN] = core::array::from_fn(|i| sha_pwd[i] ^ mask[i]);
        assert!(rec.verify_caching_sha2_password(&scramble, &response));
        let mut bad = response;
        bad[0] ^= 1;
        assert!(!rec.verify_caching_sha2_password(&scramble, &bad));
    }

    #[test]
    fn old_v1_user_blob_still_loads() {
        // Hand-constructed v1 blob: 1 user, no SCRAM byte.
        // [u32 count=1][u16 name_len=3]["bob"][u8 role=0][16 salt][32 hash]
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes());
        buf.extend_from_slice(b"bob");
        buf.push(0); // role = admin
        buf.extend_from_slice(&[7u8; SALT_LEN]);
        buf.extend_from_slice(&[42u8; HASH_LEN]);
        let s = deserialize_users(&buf).expect("v1 blob must still load");
        assert_eq!(s.len(), 1);
        let (n, rec) = s.iter().next().unwrap();
        assert_eq!(n, "bob");
        assert_eq!(rec.role, Role::Admin);
        assert!(rec.scram().is_none(), "v1 users have no SCRAM secrets");
    }

    #[test]
    fn scram_round_trip_preserves_iters_salt_keys() {
        let mut s = UserStore::new();
        s.create("alice", "pw", Role::Admin, [3; SALT_LEN]).unwrap();
        s.enable_scram("alice", "pw", [9; SCRAM_SALT_LEN], 4096)
            .unwrap();
        let bytes = serialize_users(&s);
        let s2 = deserialize_users(&bytes).unwrap();
        let (_, rec) = s2.iter().next().unwrap();
        let scram = rec.scram().expect("scram must round-trip");
        assert_eq!(scram.iters, 4096);
        assert_eq!(scram.salt, [9u8; SCRAM_SALT_LEN]);
        // StoredKey and ServerKey are deterministic given (password,
        // salt, iters); verify by recomputing.
        let expected = compute_scram_secrets("pw", [9; SCRAM_SALT_LEN], 4096);
        assert_eq!(scram.stored_key, expected.stored_key);
        assert_eq!(scram.server_key, expected.server_key);
    }

    #[test]
    fn deserialize_truncation_is_caught() {
        assert!(deserialize_users(&[]).is_err());
        assert!(deserialize_users(&[0, 0, 0]).is_err());
    }
}
