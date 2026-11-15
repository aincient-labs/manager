//! Scenario 1 — the review-tmp incident (2026-11-14): Docker kept a stopped
//! db's endpoint; the stale-endpoint recovery force-disconnected it, which also
//! drops the network from the stopped container's own settings, so db then
//! started with no network for good and the app "can't reach its database".
//! Fixed in 8b2344f (reconnect a container that lost its network) and A3 of
//! 4c1b422 (every network it declares, not only the one it was created on).
//!
//! Docker's genuine stale endpoint (a stopped container still listed by its
//! network) can't be made on demand: a paused container is the only state that
//! looks like it, and a start can't recover a paused one either way. So this
//! scenario builds the state the recovery leaves behind — a stopped db that
//! `docker network disconnect -f` (the recovery's own command) took off every
//! network — and asserts the end state of a Start. db declares a second
//! network (`backend`) in an overlay, as an engine site's edge declares two.

use std::time::Instant;

use aincient_core::ops;

use crate::harness::{docker, Feed, Site, Template};

const OVERLAY: &str = "\
services:
  db:
    networks:
      - default
      - backend
networks:
  backend: {}
";

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn start_reconnects_a_db_the_stale_endpoint_clear_left_without_networks() {
    let t = Instant::now();
    let mut site = Site::new("stale-endpoint");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.overlay("compose.scn.yaml", OVERLAY);
    site.up_ready();

    let default = site.network("default");
    let backend = site.network("backend");
    let before: Vec<String> = site.networks("db").into_iter().map(|(n, _)| n).collect();
    assert_eq!(before, vec![backend.clone(), default.clone()]);

    ops::stop(&site.stack, &mut Feed::default()).expect("stop");
    let db = site.container("db");
    assert!(!site.running("db"));
    // What clear_stale_endpoints ran for each leftover endpoint.
    for net in [&default, &backend] {
        docker(&["network", "disconnect", "-f", net, &db]);
    }
    // Docker's behaviour this incident rests on: gone from db's own settings.
    assert!(
        site.networks("db").is_empty(),
        "a force-disconnect of a stopped container drops the network from its settings"
    );

    let mut feed = Feed::default();
    let ready = ops::start(&site.stack, &mut feed);
    assert!(
        matches!(ready, Ok(true)),
        "Start must bring the site up: {ready:?}"
    );

    assert!(site.running("db"));
    let nets = site.networks("db");
    for want in [&backend, &default] {
        assert!(
            nets.iter().any(|(n, ip)| n == want && !ip.is_empty()),
            "db must have an address on {want} again: {nets:?}"
        );
    }
    assert!(site.app_reaches_db(), "app reaches `db` by name");
    eprintln!("scenario 1 took {:.0?}", t.elapsed());
}
