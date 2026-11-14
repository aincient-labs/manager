//! Scenario 17 — two operations on one site at once (M2c, `oplock`): the
//! window and the `atelier` CLI could run Update and Restore (or Start) on one
//! site together. Now an update holds the site for its whole run; a restore or
//! a start asked meanwhile is refused with [`SiteBusy`] naming the update, and
//! runs once the update is done.
//!
//! The update runs on another thread (as the window runs each command on a
//! worker thread, and as a second process would): its new build boots 20 s
//! slower ([`Site::slow_boot`]), so the update holds the site while the test
//! asks. A lock is the OS's, so one left by a process that died never blocks:
//! a child process (this test binary, [`hold_lock_child`]) takes the site and
//! is killed, and the next start runs.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aincient_core::oplock::{self, SiteBusy};
use aincient_core::ops::{self, RestoreOptions, UpdateOptions, Updated};

use crate::harness::{assert_cleanup_removes_nothing, Feed, Site, Template};

/// The refusal in `e`, with its words checked: it names `what`.
fn busy(e: &anyhow::Error, what: &str) -> SiteBusy {
    let said = format!("{e:#}");
    assert!(
        said.starts_with(&format!("Your site is busy: {what} started "))
            && said.ends_with("is still running. Wait for it to finish."),
        "the refusal in plain words: {said}"
    );
    e.downcast_ref::<SiteBusy>()
        .unwrap_or_else(|| panic!("a SiteBusy: {said}"))
        .clone()
}

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn a_second_operation_is_refused_while_an_update_runs_and_runs_after_it() {
    let t = Instant::now();
    let mut site = Site::new("site-busy");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.up_ready();
    site.slow_boot("b");
    // The download: the site's tag now names a new, slow-booting build.
    let (_, new) = site.build_app("app", "b");
    assert_cleanup_removes_nothing();

    let stack = site.stack.clone();
    let update = std::thread::spawn(move || {
        let opts = UpdateOptions { no_backup: true };
        ops::update_with(&stack, &opts, &mut Feed::default()).map_err(|e| format!("{e:#}"))
    });
    site.wait_for_holder("update");

    // A restore asked meanwhile: refused before it looks at anything.
    let refused = ops::restore_with(
        &site.stack,
        Path::new("/nonexistent/backup.tar.gz"),
        &RestoreOptions::default(),
        &mut Feed::default(),
    )
    .expect_err("a restore during an update must be refused");
    let held = busy(&refused, "an update");
    assert_eq!(held.operation.as_deref(), Some("an update"));
    assert_eq!(held.pid, Some(std::process::id()));
    assert!(held.since.is_some());
    assert!(SiteBusy::is(&refused));
    // A start too.
    let refused = ops::start(&site.stack, &mut Feed::default())
        .expect_err("a start during an update must be refused");
    busy(&refused, "an update");

    let done = update.join().expect("the update thread");
    assert!(
        matches!(done, Ok(Updated::Ready)),
        "the update itself ran to the end: {done:?}"
    );
    assert_eq!(site.inspect("app", "{{.Image}}"), new, "on the new build");

    // Once it ended, the others run.
    assert!(matches!(
        ops::start(&site.stack, &mut Feed::default()),
        Ok(true)
    ));
    let backup = ops::backup(&site.stack, None, &mut Feed::default()).expect("a backup runs");
    ops::restore_with(
        &site.stack,
        &backup,
        &RestoreOptions::default(),
        &mut Feed::default(),
    )
    .expect("the restore runs now");
    eprintln!("scenario 17 took {:.0?}", t.elapsed());
}

/// The env var that makes [`hold_lock_child`] hold a site: its home.
const CHILD_HOME: &str = "ATELIER_SCN_HOLD_LOCK";

/// Kills the child on the way out, even from a failed assertion.
struct Holder(Child);

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn a_lock_left_by_a_killed_process_does_not_block() {
    let t = Instant::now();
    let mut site = Site::new("lock-killed");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();

    // Another process takes the site (a backup) and keeps it.
    let exe = std::env::current_exe().expect("this test binary");
    let mut child = Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "site_busy::hold_lock_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_HOME, &site.stack.home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the holder");
    let out = child.stdout.take().expect("its stdout");
    let mut holder = Holder(child);
    let held = BufReader::new(out)
        .lines()
        .map_while(Result::ok)
        // libtest prints it after "test … ... " on the same line.
        .any(|l| l.trim_end().ends_with("held"));
    assert!(held, "the child took the site");

    let refused =
        ops::start(&site.stack, &mut Feed::default()).expect_err("the live holder blocks a start");
    let busy = busy(&refused, "a backup");
    assert_eq!(busy.pid, Some(holder.0.id()), "it names the other process");

    // Killed, not let go: no unlock, no cleanup of the file.
    holder.0.kill().expect("kill the holder");
    holder.0.wait().expect("reap the holder");
    assert!(
        std::fs::read_to_string(site.stack.home.join(oplock::LOCK_FILE))
            .unwrap()
            .contains("op=backup"),
        "its text is still there: only the OS lock went with it"
    );
    let started = ops::start(&site.stack, &mut Feed::default());
    assert!(
        matches!(started, Ok(true)),
        "a killed holder doesn't block: {started:?}"
    );
    drop(holder);
    eprintln!("scenario 17b took {:.0?}", t.elapsed());
}

/// Not a scenario: what [`a_lock_left_by_a_killed_process_does_not_block`]
/// runs in its child process. Without [`CHILD_HOME`] it does nothing.
#[test]
#[ignore = "child process of scenario 17b"]
fn hold_lock_child() {
    let Some(home) = std::env::var_os(CHILD_HOME) else {
        return;
    };
    let _held = oplock::hold_at(Path::new(&home), oplock::Operation::Backup)
        .expect("the site is free when the child starts");
    println!("held");
    std::thread::sleep(Duration::from_secs(120));
}
