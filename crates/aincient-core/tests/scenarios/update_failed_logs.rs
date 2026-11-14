//! Scenario 18 — a failed update saves every service's log (M2c,
//! `update_logs`): restarting a container resets its state and `docker logs`
//! lives only as long as the container, so the evidence of a failed update
//! went with the next Start. Now the update writes
//! `<site home>/logs/update-<time>.log` before anything restarts them, and its
//! failure names the file. The file never holds the site's secrets.
//!
//! The new build fails its converge on this site's data ([`Site::fail_boot`],
//! as in scenario 6); `--no-backup`, so the update reaches the boot.

use std::time::Instant;

use aincient_core::boot::BootFailed;
use aincient_core::ops::{self, UpdateOptions};

use crate::harness::{Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn a_failed_update_saves_every_services_log_without_secrets() {
    let t = Instant::now();
    let mut site = Site::new("update-logs");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.up_ready();
    site.fail_boot("b");
    let (_, new) = site.build_app("app", "b");
    let password = site
        .stack
        .env_get("DB_PASSWORD")
        .expect("the scaffold wrote a database password");
    assert!(password.len() >= 8, "a real password: {password:?}");

    let mut feed = Feed::default();
    let failed = ops::update_with(&site.stack, &UpdateOptions { no_backup: true }, &mut feed)
        .expect_err("a build whose converge fails is a failed update");
    assert!(
        failed.downcast_ref::<BootFailed>().is_some(),
        "still a failed boot (the window's failure panel): {failed:#}"
    );
    assert_eq!(
        site.inspect("app", "{{.Image}}"),
        new,
        "it ran the new build"
    );

    let dir = site.stack.home.join("logs");
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{} exists: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("update-") && n.ends_with(".log"))
        })
        .collect();
    assert_eq!(files.len(), 1, "one saved log: {files:?}");
    let file = &files[0];
    let shown = file.display().to_string();
    let said = format!("{failed:#}");
    assert!(said.contains(&shown), "the failure names the file: {said}");
    assert!(
        feed.lines.iter().any(|l| l.contains(&shown)),
        "and the step feed (the window's Details) says it"
    );

    let text = std::fs::read_to_string(file).expect("read the saved log");
    assert!(text.contains("===== docker compose ps -a ====="), "{text}");
    for service in ["app", "db", "edge"] {
        let header = |what: &str| {
            text.lines().any(|l| {
                l.starts_with(&format!("===== {service} ("))
                    && l.ends_with(&format!("): {what} ====="))
            })
        };
        assert!(header("state"), "{service}'s state section:\n{text}");
        assert!(
            header("last 2000 log lines"),
            "{service}'s log section:\n{text}"
        );
    }
    assert!(
        text.contains("[converge] FATAL: post-update health check failed (scenario build b)"),
        "the app's FATAL line:\n{text}"
    );
    assert!(text.contains("\"OOMKilled\""), "the State: {text}");
    // No secrets: not the password, not the environment it is set in.
    assert!(!text.contains(&password), "the database password leaked");
    for env in [
        "DB_PASSWORD",
        "POSTGRES_PASSWORD",
        "DATABASE_URL",
        "\"Env\"",
    ] {
        assert!(!text.contains(env), "{env} in the saved log:\n{text}");
    }
    eprintln!("scenario 18 took {:.0?}", t.elapsed());
}
