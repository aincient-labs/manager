//! One operation per site at a time (M2c, `studies/manager-prior-art.md` item 8).
//!
//! Every operation that changes a site holds an exclusive advisory lock on
//! `<site home>/.op.lock` for its whole run ([`hold`]); a second one, from the
//! Manager window or the `atelier` CLI, is refused with [`SiteBusy`] naming the
//! first ("an update started 2 minutes ago"). Read-only questions (status,
//! update check, list, checkup without repairs) take no lock. Home Assistant's
//! Supervisor rejects a second Core job the same way (`GROUP_REJECT`); DDEV
//! takes an OS file lock that names its holder.
//!
//! **The lock is the operating system's, never a pid file.** `flock(2)` on
//! macOS and Linux, `LockFileEx` on Windows: both belong to the open file and
//! the OS drops them when the holder's process ends, however it ends (a
//! crash, a kill -9, a closed laptop lid that killed the app). So a lock left
//! by a dead process never blocks, and there is no "unlock" for anyone to run.
//! The file itself stays (deleting a lock file races a second opener into
//! holding a lock on a file no one else can see); its text only names the
//! holder for the message.
//!
//! **Nesting.** Operations call each other (an update starts a stopped site
//! for its backup, then backs it up; a restore backs up first; a repair
//! starts the site). Each public entry point takes the lock, and a call
//! nested on the same thread is let through: this thread already holds the
//! site ([`HELD`]). Another thread of the same process gets its own file
//! handle and is refused like another process (the window runs each command
//! on a worker thread). The guard is `!Send`, so it is dropped on the thread
//! that took it.

use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::stack::Stack;

/// The lock file in a site's home.
pub const LOCK_FILE: &str = ".op.lock";

/// What holds a site, for the refusal's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Install,
    Reinstall,
    Start,
    Stop,
    Remove,
    Update,
    Backup,
    Restore,
    ChannelSwitch,
    Repair,
    CleanUp,
    Engine,
    Snapshot,
    Settings,
}

impl Operation {
    const ALL: [Operation; 14] = [
        Operation::Install,
        Operation::Reinstall,
        Operation::Start,
        Operation::Stop,
        Operation::Remove,
        Operation::Update,
        Operation::Backup,
        Operation::Restore,
        Operation::ChannelSwitch,
        Operation::Repair,
        Operation::CleanUp,
        Operation::Engine,
        Operation::Snapshot,
        Operation::Settings,
    ];

    /// The word written into the lock file.
    pub const fn id(self) -> &'static str {
        match self {
            Operation::Install => "install",
            Operation::Reinstall => "reinstall",
            Operation::Start => "start",
            Operation::Stop => "stop",
            Operation::Remove => "remove",
            Operation::Update => "update",
            Operation::Backup => "backup",
            Operation::Restore => "restore",
            Operation::ChannelSwitch => "channel-switch",
            Operation::Repair => "repair",
            Operation::CleanUp => "clean-up",
            Operation::Engine => "engine",
            Operation::Snapshot => "snapshot",
            Operation::Settings => "settings",
        }
    }

    /// How the refusal names it: "an update".
    pub const fn phrase(self) -> &'static str {
        match self {
            Operation::Install => "a setup",
            Operation::Reinstall => "a reinstall",
            Operation::Start => "a start",
            Operation::Stop => "a stop",
            Operation::Remove => "a removal",
            Operation::Update => "an update",
            Operation::Backup => "a backup",
            Operation::Restore => "a restore",
            Operation::ChannelSwitch => "a switch of update channel",
            Operation::Repair => "a repair",
            Operation::CleanUp => "a clean-up of old backups",
            Operation::Engine => "a change to the site's engine",
            Operation::Snapshot => "a snapshot change",
            Operation::Settings => "a settings change",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.id() == id)
    }

    /// Whether it may create the site's home to lock it in.
    fn lays_down(self) -> bool {
        matches!(self, Operation::Install | Operation::Reinstall)
    }
}

/// The site is busy: another operation holds it. Waiting is the way forward,
/// so there is no override; a holder that died has already let go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteBusy {
    /// What holds it ([`Operation::phrase`]), when the lock file says.
    pub operation: Option<String>,
    /// When it started, when the lock file says.
    pub since: Option<SystemTime>,
    /// The holder's process id, when the lock file says.
    pub pid: Option<u32>,
}

impl SiteBusy {
    /// What `atelier` exits with for it: after 3 and 4
    /// ([`crate::ops::Overridable`]) and 5 ([`crate::gate::UpdateBlocked`]), so
    /// a Manager driving a server's CLI tells it apart without its words.
    pub const EXIT_CODE: i32 = 6;

    /// Whether `e` is this refusal: the typed error here, or a server's
    /// `atelier` that ended with [`Self::EXIT_CODE`].
    pub fn is(e: &anyhow::Error) -> bool {
        e.is::<SiteBusy>()
            || e.downcast_ref::<crate::remote::RemoteExit>()
                .is_some_and(|x| x.code == Self::EXIT_CODE)
    }

    /// The refusal's words at `now`.
    pub fn message_at(&self, now: SystemTime) -> String {
        let what = self.operation.as_deref().unwrap_or("another operation");
        let when = self
            .since
            .map(|s| {
                format!(
                    " started {}",
                    ago(now.duration_since(s).unwrap_or_default())
                )
            })
            .unwrap_or_default();
        format!("Your site is busy: {what}{when} is still running. Wait for it to finish.")
    }
}

impl std::fmt::Display for SiteBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message_at(SystemTime::now()))
    }
}

impl std::error::Error for SiteBusy {}

/// "just now", "40 seconds ago", "2 minutes ago", "1 hour ago".
fn ago(d: Duration) -> String {
    let s = d.as_secs();
    let (n, unit) = match s {
        0..=4 => return "just now".into(),
        5..=59 => (s, "second"),
        60..=3599 => (s / 60, "minute"),
        3600..=86_399 => (s / 3600, "hour"),
        _ => (s / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

thread_local! {
    /// The lock files this thread holds: a nested operation on one of them
    /// runs under its caller's lock.
    static HELD: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// The site held for one operation. Dropping it lets go.
#[must_use = "the site is held only while the guard lives"]
pub struct OpLock {
    /// `None` for a nested hold (the caller's guard lets go) and for a site
    /// with nothing to lock yet.
    held: Option<(File, PathBuf)>,
    /// Dropped on the thread that took it ([`HELD`] is per thread).
    _not_send: PhantomData<*const ()>,
}

impl OpLock {
    fn none() -> Self {
        OpLock {
            held: None,
            _not_send: PhantomData,
        }
    }
}

impl Drop for OpLock {
    fn drop(&mut self) {
        if let Some((file, path)) = self.held.take() {
            // The holder's text goes before the lock does, so no one reads a
            // finished operation as running. Closing the file releases it.
            let _ = file.set_len(0);
            drop(file);
            HELD.with(|h| h.borrow_mut().retain(|p| p != &path));
        }
    }
}

/// Hold `stack`'s site for `op` until the guard drops, or refuse with
/// [`SiteBusy`] while another operation holds it. A call nested in one this
/// thread already holds is let through. A site whose home doesn't exist yet has
/// nothing to protect (the operation fails its own way), except an install,
/// which makes the home to lock it.
pub fn hold(stack: &Stack, op: Operation) -> Result<OpLock> {
    hold_at(&stack.home, op)
}

/// [`hold`] for the site in `home`.
pub fn hold_at(home: &Path, op: Operation) -> Result<OpLock> {
    let path = home.join(LOCK_FILE);
    if HELD.with(|h| h.borrow().contains(&path)) {
        return Ok(OpLock::none());
    }
    if !home.is_dir() {
        if !op.lays_down() {
            return Ok(OpLock::none());
        }
        std::fs::create_dir_all(home)
            .with_context(|| format!("could not create {}", home.display()))?;
    }
    // Never truncated on open: the holder's text is what a refusal reads.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    // A lock with no holder text is either a holder that hasn't written it
    // yet or one letting go right now: a process another thread of the
    // letting-go process forks keeps the open file (so the lock) until it
    // execs. Ask again briefly before calling the site busy.
    let mut tries = 0;
    loop {
        match sys::try_lock(&file) {
            Ok(true) => break,
            Ok(false) => {
                let found = holder(&mut file);
                tries += 1;
                if found.operation.is_some() || tries >= 10 {
                    return Err(found.into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // A filesystem without locks (some network mounts): run unguarded
            // rather than refuse every operation on the site.
            Err(_) => return Ok(OpLock::none()),
        }
    }
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let text = format!(
        "op={}\npid={}\nsince={since}\n",
        op.id(),
        std::process::id()
    );
    // Best effort: a holder whose text didn't land is still the holder.
    let _ = file
        .set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .and_then(|_| file.write_all(text.as_bytes()))
        .and_then(|()| file.flush());
    HELD.with(|h| h.borrow_mut().push(path.clone()));
    Ok(OpLock {
        held: Some((file, path)),
        _not_send: PhantomData,
    })
}

/// The refusal, from the holder's text in `file` (empty while it has none).
fn holder(file: &mut File) -> SiteBusy {
    let mut text = String::new();
    match file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_string(&mut text))
    {
        Ok(_) => parse_holder(&text),
        Err(_) => parse_holder(""),
    }
}

/// The holder's `key=value` lines ([`hold_at`] writes them).
fn parse_holder(text: &str) -> SiteBusy {
    let field = |key: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| v.trim().to_string())
    };
    SiteBusy {
        operation: field("op")
            .as_deref()
            .and_then(Operation::from_id)
            .map(|o| o.phrase().to_string()),
        since: field("since")
            .and_then(|s| s.parse::<u64>().ok())
            .map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        pid: field("pid").and_then(|p| p.parse().ok()),
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    /// `flock(LOCK_EX | LOCK_NB)`: `Ok(false)` while another open file holds
    /// it. The kernel drops it when the last descriptor of this open file
    /// closes, so also when the process dies.
    pub fn try_lock(file: &File) -> std::io::Result<bool> {
        loop {
            // SAFETY: a valid open descriptor; flock touches no memory.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(true);
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
                    return Ok(false)
                }
                _ => return Err(e),
            }
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    /// `LockFileEx` on one byte far past the end of the file, so the holder's
    /// text (at the start) stays readable: Windows locks are mandatory for the
    /// bytes they cover. Windows releases it when the handle closes, so also
    /// when the process dies.
    pub fn try_lock(file: &File) -> std::io::Result<bool> {
        // SAFETY: OVERLAPPED is plain data; all zero is a valid value.
        let mut at: OVERLAPPED = unsafe { std::mem::zeroed() };
        at.Anonymous.Anonymous.Offset = 0xFFFF_FFFE;
        at.Anonymous.Anonymous.OffsetHigh = 0x7FFF_FFFF;
        // SAFETY: a valid open handle and a live OVERLAPPED for the call.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut at,
            )
        };
        if ok != 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(false)
        } else {
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "aincient-oplock-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempHome(dir)
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Try to take `home` from another thread (another handle, as another
    /// process would): the refusal, or `None` when it got the lock.
    fn from_another_thread(home: &Path, op: Operation) -> Option<SiteBusy> {
        let home = home.to_path_buf();
        std::thread::spawn(move || match hold_at(&home, op) {
            Ok(_) => None,
            Err(e) => Some(e.downcast::<SiteBusy>().expect("a SiteBusy")),
        })
        .join()
        .unwrap()
    }

    #[test]
    fn a_second_operation_is_refused_naming_the_first_until_it_ends() {
        let t = TempHome::new();
        let first = hold_at(&t.0, Operation::Update).unwrap();
        let busy = from_another_thread(&t.0, Operation::Restore).expect("refused");
        assert_eq!(busy.operation.as_deref(), Some("an update"));
        assert_eq!(busy.pid, Some(std::process::id()));
        assert!(busy.since.is_some());
        assert_eq!(
            busy.to_string(),
            "Your site is busy: an update started just now is still running. Wait for it to \
             finish."
        );
        drop(first);
        assert_eq!(from_another_thread(&t.0, Operation::Restore), None);
        // Let go, the file no longer names a holder.
        assert_eq!(
            std::fs::read_to_string(t.0.join(LOCK_FILE)).unwrap(),
            "",
            "a finished operation is not left named as running"
        );
    }

    #[test]
    fn a_nested_operation_on_the_same_thread_runs_under_its_callers_lock() {
        let t = TempHome::new();
        let outer = hold_at(&t.0, Operation::Update).unwrap();
        {
            let _inner = hold_at(&t.0, Operation::Backup).expect("nested: no deadlock");
            let _deeper = hold_at(&t.0, Operation::Start).expect("nested again");
        }
        // The inner guards dropping didn't let go of the outer one.
        assert!(from_another_thread(&t.0, Operation::Start).is_some());
        let text = std::fs::read_to_string(t.0.join(LOCK_FILE)).unwrap();
        assert!(text.starts_with("op=update\n"), "{text}");
        drop(outer);
        assert_eq!(from_another_thread(&t.0, Operation::Start), None);
    }

    #[test]
    fn a_site_with_no_home_is_not_created_except_by_an_install() {
        let t = TempHome::new();
        let missing = t.0.join("not-yet");
        let g = hold_at(&missing, Operation::Start).unwrap();
        assert!(!missing.exists(), "a start doesn't make the site's folder");
        drop(g);
        let g = hold_at(&missing, Operation::Install).unwrap();
        assert!(missing.join(LOCK_FILE).is_file());
        assert!(from_another_thread(&missing, Operation::Start).is_some());
        drop(g);
    }

    #[test]
    fn the_refusal_reads_how_long_ago_in_plain_words() {
        let start = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let busy = |op: Option<&str>, secs: Option<u64>| SiteBusy {
            operation: op.map(str::to_string),
            since: secs.map(|_| start),
            pid: None,
        };
        let at = |s: u64| start + Duration::from_secs(s);
        assert_eq!(
            busy(Some("an update"), Some(0)).message_at(at(125)),
            "Your site is busy: an update started 2 minutes ago is still running. Wait for it to \
             finish."
        );
        assert!(busy(Some("a backup"), Some(0))
            .message_at(at(40))
            .contains("a backup started 40 seconds ago"));
        assert!(busy(Some("a restore"), Some(0))
            .message_at(at(3600))
            .contains("started 1 hour ago"));
        assert_eq!(
            busy(None, None).message_at(at(1)),
            "Your site is busy: another operation is still running. Wait for it to finish."
        );
    }

    #[test]
    fn the_holder_text_round_trips_and_unknown_words_stay_unnamed() {
        let b = parse_holder("op=channel-switch\npid=42\nsince=1700000000\n");
        assert_eq!(b.operation.as_deref(), Some("a switch of update channel"));
        assert_eq!(b.pid, Some(42));
        assert_eq!(
            b.since,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        let b = parse_holder("op=something-newer\n");
        assert_eq!(b.operation, None);
        assert_eq!(parse_holder("").operation, None);
        for op in Operation::ALL {
            assert_eq!(Operation::from_id(op.id()), Some(op));
        }
    }

    #[test]
    fn the_exit_code_follows_the_other_refusals() {
        let e = anyhow::Error::new(SiteBusy {
            operation: None,
            since: None,
            pid: None,
        });
        assert!(SiteBusy::is(&e));
        assert!(SiteBusy::is(&e.context("wrapped")));
        assert_eq!(SiteBusy::EXIT_CODE, 6);
    }
}
