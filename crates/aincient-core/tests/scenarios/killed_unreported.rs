//! Scenario 31 — Docker kills the app over and over and never says why.
//!
//! On CI (2026-11-15, containerd image store) the kernel killed the app's
//! main process seven times for lack of memory, and Docker set neither
//! `State.OOMKilled` nor an `oom` event. Start called it "keeps stopping and
//! starting". The same picture, made on purpose: a SIGKILL from outside the
//! container (a privileged helper in the host's process namespace, as the
//! kernel's OOM killer would), so the restart policy answers it as it answers
//! a memory kill (`docker kill` would count as a stop and not be restarted).
//!
//! Start must end with [`BootFailed`] typed as memory, hedged ("likely"),
//! since nothing proves memory; and Docker must indeed have said nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aincient_core::boot::BootFailed;
use aincient_core::ops;

use crate::harness::{try_docker, Feed, Site, Template};

/// Alpine is the scenario app's own base, so the helper needs no download.
const KILLER_IMAGE: &str = "alpine:3.22";

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn app_killed_without_an_oom_report_says_likely_out_of_memory() {
    let t = Instant::now();
    let mut site = Site::new("killed-silent");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();
    // A long converge, so every start is still booting when it is killed.
    site.flag("scn-boot-slow-", "a");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");

    // The "kernel": SIGKILL the app's main process whenever it runs.
    let done = Arc::new(AtomicBool::new(false));
    let container = site.container("app");
    let killer = {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let mut kills = 0u32;
            while !done.load(Ordering::Relaxed) {
                let pid = try_docker(&["inspect", "-f", "{{.State.Pid}}", &container])
                    .unwrap_or_default();
                let pid = pid.trim();
                if !pid.is_empty() && pid != "0" {
                    let killed = try_docker(&[
                        "run",
                        "--rm",
                        "--pid=host",
                        "--privileged",
                        KILLER_IMAGE,
                        "kill",
                        "-9",
                        pid,
                    ]);
                    if killed.is_ok() {
                        kills += 1;
                    }
                }
                thread::sleep(Duration::from_millis(500));
            }
            kills
        })
    };

    let mut feed = Feed::default();
    let started = ops::start(&site.stack, &mut feed);
    done.store(true, Ordering::Relaxed);
    let kills = killer.join().expect("the killer thread");

    let failed = match started {
        Err(e) => e,
        Ok(ready) => {
            panic!("a killed boot must fail the start, got Ok({ready}) after {kills} kills")
        }
    };
    let why = failed
        .downcast_ref::<BootFailed>()
        .unwrap_or_else(|| panic!("a BootFailed: {failed:#}"));
    assert!(kills >= 2, "killed more than once: {kills}");
    assert!(why.out_of_memory(), "typed as memory: {}", why.0);
    assert!(
        why.0.starts_with(
            "Docker stopped part of your site by force (the app), likely for lack of memory."
        ),
        "hedged, the app named: {}",
        why.0
    );
    // Docker said nothing about memory: the evidence was the kills alone.
    assert_eq!(site.inspect("app", "{{.State.OOMKilled}}"), "false");
    assert!(
        site.inspect("app", "{{.RestartCount}}")
            .parse::<u32>()
            .unwrap()
            >= 1,
        "the restart policy answered the kills"
    );
    eprintln!("scenario 31 took {:.0?} ({kills} kills)", t.elapsed());
}
