//! Scenario 22 — the stale-endpoint clear itself, run by Docker (the recovery
//! is e0a2bb8; reconnecting every network it was on, 4c1b422): when a start
//! step fails and a container of the site that isn't running still holds an
//! endpoint, Start force-disconnects that endpoint, connects the container
//! back under the names it had there, and runs the step once more; if that
//! fails too, it says to restart Docker (`NeedsDockerRestart`) instead of
//! Docker's own words. It must never leave the container off a network.
//!
//! Docker's genuine stale endpoint (a *stopped* container a network still
//! lists) can't be made on demand: a crash-looping container gives its
//! endpoint up between restarts, and `docker network connect` of a stopped one
//! only writes its settings, no endpoint (both checked on Docker 29). A
//! *paused* container is the one state compose reports as not running while
//! its network still lists it, and `compose start` fails on it ("cannot start
//! a paused container"). So the first step really fails, `stale_endpoints`
//! really finds it, the clear runs its own `network disconnect -f` and
//! `network connect --alias …` on every network db is on (`default` and a
//! second one, `backend`, as an engine site's edge has two), and the retry
//! fails again (still paused): the end state is the recovery's last word.
//! Unpausing (what the owner's Docker restart stands for), once db's
//! healthcheck passes again, then starts the site with db reachable by name
//! and its data.

use std::time::{Duration, Instant};

use aincient_core::docker_runtime::NeedsDockerRestart;
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
fn start_clears_a_held_endpoint_keeps_every_network_and_then_asks_for_a_docker_restart() {
    let t = Instant::now();
    let mut site = Site::new("endpoint-clear");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.overlay("compose.scn.yaml", OVERLAY);
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('kept')");

    let default = site.network("default");
    let backend = site.network("backend");
    let db = site.container("db");
    let db_id = site.id("db");
    docker(&["pause", &db]);
    for net in [&default, &backend] {
        let members = docker(&[
            "network",
            "inspect",
            "-f",
            "{{range .Containers}}{{.Name}}\n{{end}}",
            net,
        ]);
        assert!(
            members.lines().any(|l| l.trim() == db),
            "{net} lists the paused db: {members}"
        );
    }

    let mut feed = Feed::default();
    let stuck = ops::start(&site.stack, &mut feed).expect_err("a paused db can't be started");
    assert!(
        stuck.downcast_ref::<NeedsDockerRestart>().is_some(),
        "the recovery's last word is a Docker restart: {stuck:#}"
    );
    let said = format!("{stuck:#}");
    assert!(
        said.contains("even after clearing what it had left behind")
            && said.contains("Your site's data is safe")
            && said.contains("paused"),
        "plain words, then what Docker said both times: {said}"
    );
    for net in [&default, &backend] {
        let cleared = format!("Cleared a leftover network entry: {db} in {net}");
        let back = format!("Reconnected {db} to {net}");
        for line in [&cleared, &back] {
            assert!(
                feed.lines.iter().any(|l| l == line),
                "the clear ran on Docker: {line:?} in {:?}",
                feed.lines
            );
        }
    }
    // The clear took db off each network and connected it back, under `db`.
    let aliases = |net: &str| {
        site.inspect(
            "db",
            &format!(
                "{{{{with index .NetworkSettings.Networks \"{net}\"}}}}\
                 {{{{join .Aliases \",\"}}}}|{{{{.IPAddress}}}}{{{{end}}}}"
            ),
        )
    };
    for net in [&default, &backend] {
        let got = aliases(net);
        let (names, ip) = got.split_once('|').unwrap_or(("", ""));
        assert!(
            names.split(',').any(|a| a == "db") && !ip.trim().is_empty(),
            "db is on {net} under `db`, with an address: {got:?}"
        );
    }
    assert_eq!(site.id("db"), db_id, "no container was touched");

    // The owner's Docker restart starts db afresh; unpausing doesn't reset
    // the health a paused healthcheck lost, so wait for its next pass first
    // (compose refuses an `unhealthy` dependency outright).
    docker(&["unpause", &db]);
    let waited = Instant::now();
    while site.inspect("db", "{{.State.Health.Status}}") != "healthy" {
        assert!(
            waited.elapsed() < Duration::from_secs(90),
            "db never turned healthy after the unpause"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let ready = ops::start(&site.stack, &mut Feed::default());
    assert!(
        matches!(ready, Ok(true)),
        "Start brings the site up: {ready:?}"
    );
    assert!(site.app_reaches_db(), "app reaches `db` by name");
    assert_eq!(site.sql("SELECT v FROM scn_marker"), "kept", "data kept");
    eprintln!("scenario 22 took {:.0?}", t.elapsed());
}
