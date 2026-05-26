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

const SALT_LEN: usize = 16;
const HASH_LEN: usize = 32;

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
}

impl UserRecord {
    pub fn verify(&self, password: &str) -> bool {
        let candidate = derive_hash(&self.salt, password);
        constant_time_eq(&candidate, &self.hash)
    }
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
        self.users
            .insert(name.to_string(), UserRecord { role, salt, hash });
        Ok(())
    }

    pub fn drop(&mut self, name: &str) -> Result<(), UserError> {
        self.users
            .remove(name)
            .map(|_| ())
            .ok_or(UserError::NotFound)
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

/// Branch-free byte compare so verify timing doesn't leak whether
/// a prefix matched.
fn constant_time_eq(a: &[u8; HASH_LEN], b: &[u8; HASH_LEN]) -> bool {
    let mut diff: u8 = 0;
    for i in 0..HASH_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---- snapshot encoding ----
//
// Layout (after a magic + version envelope handled at Engine level):
//   [u32 user_count]
//   for each user:
//     [u16 name_len][name bytes]
//     [u8 role]                ; 0=admin, 1=readwrite, 2=readonly
//     [16 bytes salt]
//     [32 bytes hash]

pub(crate) fn serialize_users(store: &UserStore) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + store.len() * (2 + 16 + 1 + SALT_LEN + HASH_LEN));
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
        store.users.insert(name, UserRecord { role, salt, hash });
    }
    if p != buf.len() {
        return Err(UserDeserializeError::Truncated);
    }
    Ok(store)
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
    fn empty_store_round_trip_is_a_4_byte_blob() {
        let s = UserStore::new();
        let bytes = serialize_users(&s);
        assert_eq!(bytes, [0u8; 4]);
        let s2 = deserialize_users(&bytes).unwrap();
        assert!(s2.is_empty());
    }

    #[test]
    fn deserialize_truncation_is_caught() {
        assert!(deserialize_users(&[]).is_err());
        assert!(deserialize_users(&[0, 0, 0]).is_err());
    }
}
