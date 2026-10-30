//! Engine state: pairings, pairing requests, the install and wake codes,
//! lockdown and the wake failure caps. It lives in the `engine-state` volume,
//! which `app` never mounts (design `studies/manager-sidecar-security.md`).
//!
//! Every read-modify-write holds an exclusive `flock` ([`Store::lock`]), so the
//! server's threads and an admin command run over `docker exec` never lose
//! each other's writes; every file is replaced atomically ([`write_atomic`]).

use crate::audit::Audit;
use crate::codes::{
    ct_eq, hex, is_token_shaped, mint_symbols, new_handle, new_pairing_id, new_token,
    normalize_code, now, random_bytes, sha256_hex, REQUEST_CODE_LEN,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const REQUEST_TTL: u64 = 10 * 60;
/// Pending requests per address, and in all. A full list refuses new ones
/// until some expire: an outage of adding Managers, never a way in.
pub const REQUESTS_PER_IP: usize = 2;
pub const REQUESTS_MAX: usize = 32;
/// Wake tries per rolling hour from one address before that address pauses.
/// Per address, so one guesser can't keep the owner from waking their site.
pub const WAKE_TRIES_PER_IP: usize = 10;
/// Wake tries per rolling hour from all addresses before wake by code pauses
/// for everyone: the backstop against guessing from many addresses.
pub const WAKE_TRIES_PER_HOUR: usize = 200;
/// The first pause; each further one doubles, up to [`WAKE_PAUSE_MAX`].
pub const WAKE_PAUSE: u64 = 3600;
pub const WAKE_PAUSE_MAX: u64 = 24 * 3600;
/// Addresses the wake counts remember; past this, quiet ones are dropped.
const WAKE_IPS_MAX: usize = 4096;
pub const TOKEN_IDLE_EXPIRY: u64 = 90 * 24 * 3600;
/// `last_used` is rewritten at most this often, so a status poll isn't a write.
const LAST_USED_GRANULARITY: u64 = 3600;

/// A rolling one-hour count of tries.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FailureWindow(VecDeque<u64>);

impl FailureWindow {
    pub fn record(&mut self, at: u64) {
        self.0.push_back(at);
    }
    pub fn count(&mut self, at: u64) -> usize {
        while self
            .0
            .front()
            .is_some_and(|t| at.saturating_sub(*t) >= 3600)
        {
            self.0.pop_front();
        }
        self.0.len()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// A wake pause: until when, and how many in a row (each doubles the next).
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Pause {
    pub until: u64,
    pub count: u32,
}

impl Pause {
    pub fn active(&self, at: u64) -> bool {
        at < self.until
    }
    fn start(&mut self, at: u64) {
        self.count = self.count.saturating_add(1);
        let pause = WAKE_PAUSE
            .saturating_mul(1 << (self.count - 1).min(16))
            .min(WAKE_PAUSE_MAX);
        self.until = at + pause;
    }
}

/// Whether a wake try may go ahead.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WakeGate {
    Go,
    /// This address is paused.
    PausedHere,
    /// Wake by code is paused for everyone.
    Paused,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Flags {
    /// Argon2id PHC string; `None` until an owner sets one.
    pub wake_hash: Option<String>,
    /// Whether the wake code was generated (compared like any code) or the
    /// owner's own (compared as typed).
    pub wake_generated: bool,
    /// Lockdown (D-09): resting with wake by code disabled, pairing closed.
    pub lockdown: bool,
    /// SHA-256 of the unused install code, if one was made.
    pub install_hash: Option<String>,
    /// Wake by code paused for everyone. It ends on its own, so no owner is
    /// shut out for good; the primary or SSH can end it sooner.
    pub wake_pause: Pause,
    /// Every wake try in the last hour, from anywhere.
    pub wake_tries: FailureWindow,
    /// Wake tries and pauses per address, kept on disk so restarting the
    /// engine doesn't reset them.
    pub wake_ip_tries: BTreeMap<String, FailureWindow>,
    pub wake_ip_pause: BTreeMap<String, Pause>,
}

impl Flags {
    pub fn wake_paused(&self, at: u64) -> bool {
        self.wake_pause.active(at)
    }

    /// Count a wake try before its code is checked, so tries sent at once
    /// can't all slip under the cap. A right code then calls
    /// [`Flags::wake_succeeded`]. Returns whether it may go ahead, and whether
    /// it started a pause for everyone.
    pub fn wake_try(&mut self, ip: &str, at: u64) -> (WakeGate, bool) {
        if self.wake_pause.active(at) {
            return (WakeGate::Paused, false);
        }
        if self.wake_ip_pause.get(ip).is_some_and(|p| p.active(at)) {
            return (WakeGate::PausedHere, false);
        }
        self.forget_quiet_ips(at);
        let mine = self.wake_ip_tries.entry(ip.to_string()).or_default();
        mine.record(at);
        if mine.count(at) >= WAKE_TRIES_PER_IP {
            mine.clear();
            self.wake_ip_pause
                .entry(ip.to_string())
                .or_default()
                .start(at);
        }
        self.wake_tries.record(at);
        if self.wake_tries.count(at) >= WAKE_TRIES_PER_HOUR {
            self.wake_tries.clear();
            self.wake_pause.start(at);
            return (WakeGate::Go, true);
        }
        (WakeGate::Go, false)
    }

    /// The right code: this address starts over, and the next pause for
    /// everyone is a short one again.
    pub fn wake_succeeded(&mut self, ip: &str) {
        self.wake_ip_tries.remove(ip);
        self.wake_ip_pause.remove(ip);
        self.wake_pause.count = 0;
    }

    pub fn wake_resumed(&mut self) {
        self.wake_pause = Pause::default();
        self.wake_tries.clear();
        self.wake_ip_tries.clear();
        self.wake_ip_pause.clear();
    }

    fn forget_quiet_ips(&mut self, at: u64) {
        self.wake_ip_tries.retain(|_, w| w.count(at) > 0);
        // A pause is remembered a day past its end, so its doubling counts.
        self.wake_ip_pause
            .retain(|_, p| at < p.until.saturating_add(WAKE_PAUSE_MAX));
        if self.wake_ip_tries.len() > WAKE_IPS_MAX {
            self.wake_ip_tries.clear();
        }
        if self.wake_ip_pause.len() > WAKE_IPS_MAX {
            self.wake_ip_pause.retain(|_, p| p.active(at));
        }
    }
}

/// A Manager asking to be paired (E-02). It gets a key only once the owner
/// types its code into the primary (or over SSH): whoever reaches the site
/// can ask, nobody can let themselves in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    /// Shown on the new Manager's screen; the owner types it to approve. Kept
    /// as is: it lets nothing in by itself.
    pub code: String,
    /// What the new Manager polls with; only its hash is kept.
    pub handle_sha256: String,
    pub name: String,
    pub ip: String,
    pub created: u64,
    /// The pairing (or `ssh`) that approved it.
    pub approved_by: Option<String>,
}

/// What a request list shows: never the code, so the approver has to read it
/// off the new Manager.
#[derive(Debug, Clone, Serialize)]
pub struct RequestView {
    pub name: String,
    pub ip: String,
    pub created: u64,
    pub approved: bool,
}

pub fn request_views(all: &[PendingRequest]) -> Vec<RequestView> {
    all.iter()
        .map(|r| RequestView {
            name: r.name.clone(),
            ip: r.ip.clone(),
            created: r.created,
            approved: r.approved_by.is_some(),
        })
        .collect()
}

#[derive(Debug)]
pub enum Asked {
    /// `(code, handle)`: the new Manager shows the code and polls with the handle.
    Pending(String, String),
    TooMany,
    Closed,
}

#[derive(Debug)]
pub enum Redeemed {
    Paired(Pairing, String),
    /// Wrong code, or none was made. One answer for both.
    Wrong,
    Closed,
}

#[derive(Debug)]
pub enum Collected {
    Waiting,
    Paired(Pairing, String),
    /// Unknown, expired, or approved but the site locked down since.
    Gone,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pairing {
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    pub created: u64,
    pub last_used: u64,
    /// How it was made: `install code`, `approved by <id>` or `approved over ssh`.
    #[serde(default)]
    pub via: String,
    /// The owner's pairing: made with the install code, or named over SSH.
    /// Never promoted on its own and never idle-expired.
    #[serde(default)]
    pub primary: bool,
}

/// What the API and the console show of a pairing: never the hash.
#[derive(Debug, Clone, Serialize)]
pub struct PairingView {
    pub id: String,
    pub name: String,
    pub created: u64,
    pub last_used: u64,
    pub via: String,
    pub primary: bool,
}

pub fn views(all: &[Pairing]) -> Vec<PairingView> {
    all.iter()
        .map(|p| PairingView {
            id: p.id.clone(),
            name: p.name.clone(),
            created: p.created,
            last_used: p.last_used,
            via: p.via.clone(),
            primary: p.primary,
        })
        .collect()
}

/// Seniority: the primary first, then by age (order in the store). Lower is
/// more senior.
fn rank(all: &[Pairing], i: usize) -> usize {
    if all[i].primary {
        0
    } else {
        i + 1
    }
}

/// Who asks for a revoke. An attacker who gets a pairing (an owner fooled into
/// approving its code) is always newer than the owner's, so it can't remove
/// the owner or survive a lockdown (floor 9).
pub enum Revoker<'a> {
    /// A paired Manager: itself, or any pairing junior to it.
    Pairing(&'a str),
    /// The console, through the inbox: anything but the primary, since
    /// whoever runs code in Drupal writes the inbox.
    Console,
    /// An owner over `docker exec`: anything.
    Ssh,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Revoked {
    Yes,
    NotFound,
    Refused(&'static str),
}

/// The `engine-state` directory.
pub struct Store {
    dir: PathBuf,
    pub audit: Audit,
}

/// The state, locked: every method reads and writes under the lock.
pub struct Locked<'a> {
    store: &'a Store,
    _lock: File,
}

impl Store {
    pub fn at(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Self {
            audit: Audit::at(&dir),
            dir,
        })
    }

    pub fn lock(&self) -> Result<Locked<'_>> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.dir.join(".lock"))
            .context("open the engine state lock")?;
        // SAFETY: a valid open descriptor; flock has no memory effects.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("lock the engine state");
        }
        Ok(Locked {
            store: self,
            _lock: f,
        })
    }

    /// Shorthand for [`Audit::write`].
    pub fn audit(&self, actor: &str, ip: &str, verb: &str, outcome: &str) {
        self.audit.write(actor, ip, verb, outcome);
    }
}

impl Locked<'_> {
    fn read<T: for<'de> Deserialize<'de> + Default>(&self, name: &str) -> Result<T> {
        match fs::read_to_string(self.store.dir.join(name)) {
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse {name}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(e).with_context(|| format!("read {name}")),
        }
    }

    pub(crate) fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        write_atomic(
            &self.store.dir,
            name,
            &serde_json::to_vec_pretty(value)?,
            0o600,
        )
    }

    pub fn flags(&self) -> Result<Flags> {
        self.read("engine.json")
    }

    pub fn set_flags(&self, f: &Flags) -> Result<()> {
        self.write("engine.json", f)
    }

    /// Change the flags under this lock.
    pub fn update_flags<R>(&self, change: impl FnOnce(&mut Flags) -> R) -> Result<R> {
        let mut f = self.flags()?;
        let r = change(&mut f);
        self.set_flags(&f)?;
        Ok(r)
    }

    pub fn pairings(&self) -> Result<Vec<Pairing>> {
        let mut all: Vec<Pairing> = self.read("pairings.json")?;
        let at = now();
        let before = all.len();
        // The primary never idles out: a rogue pairing that keeps polling
        // would otherwise outlast the owner's Manager.
        all.retain(|p| p.primary || at.saturating_sub(p.last_used) < TOKEN_IDLE_EXPIRY);
        if all.len() != before {
            self.write("pairings.json", &all)?;
        }
        Ok(all)
    }

    /// Add a pairing, or `None` while locked down. The lockdown flag is read
    /// under the same lock, so no pairing slips in after a lockdown's revoke.
    /// A new primary leaves the old one as an ordinary pairing.
    pub(crate) fn add_pairing(
        &self,
        name: &str,
        primary: bool,
        via: &str,
    ) -> Result<Option<(Pairing, String)>> {
        if self.flags()?.lockdown {
            return Ok(None);
        }
        let mut all = self.pairings()?;
        let token = new_token();
        let at = now();
        let p = Pairing {
            id: new_pairing_id(),
            name: name.to_string(),
            token_sha256: sha256_hex(&token),
            created: at,
            last_used: at,
            via: via.to_string(),
            primary,
        };
        if primary {
            all.iter_mut().for_each(|o| o.primary = false);
        }
        all.push(p.clone());
        self.write("pairings.json", &all)?;
        Ok(Some((p, token)))
    }

    /// Over SSH only: store a new install code's hash, replacing any unused one.
    pub fn set_install_code(&self, code: &str) -> Result<()> {
        let hash = sha256_hex(&normalize_code(code));
        self.update_flags(|f| f.install_hash = Some(hash))
    }

    /// Pair with the install code: the new pairing is the primary, and the
    /// code is spent. A wrong code burns nothing.
    pub fn redeem_install_code(&self, typed: &str, name: &str) -> Result<Redeemed> {
        let mut f = self.flags()?;
        let typed = sha256_hex(&normalize_code(typed));
        let right = f
            .install_hash
            .as_deref()
            .is_some_and(|h| ct_eq(typed.as_bytes(), h.as_bytes()));
        if !right {
            return Ok(Redeemed::Wrong);
        }
        if f.lockdown {
            return Ok(Redeemed::Closed);
        }
        f.install_hash = None;
        self.set_flags(&f)?;
        Ok(match self.add_pairing(name, true, "install code")? {
            Some((p, token)) => Redeemed::Paired(p, token),
            None => Redeemed::Closed,
        })
    }

    /// Pending requests, expired ones dropped.
    pub fn requests(&self) -> Result<Vec<PendingRequest>> {
        let mut all: Vec<PendingRequest> = self.read("requests.json")?;
        let at = now();
        let before = all.len();
        all.retain(|r| at.saturating_sub(r.created) < REQUEST_TTL);
        if all.len() != before {
            self.write("requests.json", &all)?;
        }
        Ok(all)
    }

    /// A Manager asks to be paired.
    pub fn ask(&self, name: &str, ip: &str) -> Result<Asked> {
        if self.flags()?.lockdown {
            return Ok(Asked::Closed);
        }
        let mut all = self.requests()?;
        if all.len() >= REQUESTS_MAX || all.iter().filter(|r| r.ip == ip).count() >= REQUESTS_PER_IP
        {
            return Ok(Asked::TooMany);
        }
        // Unique among pending ones, so a code names exactly one request.
        let code = loop {
            let c = mint_symbols(REQUEST_CODE_LEN);
            if !all.iter().any(|r| r.code == c) {
                break c;
            }
        };
        let handle = new_handle();
        all.push(PendingRequest {
            code: code.clone(),
            handle_sha256: sha256_hex(&handle),
            name: name.to_string(),
            ip: ip.to_string(),
            created: now(),
            approved_by: None,
        });
        self.write("requests.json", &all)?;
        Ok(Asked::Pending(code, handle))
    }

    /// The owner typed a new Manager's code. Returns that request, or `None`
    /// if no pending, unapproved one has it.
    pub fn approve(&self, typed: &str, by: &str) -> Result<Option<PendingRequest>> {
        let typed = normalize_code(typed);
        let mut all = self.requests()?;
        let Some(r) = all
            .iter_mut()
            .find(|r| r.approved_by.is_none() && ct_eq(r.code.as_bytes(), typed.as_bytes()))
        else {
            return Ok(None);
        };
        r.approved_by = Some(by.to_string());
        let r = r.clone();
        self.write("requests.json", &all)?;
        Ok(Some(r))
    }

    /// The new Manager polls. Once approved, the pairing is made here, so its
    /// token never sits in the engine waiting to be collected.
    pub fn collect(&self, handle: &str) -> Result<Collected> {
        let presented = sha256_hex(handle);
        let mut all = self.requests()?;
        let mut hit = None;
        for (i, r) in all.iter().enumerate() {
            if ct_eq(r.handle_sha256.as_bytes(), presented.as_bytes()) {
                hit = Some(i);
            }
        }
        let Some(i) = hit else {
            return Ok(Collected::Gone);
        };
        let Some(by) = all[i].approved_by.clone() else {
            return Ok(Collected::Waiting);
        };
        let r = all.remove(i);
        self.write("requests.json", &all)?;
        let via = if by == "ssh" {
            "approved over ssh".to_string()
        } else {
            format!("approved by {by}")
        };
        Ok(match self.add_pairing(&r.name, false, &via)? {
            Some((p, token)) => Collected::Paired(p, token),
            None => Collected::Gone,
        })
    }

    /// Lockdown: every pending request goes, approved or not.
    pub fn clear_requests(&self) -> Result<()> {
        self.write("requests.json", &Vec::<PendingRequest>::new())
    }

    /// Over SSH only: make `id` the primary (the old one stays, as an
    /// ordinary pairing).
    pub fn make_primary(&self, id: &str) -> Result<bool> {
        let mut all = self.pairings()?;
        if !all.iter().any(|p| p.id == id) {
            return Ok(false);
        }
        for p in &mut all {
            p.primary = p.id == id;
        }
        self.write("pairings.json", &all)?;
        Ok(true)
    }

    /// The pairing a bearer token belongs to. Every stored hash is compared,
    /// with no early exit, so the time taken doesn't say which one matched.
    pub fn authenticate(&self, token: &str) -> Result<Option<Pairing>> {
        if !is_token_shaped(token) {
            return Ok(None);
        }
        let presented = sha256_hex(token);
        let mut all = self.pairings()?;
        let mut hit = None;
        for (i, p) in all.iter().enumerate() {
            if ct_eq(presented.as_bytes(), p.token_sha256.as_bytes()) {
                hit = Some(i);
            }
        }
        let Some(i) = hit else { return Ok(None) };
        let at = now();
        if at.saturating_sub(all[i].last_used) >= LAST_USED_GRANULARITY {
            all[i].last_used = at;
            self.write("pairings.json", &all)?;
        }
        Ok(Some(all[i].clone()))
    }

    pub fn revoke(&self, by: Revoker, id: &str) -> Result<Revoked> {
        let mut all = self.pairings()?;
        let Some(target) = all.iter().position(|p| p.id == id) else {
            return Ok(Revoked::NotFound);
        };
        match by {
            Revoker::Pairing(me) => match all.iter().position(|p| p.id == me) {
                Some(mine) if rank(&all, target) >= rank(&all, mine) => {}
                _ => return Ok(Revoked::Refused("a pairing can't revoke one senior to itself")),
            },
            Revoker::Console if all[target].primary => {
                return Ok(Revoked::Refused(
                    "the console can't revoke the primary pairing; revoke it from that Manager or over SSH",
                ))
            }
            Revoker::Console | Revoker::Ssh => {}
        }
        all.remove(target);
        self.write("pairings.json", &all)?;
        Ok(Revoked::Yes)
    }

    /// Lockdown: drop every pairing junior to `me`; returns their ids. If
    /// `me` is already gone (revoked a moment ago), everything but the primary
    /// goes: a lockdown never revokes less than asked.
    pub fn revoke_junior_to(&self, me: &str) -> Result<Vec<String>> {
        let all = self.pairings()?;
        let mine = all
            .iter()
            .position(|p| p.id == me)
            .map(|i| rank(&all, i))
            .unwrap_or(0);
        let (keep, gone): (Vec<_>, Vec<_>) = (0..all.len()).partition(|&i| rank(&all, i) <= mine);
        if gone.is_empty() {
            return Ok(vec![]);
        }
        let ids = gone.iter().map(|&i| all[i].id.clone()).collect();
        let kept: Vec<Pairing> = keep.into_iter().map(|i| all[i].clone()).collect();
        self.write("pairings.json", &kept)?;
        Ok(ids)
    }

    /// Lockdown, as one change under this lock: the flag, the revoke of
    /// every pairing junior to `me`, and every pending request dropped.
    /// Returns the revoked ids.
    pub fn lock_down(&self, me: &str) -> Result<Vec<String>> {
        self.update_flags(|f| f.lockdown = true)?;
        let gone = self.revoke_junior_to(me)?;
        self.clear_requests()?;
        Ok(gone)
    }
}

/// Write `name` in `dir` without ever following a link an attacker planted:
/// a fresh temp file (`O_EXCL|O_NOFOLLOW`) renamed over the target, which
/// replaces a symlink rather than writing through it.
pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<()> {
    let tmp = dir.join(format!(".{name}.{}", hex(&random_bytes::<6>())));
    let r = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, dir.join(name)).with_context(|| format!("replace {name}"))
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codes::{mint_install_code, REQUEST_CODE_LEN};

    /// A temp directory removed on drop.
    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub(crate) fn new() -> Self {
            let d = std::env::temp_dir()
                .join(format!("atelier-engine-test-{}", hex(&random_bytes::<6>())));
            fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn add(l: &Locked, name: &str) -> Pairing {
        l.add_pairing(name, false, "test").unwrap().unwrap().0
    }

    pub(crate) fn primary(l: &Locked) -> Pairing {
        l.add_pairing("owner", true, "test").unwrap().unwrap().0
    }

    #[test]
    fn the_install_code_pairs_the_primary_once_and_wrong_tries_burn_nothing() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let l = store.lock().unwrap();
        assert!(matches!(
            l.redeem_install_code("anything", "x").unwrap(),
            Redeemed::Wrong
        ));
        let old = add(&l, "earlier");
        let code = mint_install_code();
        l.set_install_code(&code).unwrap();
        assert!(!fs::read_to_string(d.0.join("engine.json"))
            .unwrap()
            .contains(&normalize_code(&code)));
        for _ in 0..50 {
            assert!(matches!(
                l.redeem_install_code("WRONG", "x").unwrap(),
                Redeemed::Wrong
            ));
        }
        let Redeemed::Paired(p, _) = l
            .redeem_install_code(&code.to_lowercase(), "owner")
            .unwrap()
        else {
            panic!("the right code, typed lower case, pairs");
        };
        assert!(p.primary && p.via == "install code");
        assert!(!l
            .pairings()
            .unwrap()
            .iter()
            .any(|o| o.id == old.id && o.primary));
        assert!(
            matches!(
                l.redeem_install_code(&code, "again").unwrap(),
                Redeemed::Wrong
            ),
            "spent"
        );
    }

    #[test]
    fn a_request_gets_a_key_only_once_the_owner_types_its_code() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let l = store.lock().unwrap();
        let owner = primary(&l);
        let Asked::Pending(code, handle) = l.ask("laptop", "192.0.2.4").unwrap() else {
            panic!("asked");
        };
        assert_eq!(code.len(), REQUEST_CODE_LEN);
        assert!(!fs::read_to_string(d.0.join("requests.json"))
            .unwrap()
            .contains(&handle));
        assert!(matches!(l.collect(&handle).unwrap(), Collected::Waiting));
        assert!(matches!(l.collect("made-up").unwrap(), Collected::Gone));
        assert!(l.approve("WRONGCOD", &owner.id).unwrap().is_none());
        let r = l.approve(&code.to_lowercase(), &owner.id).unwrap().unwrap();
        assert_eq!(r.name, "laptop");
        assert!(
            l.approve(&code, &owner.id).unwrap().is_none(),
            "approved once"
        );
        let Collected::Paired(p, token) = l.collect(&handle).unwrap() else {
            panic!("approved request pairs");
        };
        assert!(!p.primary, "only the install code makes a primary");
        assert_eq!(p.via, format!("approved by {}", owner.id));
        assert_eq!(l.authenticate(&token).unwrap().unwrap().id, p.id);
        assert!(
            matches!(l.collect(&handle).unwrap(), Collected::Gone),
            "collected once"
        );
    }

    #[test]
    fn requests_are_capped_expire_and_die_in_a_lockdown() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let l = store.lock().unwrap();
        let owner = primary(&l);
        for _ in 0..REQUESTS_PER_IP {
            assert!(matches!(
                l.ask("x", "198.51.100.6").unwrap(),
                Asked::Pending(..)
            ));
        }
        assert!(matches!(
            l.ask("x", "198.51.100.6").unwrap(),
            Asked::TooMany
        ));
        for i in 0..REQUESTS_MAX - REQUESTS_PER_IP {
            l.ask("x", &format!("10.0.0.{i}")).unwrap();
        }
        assert!(
            matches!(l.ask("x", "10.9.9.9").unwrap(), Asked::TooMany),
            "32 in all"
        );
        l.clear_requests().unwrap();
        let Asked::Pending(code, handle) = l.ask("laptop", "192.0.2.4").unwrap() else {
            panic!("ask");
        };
        l.approve(&code, "ssh").unwrap().unwrap();
        l.lock_down(&owner.id).unwrap();
        assert!(l.requests().unwrap().is_empty());
        assert!(matches!(l.ask("x", "192.0.2.9").unwrap(), Asked::Closed));
        assert!(
            matches!(l.collect(&handle).unwrap(), Collected::Gone),
            "approved, then locked down"
        );
        l.update_flags(|f| f.lockdown = false).unwrap();
        let Asked::Pending(..) = l.ask("x", "192.0.2.9").unwrap() else {
            panic!("ask");
        };
        let mut all = l.requests().unwrap();
        all.iter_mut().for_each(|r| r.created = now() - REQUEST_TTL);
        l.write("requests.json", &all).unwrap();
        assert!(l.requests().unwrap().is_empty(), "expired");
    }

    #[test]
    fn tokens_are_stored_as_hashes_and_authenticate() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let l = store.lock().unwrap();
        let (p, token) = l.add_pairing("laptop", false, "test").unwrap().unwrap();
        let on_disk = fs::read_to_string(d.0.join("pairings.json")).unwrap();
        assert!(!on_disk.contains(&token[4..]), "plaintext token on disk");
        assert_eq!(l.authenticate(&token).unwrap().unwrap().id, p.id);
        assert!(l.authenticate(&new_token()).unwrap().is_none());
        assert!(l.authenticate("aet_short").unwrap().is_none());
        assert_eq!(l.revoke(Revoker::Ssh, &p.id).unwrap(), Revoked::Yes);
        assert!(l.authenticate(&token).unwrap().is_none(), "revoked");
    }

    #[test]
    fn pairings_rank_by_seniority_and_revoke_only_juniors() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let l = store.lock().unwrap();
        let owner = primary(&l);
        let second = add(&l, "second");
        let rogue = add(&l, "rogue");
        assert!(matches!(
            l.revoke(Revoker::Pairing(&rogue.id), &owner.id).unwrap(),
            Revoked::Refused(_)
        ));
        assert!(matches!(
            l.revoke(Revoker::Pairing(&rogue.id), &second.id).unwrap(),
            Revoked::Refused(_)
        ));
        assert!(matches!(
            l.revoke(Revoker::Console, &owner.id).unwrap(),
            Revoked::Refused(_)
        ));
        assert!(
            matches!(
                l.revoke(Revoker::Pairing("zzzzzzzzzzzz"), &rogue.id)
                    .unwrap(),
                Revoked::Refused(_)
            ),
            "a pairing that is gone revokes nothing"
        );
        assert_eq!(
            l.revoke(Revoker::Pairing(&second.id), &rogue.id).unwrap(),
            Revoked::Yes
        );
        assert_eq!(
            l.revoke(Revoker::Console, &second.id).unwrap(),
            Revoked::Yes
        );
        assert_eq!(
            l.revoke(Revoker::Console, &second.id).unwrap(),
            Revoked::NotFound
        );
        assert_eq!(
            l.revoke(Revoker::Pairing(&owner.id), &owner.id).unwrap(),
            Revoked::Yes,
            "a pairing may forget itself"
        );
        // With the primary gone, nobody is promoted.
        let later = add(&l, "later");
        assert!(!later.primary);
        assert!(views(&l.pairings().unwrap()).iter().all(|v| !v.primary));
    }

    #[test]
    fn the_primary_is_sticky_never_idles_out_and_ssh_can_move_it() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let l = store.lock().unwrap();
        let owner = primary(&l);
        let (_, junior_token) = l.add_pairing("old", false, "test").unwrap().unwrap();
        let mut all = l.pairings().unwrap();
        all.iter_mut()
            .for_each(|p| p.last_used = now() - TOKEN_IDLE_EXPIRY);
        l.write("pairings.json", &all).unwrap();
        let left = l.pairings().unwrap();
        assert_eq!(left.len(), 1, "the junior idled out");
        assert_eq!(left[0].id, owner.id, "the primary is kept");
        assert!(l.authenticate(&junior_token).unwrap().is_none());
        let next = add(&l, "next");
        assert!(l.make_primary(&next.id).unwrap());
        assert!(matches!(
            l.revoke(Revoker::Pairing(&owner.id), &next.id).unwrap(),
            Revoked::Refused(_)
        ));
        assert!(!l.make_primary("zzzzzzzzzzzz").unwrap());
    }

    #[test]
    fn lockdown_revokes_juniors_and_no_pairing_is_added_during_it() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let l = store.lock().unwrap();
        let owner = primary(&l);
        let second = add(&l, "second");
        let third = add(&l, "third");
        assert_eq!(l.lock_down(&second.id).unwrap(), vec![third.id.clone()]);
        assert!(l.add_pairing("rogue", false, "test").unwrap().is_none());
        assert!(l.add_pairing("rogue", true, "test").unwrap().is_none());
        let code = mint_install_code();
        l.set_install_code(&code).unwrap();
        assert!(matches!(
            l.redeem_install_code(&code, "x").unwrap(),
            Redeemed::Closed
        ));
        assert!(
            l.flags().unwrap().install_hash.is_some(),
            "not spent while closed"
        );
        // A caller that is already gone revokes everything but the primary.
        assert_eq!(
            l.lock_down("zzzzzzzzzzzz").unwrap(),
            vec![second.id.clone()]
        );
        let left = l.pairings().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, owner.id);
    }

    #[test]
    fn failure_window_forgets_after_an_hour() {
        let mut w = FailureWindow::default();
        w.record(0);
        w.record(1800);
        assert_eq!(w.count(3599), 2);
        assert_eq!(w.count(3600), 1);
        assert_eq!(w.count(5400), 0);
    }

    #[test]
    fn one_address_pauses_only_itself() {
        let mut f = Flags::default();
        for _ in 0..WAKE_TRIES_PER_IP {
            assert_eq!(f.wake_try("198.51.100.6", 100).0, WakeGate::Go);
        }
        assert_eq!(f.wake_try("198.51.100.6", 101).0, WakeGate::PausedHere);
        assert_eq!(f.wake_try("192.0.2.4", 101).0, WakeGate::Go, "the owner");
        assert_eq!(
            f.wake_try("198.51.100.6", 100 + WAKE_PAUSE).0,
            WakeGate::Go,
            "ends on its own"
        );
        for _ in 1..WAKE_TRIES_PER_IP {
            f.wake_try("198.51.100.6", 5000);
        }
        assert_eq!(
            f.wake_ip_pause["198.51.100.6"].until,
            5000 + 2 * WAKE_PAUSE,
            "doubles"
        );
        f.wake_succeeded("198.51.100.6");
        assert_eq!(f.wake_try("198.51.100.6", 5001).0, WakeGate::Go);
    }

    #[test]
    fn many_addresses_pause_everyone_for_a_while() {
        let mut f = Flags::default();
        let mut started = 0;
        for i in 0..WAKE_TRIES_PER_HOUR {
            started += f.wake_try(&format!("10.0.{}.{}", i / 250, i % 250), 100).1 as usize;
        }
        assert_eq!(started, 1);
        assert_eq!(f.wake_try("192.0.2.4", 101).0, WakeGate::Paused);
        assert!(!f.wake_paused(100 + WAKE_PAUSE), "ends on its own");
        f.wake_pause.count = 30;
        f.wake_pause.start(50_000);
        assert_eq!(f.wake_pause.until, 50_000 + WAKE_PAUSE_MAX, "capped");
        f.wake_resumed();
        assert!(!f.wake_paused(50_001));
    }

    #[test]
    fn the_wake_counts_survive_a_restart() {
        let d = TempDir::new();
        let store = Store::at(&d.0).unwrap();
        let at = now();
        store
            .lock()
            .unwrap()
            .update_flags(|f| {
                for _ in 0..WAKE_TRIES_PER_IP {
                    f.wake_try("198.51.100.6", at);
                }
            })
            .unwrap();
        let again = Store::at(&d.0).unwrap();
        let mut f = again.lock().unwrap().flags().unwrap();
        assert_eq!(f.wake_try("198.51.100.6", at).0, WakeGate::PausedHere);
    }

    #[test]
    fn atomic_write_replaces_a_planted_symlink_instead_of_following_it() {
        let d = TempDir::new();
        let victim = d.0.join("victim");
        fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, d.0.join("pairings.json")).unwrap();
        write_atomic(&d.0, "pairings.json", b"[]", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!fs::symlink_metadata(d.0.join("pairings.json"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
