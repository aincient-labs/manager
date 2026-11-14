//! Scenario 12 — the database runs out of memory while the site boots (M2b).
//! Postgres has a memory limit of its own (swap included) and the shipped
//! restart policy; the app's "converge" runs a query whose result is bigger
//! than that limit. The kernel kills the Postgres backend, Postgres logs
//! "server process (PID n) was terminated by signal 9: Killed" and restarts
//! itself inside the container, so `db` keeps running with `OOMKilled: true`;
//! converge fails. Start said "couldn't finish starting"; it must say the
//! database ran out of memory, and that the data is safe.

use std::time::Instant;

use aincient_core::boot::BootFailed;
use aincient_core::ops;

use crate::harness::{Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn db_oom_killed_during_boot_says_the_database_ran_out_of_memory() {
    let t = Instant::now();
    let mut site = Site::new("oom-db");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.limit_memory("db", "128m");
    site.up_ready();
    site.flag("scn-boot-db-oom-", "a");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");

    let failed = ops::start(&site.stack, &mut Feed::default())
        .expect_err("a boot whose database ran out of memory fails");
    let why = failed
        .downcast_ref::<BootFailed>()
        .unwrap_or_else(|| panic!("a BootFailed: {failed:#}"));
    assert!(why.out_of_memory(), "{}", why.0);
    assert!(
        why.0.starts_with(
            "Your site ran out of memory, so Docker stopped part of it (the database). Your \
             site's data is safe. Give Docker more memory"
        ),
        "{}",
        why.0
    );
    assert!(site.running("db"), "Postgres restarted itself in place");
    assert_eq!(site.inspect("db", "{{.State.OOMKilled}}"), "true");
    assert!(
        db_log(&site).contains("was terminated by signal 9: Killed"),
        "Postgres's own line is in the db's log"
    );
    eprintln!("scenario 12 took {:.0?}", t.elapsed());
}

/// The db's whole log: Postgres writes to stderr, which `docker logs` hands
/// back on stderr.
fn db_log(site: &Site) -> String {
    let mut c = aincient_core::docker::docker();
    c.args(["logs", &site.container("db")]);
    let out = c.output().expect("docker logs");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
