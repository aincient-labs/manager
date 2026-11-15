//! Scenario 19 — a running site's db lost its network (fixed in 8b2344f): the
//! app keeps running but can't reach `db` by name, and `compose start` (the
//! app runs, so Start's verb) changes nothing. Start must reconnect db under
//! its service name AND restart the app, whose boot ran (or runs) without its
//! database, then end Ready. Scenario 2 reconnects a db while everything else
//! is stopped, so it never reaches the restart (`reconnect_site` restarts the
//! app only when it runs).
//!
//! `docker network disconnect -f` on the running db is Docker's own way to the
//! state (what an earlier stale-endpoint clear did to it).

use std::time::Instant;

use aincient_core::ops;

use crate::harness::{docker, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn start_reconnects_a_running_db_and_restarts_the_app_that_lost_it() {
    let t = Instant::now();
    let mut site = Site::new("reconnect-restart");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('kept')");

    let default = site.network("default");
    let db = site.container("db");
    docker(&["network", "disconnect", "-f", &default, &db]);
    assert!(site.running("db") && site.running("app"));
    assert!(site.networks("db").is_empty(), "db on no network");
    assert!(!site.app_reaches_db(), "the app can't reach `db`");
    let app_id = site.id("app");
    let app_started = site.started_at("app");
    let db_id = site.id("db");

    let mut feed = Feed::default();
    let ready = ops::start(&site.stack, &mut feed);
    assert!(
        matches!(ready, Ok(true)),
        "Start must bring the site up: {ready:?}"
    );

    assert!(
        site.networks("db")
            .iter()
            .any(|(n, ip)| *n == default && !ip.is_empty()),
        "db has an address on {default} again: {:?}",
        site.networks("db")
    );
    assert_ne!(
        site.started_at("app"),
        app_started,
        "the app was restarted after the reconnect"
    );
    assert_eq!(site.id("app"), app_id, "restarted, not recreated");
    assert_eq!(site.id("db"), db_id, "db kept its container");
    assert!(
        feed.lines
            .iter()
            .any(|l| l.contains("Reconnected it; restarting the site")),
        "Start says why it restarts the site: {:?}",
        feed.lines
    );
    assert!(site.app_reaches_db(), "app reaches `db` by name");
    assert_eq!(site.sql("SELECT v FROM scn_marker"), "kept");
    eprintln!("scenario 19 took {:.0?}", t.elapsed());
}
