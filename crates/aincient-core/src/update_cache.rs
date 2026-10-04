//! The cached, throttled, single-flight update check behind the GUI's banner
//! (manager#6).
//!
//! A registry check costs a network round trip and can take its full deadline on a
//! flaky connection, so the GUI never waits on one to paint. Instead:
//!
//! - the last [`UpdateCheck`] and when it ran are kept in
//!   [`UPDATE_CACHE_FILE`] inside the stack directory, written atomically;
//! - the Home banner reads that file, never the network;
//! - a background refresh re-asks the registry only once the cache is older than
//!   [`UPDATE_CACHE_TTL`] (or holds no conclusive answer);
//! - at most one check runs at a time — concurrent callers wait for and share the
//!   result of the one already in flight ([`SingleFlight`]), so a slow registry
//!   can never pile up `docker buildx imagetools` processes.
//!
//! The CLI's `update --check` deliberately stays a live check and does not touch
//! the cache; it only benefits from [`invalidate`], which every successful image
//! pull calls so a cached "update available" never outlives the update.

use std::path::PathBuf;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ops::{self, UpdateCheck};
use crate::stack::Stack;

/// The cache file, inside the stack directory (`~/.atelier/update-check.json`).
pub const UPDATE_CACHE_FILE: &str = "update-check.json";

/// How long a conclusive answer is trusted before a background refresh asks the
/// registry again. Hours, not minutes: releases are rare, and an update that waits
/// a few hours to be announced costs nothing next to a check on every start/stop.
pub const UPDATE_CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Bumped whenever the on-disk shape changes incompatibly; any other value reads
/// as "no cache", so an older or newer manager's file is simply re-checked.
const FORMAT: u32 = 1;

/// The on-disk record: the check and when it was taken.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedUpdate {
    format: u32,
    /// Unix seconds at which `check` was taken.
    pub checked_at: u64,
    pub check: UpdateCheck,
}

impl CachedUpdate {
    pub fn new(check: UpdateCheck, checked_at: u64) -> Self {
        Self {
            format: FORMAT,
            checked_at,
            check,
        }
    }

    /// Whether this answer is still good enough to stand in for a live check at
    /// `now` (Unix seconds).
    ///
    /// Only a *conclusive* answer is ever fresh: an inconclusive one (offline,
    /// Docker down, …) is worth retrying on the next refresh, not trusting for six
    /// hours. A timestamp in the future (clock moved backwards) is not fresh
    /// either — trusting it would suppress checks until the clock catches up.
    pub fn is_fresh(&self, now: u64, ttl: Duration) -> bool {
        self.check.update_available.is_some()
            && now >= self.checked_at
            && now - self.checked_at < ttl.as_secs()
    }
}

/// What the GUI is handed: the cached check plus whether it is still fresh, so
/// the webview never has to do clock arithmetic.
#[derive(Debug, Clone, Serialize)]
pub struct CacheStatus {
    pub fresh: bool,
    pub checked_at: u64,
    pub check: UpdateCheck,
}

impl CacheStatus {
    fn of(cached: CachedUpdate, now: u64) -> Self {
        Self {
            fresh: cached.is_fresh(now, UPDATE_CACHE_TTL),
            checked_at: cached.checked_at,
            check: cached.check,
        }
    }
}

/// Current Unix time in seconds (0 if the clock is before 1970).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn cache_path(stack: &Stack) -> PathBuf {
    stack.home.join(UPDATE_CACHE_FILE)
}

/// Read the cache. Missing, unreadable, corrupt, of another format, or about a
/// different image than the stack is configured for (a channel switch, a hand
/// edit) — all read as `None`, i.e. "never checked".
pub fn read(stack: &Stack) -> Option<CachedUpdate> {
    let text = std::fs::read_to_string(cache_path(stack)).ok()?;
    let cached: CachedUpdate = serde_json::from_str(&text).ok()?;
    (cached.format == FORMAT && cached.check.image == stack.image()).then_some(cached)
}

/// Write the cache atomically: a sibling temp file, then a rename over the real
/// one, so a crash mid-write leaves the old file or the new one, never half of
/// either. Best effort by design — failing to cache only costs a re-check later.
///
/// Never creates the stack directory: an update check must not lay down a stack
/// (an empty `~/.atelier` would change what "installed" means elsewhere).
pub fn write(stack: &Stack, cached: &CachedUpdate) -> std::io::Result<()> {
    if !stack.home.is_dir() {
        return Ok(());
    }
    let path = cache_path(stack);
    let tmp = stack
        .home
        .join(format!(".{UPDATE_CACHE_FILE}.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(cached).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Forget the cached check (just the one file — never anything else in the stack
/// directory). Called after every successful image pull.
pub fn invalidate(stack: &Stack) {
    let _ = std::fs::remove_file(cache_path(stack));
}

/// Fold a fresh `check` into the cache and return what was stored.
///
/// One rule beyond "newest wins": an inconclusive check never overwrites a
/// conclusive answer for the same image. Going offline for a minute shouldn't
/// erase a known update from the banner; the old answer keeps its old timestamp,
/// so it still goes stale and gets retried on schedule.
pub fn record(stack: &Stack, check: UpdateCheck, now: u64) -> CachedUpdate {
    if check.update_available.is_none() {
        if let Some(prev) = read(stack) {
            if prev.check.update_available.is_some() {
                return prev;
            }
        }
    }
    let cached = CachedUpdate::new(check, now);
    let _ = write(stack, &cached);
    cached
}

/// The cache as the banner sees it — a file read, never the network.
pub fn status(stack: &Stack) -> Option<CacheStatus> {
    read(stack).map(|c| CacheStatus::of(c, now_secs()))
}

/// A live registry check through `flight` (so concurrent callers share one), with
/// its result recorded in the cache. Returns the live check itself — an explicit
/// "Check for updates" must report *this* attempt (e.g. "offline"), even when the
/// cache keeps an older conclusive answer.
pub fn live_check(stack: &Stack, flight: &SingleFlight<UpdateCheck>) -> UpdateCheck {
    flight.run(|| {
        let check = ops::check_update(stack);
        record(stack, check.clone(), now_secs());
        check
    })
}

/// The background refresh: a live check only when `force` is set or the cache
/// isn't fresh, then the cache as it now stands.
pub fn refresh(
    stack: &Stack,
    flight: &SingleFlight<UpdateCheck>,
    force: bool,
) -> Option<CacheStatus> {
    let fresh = read(stack).is_some_and(|c| c.is_fresh(now_secs(), UPDATE_CACHE_TTL));
    if force || !fresh {
        live_check(stack, flight);
    }
    status(stack)
}

/// At most one run of an expensive call at a time; callers arriving while one is
/// in flight block until it lands and get a clone of its result instead of
/// starting their own.
///
/// Blocking (a `Mutex` + `Condvar`), so call it from a worker thread — the GUI
/// runs it inside `spawn_blocking`. If the running call panics, a waiter takes
/// over and runs it itself rather than waiting forever.
pub struct SingleFlight<T> {
    state: Mutex<Flight<T>>,
    landed: Condvar,
}

struct Flight<T> {
    running: bool,
    /// Incremented every time a run lands (or unwinds).
    generation: u64,
    /// The result of the run that ended `generation`, tagged with it.
    last: Option<(u64, T)>,
}

impl<T: Clone> Default for SingleFlight<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> SingleFlight<T> {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(Flight {
                running: false,
                generation: 0,
                last: None,
            }),
            landed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Flight<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `f`, or — if a run is already in flight — wait for it and share its
    /// result.
    pub fn run<F: FnOnce() -> T>(&self, f: F) -> T {
        let mut g = self.lock();
        if g.running {
            let awaited = g.generation;
            while g.running && g.generation == awaited {
                g = self.landed.wait(g).unwrap_or_else(PoisonError::into_inner);
            }
            if let Some((gen, v)) = &g.last {
                if *gen == awaited {
                    return v.clone();
                }
            }
            // The run we waited on unwound without a result: start over.
            drop(g);
            return self.run(f);
        }
        g.running = true;
        drop(g);

        // Lands the flight even if `f` panics, so waiters are always released.
        struct Landing<'a, T: Clone> {
            flight: &'a SingleFlight<T>,
            result: Option<T>,
        }
        impl<T: Clone> Drop for Landing<'_, T> {
            fn drop(&mut self) {
                let mut g = self.flight.lock();
                if let Some(v) = self.result.take() {
                    g.last = Some((g.generation, v));
                }
                g.running = false;
                g.generation += 1;
                self.flight.landed.notify_all();
            }
        }
        let mut landing = Landing {
            flight: self,
            result: None,
        };
        let v = f();
        landing.result = Some(v.clone());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    /// A unique temp stack directory per test, removed on drop.
    struct TempStack(Stack);
    impl TempStack {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "atelier-update-cache-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempStack(Stack { home: dir })
        }
    }
    impl Drop for TempStack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.home);
        }
    }

    fn check(image: &str, update_available: Option<bool>) -> UpdateCheck {
        UpdateCheck {
            image: image.to_string(),
            current: Some("sha256:aaa".into()),
            latest: update_available.map(|_| "sha256:bbb".into()),
            current_version: Some("v0.17.0".into()),
            latest_version: Some("v0.18.0".into()),
            update_available,
            problem: update_available
                .is_none()
                .then(|| "You're offline — couldn't reach the registry.".into()),
            plan: None,
            offline: update_available.is_none(),
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn a_conclusive_check_is_fresh_until_the_ttl_runs_out() {
        let c = CachedUpdate::new(check("img", Some(true)), NOW);
        assert!(c.is_fresh(NOW, UPDATE_CACHE_TTL));
        assert!(c.is_fresh(NOW + UPDATE_CACHE_TTL.as_secs() - 1, UPDATE_CACHE_TTL));
        assert!(!c.is_fresh(NOW + UPDATE_CACHE_TTL.as_secs(), UPDATE_CACHE_TTL));
        assert!(!c.is_fresh(NOW + 10 * UPDATE_CACHE_TTL.as_secs(), UPDATE_CACHE_TTL));
    }

    #[test]
    fn an_inconclusive_or_future_dated_check_is_never_fresh() {
        let offline = CachedUpdate::new(check("img", None), NOW);
        assert!(!offline.is_fresh(NOW, UPDATE_CACHE_TTL));
        let future = CachedUpdate::new(check("img", Some(false)), NOW + 60);
        assert!(!future.is_fresh(NOW, UPDATE_CACHE_TTL));
    }

    #[test]
    fn the_cache_round_trips_through_the_stack_dir() {
        let ts = TempStack::new();
        let image = ts.0.image();
        let mut c = check(&image, Some(true));
        c.plan = Some(ops::UpgradePlan {
            from: ops::Version::parse("0.17.0"),
            target_image: image.clone(),
            steps: vec![ops::UpgradeStep {
                image: image.clone(),
                version: ops::Version::parse("v0.18.0"),
                is_target: true,
                reason: None,
            }],
            problem: None,
            explicit_target: false,
        });
        write(&ts.0, &CachedUpdate::new(c, NOW)).unwrap();

        let back = read(&ts.0).expect("cache reads back");
        assert_eq!(back.checked_at, NOW);
        assert_eq!(back.check.update_available, Some(true));
        assert_eq!(back.check.latest_version.as_deref(), Some("v0.18.0"));
        let plan = back.check.plan.expect("plan survives");
        assert_eq!(plan.target_version(), ops::Version::parse("0.18.0"));
        assert_eq!(plan.from, ops::Version::parse("0.17.0"));
        // Atomic write leaves no temp file behind.
        let names: Vec<_> = std::fs::read_dir(&ts.0.home)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![UPDATE_CACHE_FILE.to_string()]);
    }

    #[test]
    fn a_missing_cache_reads_as_none() {
        let ts = TempStack::new();
        assert!(read(&ts.0).is_none());
        assert!(status(&ts.0).is_none());
    }

    #[test]
    fn a_corrupt_cache_reads_as_missing() {
        let ts = TempStack::new();
        for junk in ["", "{", "not json", "{\"format\":1}", "[1,2,3]"] {
            std::fs::write(cache_path(&ts.0), junk).unwrap();
            assert!(read(&ts.0).is_none(), "{junk:?} should read as missing");
        }
    }

    #[test]
    fn a_cache_of_another_format_or_image_reads_as_missing() {
        let ts = TempStack::new();
        let mut c = CachedUpdate::new(check(&ts.0.image(), Some(true)), NOW);
        c.format = FORMAT + 1;
        write(&ts.0, &c).unwrap();
        assert!(read(&ts.0).is_none());

        write(
            &ts.0,
            &CachedUpdate::new(check("ghcr.io/someone/else:latest", Some(true)), NOW),
        )
        .unwrap();
        assert!(read(&ts.0).is_none());
    }

    #[test]
    fn writing_never_creates_the_stack_dir() {
        let ts = TempStack::new();
        let missing = Stack {
            home: ts.0.home.join("not-a-stack"),
        };
        write(&missing, &CachedUpdate::new(check("img", Some(true)), NOW)).unwrap();
        assert!(!missing.home.exists());
    }

    #[test]
    fn invalidate_removes_only_the_cache_file() {
        let ts = TempStack::new();
        std::fs::write(ts.0.home.join(".env"), "KEEP=1\n").unwrap();
        write(
            &ts.0,
            &CachedUpdate::new(check(&ts.0.image(), Some(true)), NOW),
        )
        .unwrap();
        invalidate(&ts.0);
        assert!(read(&ts.0).is_none());
        assert!(ts.0.home.join(".env").is_file());
        invalidate(&ts.0); // idempotent on a missing file
    }

    #[test]
    fn an_offline_check_does_not_erase_a_known_answer() {
        let ts = TempStack::new();
        let image = ts.0.image();
        record(&ts.0, check(&image, Some(true)), NOW);
        let kept = record(&ts.0, check(&image, None), NOW + 100);
        assert_eq!(kept.check.update_available, Some(true));
        assert_eq!(
            kept.checked_at, NOW,
            "keeps its age, so it still goes stale"
        );

        // With nothing conclusive to protect, the inconclusive answer is stored.
        invalidate(&ts.0);
        let stored = record(&ts.0, check(&image, None), NOW);
        assert!(stored.check.offline);
        assert_eq!(read(&ts.0).unwrap().check.update_available, None);

        // And a conclusive answer always replaces whatever was there.
        let replaced = record(&ts.0, check(&image, Some(false)), NOW + 5);
        assert_eq!(replaced.check.update_available, Some(false));
        assert_eq!(read(&ts.0).unwrap().checked_at, NOW + 5);
    }

    #[test]
    fn concurrent_callers_share_one_run() {
        const CALLERS: usize = 8;
        let flight = Arc::new(SingleFlight::<u32>::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(CALLERS));
        let handles: Vec<_> = (0..CALLERS)
            .map(|_| {
                let (flight, runs, start) = (flight.clone(), runs.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    flight.run(|| {
                        runs.fetch_add(1, Ordering::SeqCst);
                        // Long enough that every caller arrives while it's in flight.
                        std::thread::sleep(Duration::from_millis(300));
                        42
                    })
                })
            })
            .collect();
        let results: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the check ran once");
        assert!(results.iter().all(|&r| r == 42));
    }

    #[test]
    fn a_later_call_starts_a_new_run() {
        let flight = SingleFlight::<u32>::new();
        assert_eq!(flight.run(|| 1), 1);
        assert_eq!(flight.run(|| 2), 2, "a finished flight isn't reused");
    }

    #[test]
    fn a_panicking_run_releases_its_waiters() {
        let flight = Arc::new(SingleFlight::<u32>::new());
        let entered = Arc::new(Barrier::new(2));
        let leader = {
            let (flight, entered) = (flight.clone(), entered.clone());
            std::thread::spawn(move || {
                flight.run(|| {
                    entered.wait();
                    std::thread::sleep(Duration::from_millis(200));
                    panic!("check blew up");
                })
            })
        };
        entered.wait();
        // Arrives mid-flight, is released by the unwind, and runs it itself.
        assert_eq!(flight.run(|| 7), 7);
        assert!(leader.join().is_err());
    }
}
