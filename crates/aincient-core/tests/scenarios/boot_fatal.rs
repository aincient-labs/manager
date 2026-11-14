//! Scenario 9 — a boot that logs `[converge] FATAL` (M3, A5 in 4c1b422): the
//! appliance's entrypoint starts its web server even after a failed converge,
//! so the console answers; Start must still say the boot failed, with
//! converge's reason in plain words. A later converge in the same container
//! (doctor's re-converge repair, which mirrors its output into the app's own
//! log) that ends `converge OK` settles it: the next Start is Ready, not
//! failed again on the old FATAL still in that log.
//!
//! The FATAL goes to stderr and `converge OK` to stdout, as `converge.sh`'s
//! `die` and `log` write them; the later line is written to PID 1's stdout,
//! where doctor's re-converge tees it.

use std::time::Instant;

use aincient_core::boot::BootFailed;
use aincient_core::ops;

use crate::harness::{docker, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn start_reports_a_fatal_boot_and_a_later_converge_ok_clears_it() {
    let t = Instant::now();
    let mut site = Site::new("boot-fatal");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();
    site.fail_boot("a");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");

    let mut feed = Feed::default();
    let failed = ops::start(&site.stack, &mut feed).expect_err("a FATAL boot is a failed start");
    let why = failed
        .downcast_ref::<BootFailed>()
        .unwrap_or_else(|| panic!("a BootFailed: {failed:#}"));
    assert_eq!(
        why.0,
        "Your site couldn't finish starting (post-update health check failed (scenario build \
         a)). Run a checkup (System → Troubleshoot, or `atelier doctor`) to see why.",
    );
    assert!(site.running("app"), "the server is up on the failed state");
    let started = site.inspect("app", "{{.State.StartedAt}}");

    // A converge run later in this container succeeds, its output mirrored
    // into the app's log the way doctor's re-converge does it.
    docker(&[
        "exec",
        &site.container("app"),
        "sh",
        "-c",
        "sleep 1; echo '[converge] converge OK — site re-converged' | tee -a /proc/1/fd/1",
    ]);

    let mut feed = Feed::default();
    let ready = ops::start(&site.stack, &mut feed);
    assert!(
        matches!(ready, Ok(true)),
        "the later converge OK settles the old FATAL: {ready:?}"
    );
    assert_eq!(
        site.inspect("app", "{{.State.StartedAt}}"),
        started,
        "same container run: its log still holds the FATAL"
    );
    eprintln!("scenario 9 took {:.0?}", t.elapsed());
}
