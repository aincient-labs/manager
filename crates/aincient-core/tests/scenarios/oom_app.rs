//! Scenarios 11 and 11b — the app runs out of memory while it boots (M2b,
//! `studies/manager-prior-art.md` § OOM). The app has a memory limit of its
//! own (swap included) and the shipped `restart: unless-stopped`.
//!
//! - 11: PID 1 itself allocates past the limit during "converge". Docker
//!   kills the container, its restart policy starts it again within a second
//!   (and that start clears `State.OOMKilled`), and it is killed again: a
//!   container that is restarting, never stopped. Docker even says `Running:
//!   true` between the runs, so a wait that only asked "is it running?" ran
//!   out its two minutes and called the site "still booting".
//! - 11b: a child process (as a php-fpm worker or `drush` would) is killed;
//!   PID 1 lives on, reports a failed converge and serves. The container runs
//!   with `OOMKilled: true`, and Start said only "couldn't finish starting".
//!
//! Both must end Start with [`BootFailed`] saying the site ran out of memory.

use std::time::Instant;

use aincient_core::boot::BootFailed;
use aincient_core::ops;

use crate::harness::{out_of_memory_said, Feed, Site, Template};

/// Enough for the idle app; the allocation passes it within a second.
const APP_LIMIT: &str = "48m";

fn start_fails(site: &Site) -> String {
    let mut feed = Feed::default();
    let failed = ops::start(&site.stack, &mut feed);
    let failed = match failed {
        Err(e) => e,
        Ok(ready) => panic!("an out-of-memory boot must fail the start, got Ok({ready})"),
    };
    let why = failed
        .downcast_ref::<BootFailed>()
        .unwrap_or_else(|| panic!("a BootFailed: {failed:#}"));
    assert!(why.out_of_memory(), "typed as out of memory: {}", why.0);
    why.0.clone()
}

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn app_oom_killed_during_boot_keeps_restarting_and_start_says_out_of_memory() {
    let t = Instant::now();
    let mut site = Site::new("oom-app");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.limit_memory("app", APP_LIMIT);
    site.up_ready();
    site.flag("scn-boot-oom-", "a");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");

    let why = start_fails(&site);
    let said = out_of_memory_said("the app");
    assert!(why.starts_with(&said), "{why}\n  expected to start: {said}");
    assert!(
        why.ends_with("or stop other sites, then start your site again."),
        "{why}"
    );
    // Restarting, not stopped: the shipped restart policy keeps at it.
    let state = site.inspect("app", "{{.State.Status}} {{.RestartCount}}");
    let (status, count) = state.split_once(' ').unwrap();
    assert!(
        matches!(status, "restarting" | "running"),
        "the policy restarts it: {state}"
    );
    assert!(
        count.parse::<u32>().unwrap() >= 1,
        "restarted at least once: {state}"
    );
    eprintln!("scenario 11 took {:.0?}", t.elapsed());
}

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn a_child_process_oom_killed_during_boot_says_out_of_memory() {
    let t = Instant::now();
    let mut site = Site::new("oom-child");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.limit_memory("app", APP_LIMIT);
    site.up_ready();
    site.flag("scn-boot-oom-child-", "a");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");

    let why = start_fails(&site);
    assert!(
        why.starts_with("Your site ran out of memory, so Docker stopped part of it (the app)."),
        "{why}"
    );
    // PID 1 lived: the container runs, flagged, never restarted.
    assert_eq!(
        site.inspect(
            "app",
            "{{.State.Running}} {{.State.Restarting}} {{.State.OOMKilled}} {{.RestartCount}}"
        ),
        "true false true 0",
        "a killed child leaves the container running with OOMKilled"
    );
    eprintln!("scenario 11b took {:.0?}", t.elapsed());
}
