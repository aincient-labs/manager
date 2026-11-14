//! Scenario 2 — db detached: `docker network disconnect -f` on the stopped db
//! drops the network from its own settings, and a start outside the Manager
//! (a Docker restart's `unless-stopped`, a `docker start`) leaves it running
//! with `Networks {}`. Neither `compose start`/`up` nor a Docker restart puts it
//! back; the app can't reach its database. The checkup must name it, and Start
//! must reconnect it and bring the site up (fixed in 8b2344f).

use std::time::Instant;

use aincient_core::doctor::{self, Severity};
use aincient_core::ops;

use crate::harness::{docker, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn checkup_names_a_detached_db_and_start_reconnects_it() {
    let t = Instant::now();
    let mut site = Site::new("detached-db");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();

    let default = site.network("default");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");
    let db = site.container("db");
    docker(&["network", "disconnect", "-f", &default, &db]);
    docker(&["start", &db]);
    // The incident's state, as Docker makes it: running, on no network.
    assert!(site.running("db"));
    assert!(
        site.networks("db").is_empty(),
        "db started with no network: {:?}",
        site.networks("db")
    );

    let report = doctor::diagnose(&site.stack);
    let network = report
        .checks
        .iter()
        .find(|c| c.id == "stack.network")
        .expect("the checkup has a network check");
    assert_eq!(network.severity, Severity::Fail, "{network:?}");
    let detail = network.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains(&db) && detail.contains(&default),
        "the checkup names db and its network: {detail}"
    );

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
    assert!(site.app_reaches_db(), "app reaches `db` by name");

    let after = doctor::diagnose(&site.stack);
    let network = after
        .checks
        .iter()
        .find(|c| c.id == "stack.network")
        .expect("network check");
    assert_eq!(network.severity, Severity::Ok, "{network:?}");
    eprintln!("scenario 2 took {:.0?}", t.elapsed());
}
