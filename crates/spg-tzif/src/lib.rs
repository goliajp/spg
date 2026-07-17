//! IANA timezone database reader for SPG.
//!
//! Clean-room implementation of the RFC 8536 TZif file format (v1/v2/v3)
//! plus POSIX TZ rule evaluation for the footer string — modern "slim"
//! tzdata files carry explicit transitions only up to the point the
//! current rule was adopted; every later instant is governed by the
//! footer rule, so evaluating it is a correctness requirement, not an
//! optimisation.
//!
//! Data comes from the system zoneinfo directory (`/usr/share/zoneinfo`
//! on macOS and Linux — the same IANA data PostgreSQL compiles in, so
//! differential runs agree). No zoneinfo on the host degrades honestly:
//! named zones fail to resolve (SET reports PG's invalid-parameter
//! error) while fixed offsets and UTC keep working.
//!
//! The engine is `no_std` and receives this functionality through
//! injected fn pointers (see `spg-engine`'s `TzOffsetFn` family); this
//! crate is the std-side implementation shared by spg-server and
//! spg-embedded. Zero external dependencies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// One resolved local-time type: seconds east of UTC + DST flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalType {
    /// Seconds east of UTC (Tokyo +32400, New York winter -18000).
    pub utoff_secs: i64,
    pub is_dst: bool,
}

/// A parsed zone: explicit transitions (from the TZif data block) plus
/// the optional footer rule for instants beyond the last transition.
#[derive(Debug)]
pub struct Zone {
    /// Transition instants, ascending, in UTC seconds since the epoch.
    transitions: Vec<i64>,
    /// `types[type_idx[i]]` applies from `transitions[i]` (inclusive)
    /// until the next transition.
    type_idx: Vec<usize>,
    types: Vec<LocalType>,
    /// Designation (abbreviation) per type, parallel to `types`
    /// ("JST", "EST", "EDT").
    abbrs: Vec<String>,
    /// Before the first transition (RFC 8536: the first standard-time
    /// type, else type 0).
    first: LocalType,
    first_abbr: String,
    /// Footer POSIX TZ rule, when present and parseable.
    footer: Option<PosixTz>,
}

impl Zone {
    /// The local type in effect at a UTC instant.
    pub fn type_at_utc(&self, utc_secs: i64) -> LocalType {
        self.type_and_abbr_at_utc(utc_secs).0
    }

    /// Local type + designation ("JST", "EDT") at a UTC instant.
    pub fn type_and_abbr_at_utc(&self, utc_secs: i64) -> (LocalType, &str) {
        if let Some(&last) = self.transitions.last()
            && utc_secs >= last
        {
            // Inside or past the last explicit transition: the
            // footer rule governs when present (slim tzdata), else
            // the last explicit type extends forever.
            if let Some(f) = &self.footer {
                return f.type_and_abbr_at_utc(utc_secs);
            }
            let i = self.transitions.len() - 1;
            let ti = self.type_idx[i];
            return (self.types[ti], &self.abbrs[ti]);
        }
        match self.transitions.partition_point(|&t| t <= utc_secs) {
            0 => {
                if self.transitions.is_empty()
                    && let Some(f) = &self.footer
                {
                    return f.type_and_abbr_at_utc(utc_secs);
                }
                (self.first, &self.first_abbr)
            }
            n => {
                let ti = self.type_idx[n - 1];
                (self.types[ti], &self.abbrs[ti])
            }
        }
    }

    /// UTC seconds east at a UTC instant.
    pub fn utc_offset(&self, utc_secs: i64) -> i64 {
        self.type_at_utc(utc_secs).utoff_secs
    }

    /// Resolve a local wall-clock reading to UTC, with PostgreSQL's
    /// disambiguation (verified against the live PG18 oracle):
    /// an ambiguous reading (DST fall-back repeats the hour) prefers
    /// the STANDARD interpretation; a reading inside a spring-forward
    /// gap uses the offset in effect BEFORE the transition.
    pub fn local_to_utc(&self, local_secs: i64) -> i64 {
        // Sample the offsets a day either side of the reading; if they
        // agree there is no nearby transition and the answer is exact.
        let off_before = self.utc_offset(local_secs - 86_400);
        let off_after = self.utc_offset(local_secs + 86_400);
        if off_before == off_after {
            return local_secs - off_before;
        }
        let t_before = LocalType {
            utoff_secs: off_before,
            is_dst: self.type_at_utc(local_secs - 86_400).is_dst,
        };
        let t_after = LocalType {
            utoff_secs: off_after,
            is_dst: self.type_at_utc(local_secs + 86_400).is_dst,
        };
        // A candidate offset is a valid interpretation when converting
        // with it lands on an instant where that offset really applies.
        let valid = |t: LocalType| -> bool {
            let utc = local_secs - t.utoff_secs;
            self.utc_offset(utc) == t.utoff_secs
        };
        match (valid(t_before), valid(t_after)) {
            // Ambiguous (fall back): prefer standard time, tie -> before.
            (true, true) => {
                if t_before.is_dst && !t_after.is_dst {
                    local_secs - t_after.utoff_secs
                } else {
                    local_secs - t_before.utoff_secs
                }
            }
            (true, false) => local_secs - t_before.utoff_secs,
            (false, true) => local_secs - t_after.utoff_secs,
            // Gap (spring forward): the pre-transition offset.
            (false, false) => local_secs - t_before.utoff_secs,
        }
    }
}

// ---- TZif binary parsing (RFC 8536) ----

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be_i32(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be_i64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

struct Counts {
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn parse_header(b: &[u8], at: usize) -> Option<(u8, Counts)> {
    if b.get(at..at + 4)? != b"TZif" {
        return None;
    }
    let version = *b.get(at + 4)?;
    let c = Counts {
        isutcnt: be_u32(b, at + 20)? as usize,
        isstdcnt: be_u32(b, at + 24)? as usize,
        leapcnt: be_u32(b, at + 28)? as usize,
        timecnt: be_u32(b, at + 32)? as usize,
        typecnt: be_u32(b, at + 36)? as usize,
        charcnt: be_u32(b, at + 40)? as usize,
    };
    Some((version, c))
}

fn data_block_len(c: &Counts, time_size: usize) -> usize {
    c.timecnt * time_size          // transition times
        + c.timecnt                // transition type indexes
        + c.typecnt * 6            // local time types
        + c.charcnt                // designations
        + c.leapcnt * (time_size + 4) // leap-second records
        + c.isstdcnt
        + c.isutcnt
}

fn parse_block(b: &[u8], at: usize, c: &Counts, time_size: usize) -> Option<Zone> {
    let mut transitions = Vec::with_capacity(c.timecnt);
    for i in 0..c.timecnt {
        let t = if time_size == 8 {
            be_i64(b, at + i * 8)?
        } else {
            i64::from(be_i32(b, at + i * 4)?)
        };
        transitions.push(t);
    }
    let idx_at = at + c.timecnt * time_size;
    let mut type_idx = Vec::with_capacity(c.timecnt);
    for i in 0..c.timecnt {
        let ti = *b.get(idx_at + i)? as usize;
        if ti >= c.typecnt {
            return None;
        }
        type_idx.push(ti);
    }
    let types_at = idx_at + c.timecnt;
    let mut types = Vec::with_capacity(c.typecnt);
    let mut desig_idx = Vec::with_capacity(c.typecnt);
    for i in 0..c.typecnt {
        let utoff = i64::from(be_i32(b, types_at + i * 6)?);
        let is_dst = *b.get(types_at + i * 6 + 4)? != 0;
        desig_idx.push(*b.get(types_at + i * 6 + 5)? as usize);
        types.push(LocalType {
            utoff_secs: utoff,
            is_dst,
        });
    }
    if types.is_empty() {
        return None;
    }
    // Designations: NUL-terminated strings in a shared byte pool,
    // each type indexing its start.
    let chars_at = types_at + c.typecnt * 6;
    let pool = b.get(chars_at..chars_at + c.charcnt)?;
    let abbrs: Vec<String> = desig_idx
        .iter()
        .map(|&di| {
            let tail = pool.get(di..).unwrap_or(&[]);
            let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
            String::from_utf8_lossy(&tail[..end]).into_owned()
        })
        .collect();
    // RFC 8536: the type in force before the first transition is the
    // first non-DST type, else type 0.
    let first_i = types.iter().position(|t| !t.is_dst).unwrap_or(0);
    Some(Zone {
        transitions,
        type_idx,
        first: types[first_i],
        first_abbr: abbrs[first_i].clone(),
        types,
        abbrs,
        footer: None,
    })
}

/// Parse a whole TZif file. Version 2+ files carry a second, 64-bit
/// data block (used in preference to the 32-bit one) and a trailing
/// POSIX TZ footer line.
pub fn parse_tzif(b: &[u8]) -> Option<Zone> {
    let (version, c1) = parse_header(b, 0)?;
    let v1_end = 44 + data_block_len(&c1, 4);
    if version == 0 {
        return parse_block(b, 44, &c1, 4);
    }
    let (_, c2) = parse_header(b, v1_end)?;
    let block_at = v1_end + 44;
    let mut zone = parse_block(b, block_at, &c2, 8)?;
    // Footer: '\n' TZstring '\n'
    let footer_at = block_at + data_block_len(&c2, 8);
    if b.get(footer_at) == Some(&b'\n')
        && let Some(rest) = b.get(footer_at + 1..)
        && let Some(nl) = rest.iter().position(|&c| c == b'\n')
        && let Ok(tzs) = core::str::from_utf8(&rest[..nl])
    {
        zone.footer = PosixTz::parse(tzs);
    }
    Some(zone)
}

// ---- POSIX TZ rule (footer) evaluation ----

/// Day-of-year rule for a DST boundary.
#[derive(Debug, Clone, Copy)]
enum TzRuleDay {
    /// `Jn` — Julian day 1..365, February 29 never counted.
    JulianNoLeap(u16),
    /// `n` — zero-based day 0..365, Feb 29 counted in leap years.
    ZeroBased(u16),
    /// `Mm.w.d` — month 1..12, week 1..5 (5 = last), weekday 0=Sunday.
    MonthWeekDay(u8, u8, u8),
}

#[derive(Debug, Clone, Copy)]
struct TzRule {
    day: TzRuleDay,
    /// Local seconds after midnight at which the change occurs
    /// (default 02:00:00; RFC 8536 v3 allows -167h..167h).
    at_local_secs: i64,
}

#[derive(Debug)]
struct PosixTz {
    std: LocalType,
    std_name: String,
    dst: Option<(LocalType, String, TzRule, TzRule)>,
}

impl PosixTz {
    /// Parse `EST5EDT,M3.2.0,M11.1.0` / `JST-9` / `<+0545>-5:45` forms.
    fn parse(s: &str) -> Option<Self> {
        let mut rest = s;
        let std_name = take_name(&mut rest)?.to_string();
        let std_off_west = take_offset(&mut rest)?;
        let std = LocalType {
            utoff_secs: -std_off_west,
            is_dst: false,
        };
        if rest.is_empty() {
            return Some(Self {
                std,
                std_name,
                dst: None,
            });
        }
        let dst_name = take_name(&mut rest)?.to_string();
        // DST offset defaults to one hour ahead of standard.
        let dst_off_secs = if rest.starts_with(',') || rest.is_empty() {
            std.utoff_secs + 3600
        } else {
            -take_offset(&mut rest)?
        };
        let dst = LocalType {
            utoff_secs: dst_off_secs,
            is_dst: true,
        };
        if !rest.starts_with(',') {
            return None; // DST named but no rules — not representable
        }
        rest = &rest[1..];
        let (start, r2) = take_rule(rest)?;
        if !r2.starts_with(',') {
            return None;
        }
        let (end, r3) = take_rule(&r2[1..])?;
        if !r3.is_empty() {
            return None;
        }
        Some(Self {
            std,
            std_name,
            dst: Some((dst, dst_name, start, end)),
        })
    }

    fn type_and_abbr_at_utc(&self, utc_secs: i64) -> (LocalType, &str) {
        let Some((dst, dst_name, start, end)) = &self.dst else {
            return (self.std, &self.std_name);
        };
        let year = civil_from_days(utc_secs.div_euclid(86_400)).0;
        // Boundary instants in UTC for this year: the start rule is
        // read on the standard clock, the end rule on the DST clock.
        let start_utc = rule_utc(start, year, self.std.utoff_secs);
        let end_utc = rule_utc(end, year, dst.utoff_secs);
        let in_dst = if start_utc <= end_utc {
            // Northern hemisphere: DST inside the year.
            utc_secs >= start_utc && utc_secs < end_utc
        } else {
            // Southern hemisphere: DST wraps the new year.
            utc_secs >= start_utc || utc_secs < end_utc
        };
        if in_dst {
            (*dst, dst_name)
        } else {
            (self.std, &self.std_name)
        }
    }
}

fn take_name<'a>(rest: &mut &'a str) -> Option<&'a str> {
    if let Some(stripped) = rest.strip_prefix('<') {
        let end = stripped.find('>')?;
        let name = &stripped[..end];
        *rest = &stripped[end + 1..];
        return Some(name);
    }
    let n = rest
        .find(|c: char| !(c.is_ascii_alphabetic()))
        .unwrap_or(rest.len());
    if n < 3 {
        return None;
    }
    let (name, r) = rest.split_at(n);
    *rest = r;
    Some(name)
}

/// `[+|-]hh[:mm[:ss]]` — POSIX sign convention: positive is WEST.
fn take_offset(rest: &mut &str) -> Option<i64> {
    let mut chars = rest.char_indices().peekable();
    let mut neg = false;
    if let Some(&(_, c)) = chars.peek()
        && (c == '+' || c == '-')
    {
        neg = c == '-';
        chars.next();
    }
    let mut fields = [0i64; 3];
    let mut fi = 0usize;
    let mut any = false;
    let mut consumed = 0usize;
    while let Some(&(i, c)) = chars.peek() {
        if c.is_ascii_digit() {
            fields[fi] = fields[fi] * 10 + i64::from(c as u8 - b'0');
            any = true;
            consumed = i + c.len_utf8();
            chars.next();
        } else if c == ':' && fi < 2 && any {
            fi += 1;
            consumed = i + 1;
            chars.next();
        } else {
            break;
        }
    }
    if !any {
        return None;
    }
    *rest = &rest[consumed..];
    let secs = fields[0] * 3600 + fields[1] * 60 + fields[2];
    Some(if neg { -secs } else { secs })
}

fn take_rule(s: &str) -> Option<(TzRule, &str)> {
    let mut rest = s;
    let day = if let Some(r) = rest.strip_prefix('M') {
        let (m, r) = take_num(r)?;
        let r = r.strip_prefix('.')?;
        let (w, r) = take_num(r)?;
        let r = r.strip_prefix('.')?;
        let (d, r) = take_num(r)?;
        rest = r;
        TzRuleDay::MonthWeekDay(
            u8::try_from(m).ok()?,
            u8::try_from(w).ok()?,
            u8::try_from(d).ok()?,
        )
    } else if let Some(r) = rest.strip_prefix('J') {
        let (n, r) = take_num(r)?;
        rest = r;
        TzRuleDay::JulianNoLeap(u16::try_from(n).ok()?)
    } else {
        let (n, r) = take_num(rest)?;
        rest = r;
        TzRuleDay::ZeroBased(u16::try_from(n).ok()?)
    };
    let at_local_secs = if let Some(r) = rest.strip_prefix('/') {
        let mut rr = r;
        let secs = take_offset(&mut rr)?; // reuses hh[:mm[:ss]] with sign
        rest = rr;
        secs
    } else {
        2 * 3600
    };
    Some((TzRule { day, at_local_secs }, rest))
}

fn take_num(s: &str) -> Option<(u32, &str)> {
    let n = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if n == 0 {
        return None;
    }
    Some((s[..n].parse().ok()?, &s[n..]))
}

/// UTC instant of a rule boundary in `year`, where the rule's local
/// clock runs at `local_off_secs` east of UTC.
fn rule_utc(rule: &TzRule, year: i32, local_off_secs: i64) -> i64 {
    let day = match rule.day {
        TzRuleDay::JulianNoLeap(n) => {
            // Day 1..365, Feb 29 never counted: day 60 is always Mar 1.
            let n = i64::from(n);
            let jan1 = days_from_civil(year, 1, 1);
            let feb28 = days_from_civil(year, 2, 28);
            let offset_within = n - 1;
            let d = jan1 + offset_within;
            if d > feb28 && is_leap(year) { d + 1 } else { d }
        }
        TzRuleDay::ZeroBased(n) => days_from_civil(year, 1, 1) + i64::from(n),
        TzRuleDay::MonthWeekDay(m, w, dow) => month_week_day(year, m, w, dow),
    };
    day * 86_400 + rule.at_local_secs - local_off_secs
}

/// Day count (unix epoch days) of the `w`-th (5 = last) `dow`
/// (0 = Sunday) of month `m` in `year`.
fn month_week_day(year: i32, m: u8, w: u8, dow: u8) -> i64 {
    let first = days_from_civil(year, u32::from(m), 1);
    // 1970-01-01 (day 0) was a Thursday = weekday 4 (Sunday = 0).
    let first_dow = (first + 4).rem_euclid(7);
    let mut day = first + (i64::from(dow) - first_dow).rem_euclid(7);
    let days_in = i64::from(days_in_month(year, u32::from(m)));
    let mut count = 1u8;
    while count < w && day + 7 < first + days_in {
        day += 7;
        count += 1;
    }
    day
}

// ---- Civil-calendar helpers (Howard Hinnant's algorithms, as used
// throughout SPG) ----

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y_adj = if m <= 2 {
        i64::from(y) - 1
    } else {
        i64::from(y)
    };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let doy = i64::from((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

// ---- The database over the system zoneinfo directory ----

/// Candidate zoneinfo roots, first that exists wins.
const ZONEINFO_ROOTS: &[&str] = &["/usr/share/zoneinfo", "/usr/lib/zoneinfo", "/etc/zoneinfo"];

#[derive(Debug)]
pub struct TzDb {
    root: PathBuf,
    /// lowercase name -> canonical name ("asia/tokyo" -> "Asia/Tokyo").
    names: HashMap<String, String>,
    cache: Mutex<HashMap<String, Arc<Zone>>>,
}

impl TzDb {
    /// Open the system zoneinfo directory; `None` when the host ships
    /// no tzdata (named zones then fail to resolve, honestly).
    pub fn open_system() -> Option<Self> {
        let root = ZONEINFO_ROOTS
            .iter()
            .map(Path::new)
            .find(|p| p.is_dir())?
            .to_path_buf();
        let mut names = HashMap::new();
        collect_names(&root, &root, &mut names);
        if names.is_empty() {
            return None;
        }
        Some(Self {
            root,
            names,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Canonical spelling of a zone name, case-insensitively
    /// (`asia/tokyo` -> `Asia/Tokyo`). `None` = unknown zone.
    pub fn canonical(&self, name: &str) -> Option<&str> {
        self.names
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Load (and cache) a zone by any-case name.
    pub fn zone(&self, name: &str) -> Option<Arc<Zone>> {
        let canon = self.canonical(name)?.to_string();
        if let Ok(cache) = self.cache.lock()
            && let Some(z) = cache.get(&canon)
        {
            return Some(Arc::clone(z));
        }
        let bytes = std::fs::read(self.root.join(&canon)).ok()?;
        let zone = Arc::new(parse_tzif(&bytes)?);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(canon, Arc::clone(&zone));
        }
        Some(zone)
    }
}

/// Recursively collect TZif files under `root`, mapping lowercase
/// relative names to canonical ones. Skips the non-zone auxiliary
/// files the directory also carries.
fn collect_names(root: &Path, dir: &Path, out: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Auxiliary non-zone entries present in zoneinfo trees.
        if matches!(
            fname,
            "posixrules"
                | "localtime"
                | "leapseconds"
                | "tzdata.zi"
                | "zone.tab"
                | "zone1970.tab"
                | "iso3166.tab"
                | "leap-seconds.list"
                | "SECURITY"
                | "+VERSION"
        ) {
            continue;
        }
        if path.is_dir() {
            collect_names(root, &path, out);
            continue;
        }
        // A zone file starts with the TZif magic; probing every file
        // would be slow, so trust the tree structure and verify lazily
        // at load time (parse_tzif rejects non-TZif bytes).
        if let Ok(rel) = path.strip_prefix(root)
            && let Some(rel) = rel.to_str()
        {
            out.insert(rel.to_ascii_lowercase(), rel.to_string());
        }
    }
}

/// Process-wide database handle for fn-pointer injection into the
/// no_std engine (which cannot hold the Arc itself).
static GLOBAL_DB: OnceLock<Option<TzDb>> = OnceLock::new();

fn global_db() -> Option<&'static TzDb> {
    GLOBAL_DB.get_or_init(TzDb::open_system).as_ref()
}

/// `TzOffsetFn`-shaped: seconds -> micros handled by the caller; this
/// takes and returns MICROSECONDS to match the engine's timestamp unit.
pub fn tz_offset_at(zone: &str, utc_micros: i64) -> Option<i64> {
    let z = global_db()?.zone(zone)?;
    Some(z.utc_offset(utc_micros.div_euclid(1_000_000)) * 1_000_000)
}

/// `TzLocalizeFn`-shaped: local wall micros -> UTC micros.
pub fn tz_local_to_utc(zone: &str, local_micros: i64) -> Option<i64> {
    let z = global_db()?.zone(zone)?;
    let secs = local_micros.div_euclid(1_000_000);
    let sub = local_micros.rem_euclid(1_000_000);
    Some(z.local_to_utc(secs) * 1_000_000 + sub)
}

/// `TzAbbrevFn`-shaped: designation ("JST", "EDT") at a UTC instant.
pub fn tz_abbrev_at(zone: &str, utc_micros: i64) -> Option<String> {
    let z = global_db()?.zone(zone)?;
    let (_, abbr) = z.type_and_abbr_at_utc(utc_micros.div_euclid(1_000_000));
    Some(abbr.to_string())
}

/// `TzCanonFn`-shaped: canonical zone spelling.
pub fn tz_canonical(zone: &str) -> Option<String> {
    global_db()?.canonical(zone).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> &'static TzDb {
        global_db().expect("system zoneinfo present on dev hosts")
    }

    fn utc_micros(y: i32, m: u32, d: u32, hh: i64, mm: i64) -> i64 {
        (days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60) * 1_000_000
    }

    #[test]
    fn canonicalises_case_insensitively() {
        assert_eq!(db().canonical("asia/tokyo"), Some("Asia/Tokyo"));
        assert_eq!(db().canonical("AMERICA/NEW_YORK"), Some("America/New_York"));
        assert_eq!(db().canonical("Bogus/Zone"), None);
    }

    #[test]
    fn fixed_zone_offset() {
        // Tokyo has no DST: always +09.
        let off = tz_offset_at("Asia/Tokyo", utc_micros(2024, 3, 15, 10, 20)).unwrap();
        assert_eq!(off, 9 * 3_600_000_000);
        // Kathmandu +05:45 — sub-hour offsets survive.
        let off = tz_offset_at("Asia/Katmandu", utc_micros(2024, 3, 15, 10, 20)).unwrap();
        assert_eq!(off, (5 * 3600 + 45 * 60) * 1_000_000);
    }

    #[test]
    fn dst_offsets_per_instant() {
        // New York: -05 in January, -04 in July (2024).
        let jan = tz_offset_at("America/New_York", utc_micros(2024, 1, 15, 12, 0)).unwrap();
        let jul = tz_offset_at("America/New_York", utc_micros(2024, 7, 15, 12, 0)).unwrap();
        assert_eq!(jan, -5 * 3_600_000_000);
        assert_eq!(jul, -4 * 3_600_000_000);
        // A FUTURE year exercises the POSIX footer on slim tzdata.
        let jan = tz_offset_at("America/New_York", utc_micros(2085, 1, 15, 12, 0)).unwrap();
        let jul = tz_offset_at("America/New_York", utc_micros(2085, 7, 15, 12, 0)).unwrap();
        assert_eq!(jan, -5 * 3_600_000_000);
        assert_eq!(jul, -4 * 3_600_000_000);
    }

    #[test]
    fn dst_transition_boundary_2024() {
        // US 2024 spring forward: 2024-03-10 07:00 UTC.
        let before = tz_offset_at("America/New_York", utc_micros(2024, 3, 10, 6, 59)).unwrap();
        let after = tz_offset_at("America/New_York", utc_micros(2024, 3, 10, 7, 0)).unwrap();
        assert_eq!(before, -5 * 3_600_000_000);
        assert_eq!(after, -4 * 3_600_000_000);
    }

    #[test]
    fn local_to_utc_pg_disambiguation() {
        let z = db().zone("America/New_York").unwrap();
        // Unambiguous winter reading: 12:00 local = 17:00 UTC.
        let local = days_from_civil(2024, 1, 15) * 86_400 + 12 * 3600;
        assert_eq!(z.local_to_utc(local), local + 5 * 3600);
        // Fall-back ambiguity (01:30 on 2024-11-03 happens twice):
        // PG prefers the STANDARD reading -> -05 -> 06:30 UTC.
        let local = days_from_civil(2024, 11, 3) * 86_400 + 3600 + 1800;
        assert_eq!(z.local_to_utc(local), local + 5 * 3600);
        // Spring gap (02:30 on 2024-03-10 does not exist): PG applies
        // the pre-transition offset -05 -> 07:30 UTC.
        let local = days_from_civil(2024, 3, 10) * 86_400 + 2 * 3600 + 1800;
        assert_eq!(z.local_to_utc(local), local + 5 * 3600);
    }

    #[test]
    fn southern_hemisphere_dst() {
        // Sydney: +11 in January (DST), +10 in July.
        let jan = tz_offset_at("Australia/Sydney", utc_micros(2024, 1, 15, 12, 0)).unwrap();
        let jul = tz_offset_at("Australia/Sydney", utc_micros(2024, 7, 15, 12, 0)).unwrap();
        assert_eq!(jan, 11 * 3_600_000_000);
        assert_eq!(jul, 10 * 3_600_000_000);
        // Future year via the footer rule.
        let jan = tz_offset_at("Australia/Sydney", utc_micros(2085, 1, 15, 12, 0)).unwrap();
        assert_eq!(jan, 11 * 3_600_000_000);
    }

    #[test]
    fn abbreviations() {
        assert_eq!(
            tz_abbrev_at("Asia/Tokyo", utc_micros(2024, 3, 15, 10, 20)).as_deref(),
            Some("JST")
        );
        assert_eq!(
            tz_abbrev_at("America/New_York", utc_micros(2024, 7, 15, 12, 0)).as_deref(),
            Some("EDT")
        );
        assert_eq!(
            tz_abbrev_at("America/New_York", utc_micros(2085, 1, 15, 12, 0)).as_deref(),
            Some("EST")
        );
    }

    #[test]
    fn posix_tz_string_forms() {
        let p = PosixTz::parse("JST-9").unwrap();
        assert_eq!(p.std.utoff_secs, 9 * 3600);
        assert!(p.dst.is_none());
        let p = PosixTz::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        assert_eq!(p.std.utoff_secs, -5 * 3600);
        assert_eq!(p.dst.as_ref().unwrap().0.utoff_secs, -4 * 3600);
        let p = PosixTz::parse("<+0545>-5:45").unwrap();
        assert_eq!(p.std.utoff_secs, 5 * 3600 + 45 * 60);
    }
}
