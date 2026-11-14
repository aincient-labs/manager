//! Scenario 10 — automatic-backup prune (B6, fixed in 11dd0cc): Managers before
//! 0.17.0 took automatic backups and never removed one. 0.17.0 keeps the 3
//! newest automatic backups, but only those taken since it started counting
//! (`.automatic-backups-since`); the older ones are the owner's to remove (Free
//! up space offers them), never pruned behind their back. Neither is an
//! owner's own backup, nor the backup a restore just loaded.
//!
//! Real backup files (copies of one real `ops::backup` archive under the names
//! the Manager gives its automatic backups) in the site's real backups folder;
//! the prune runs where it runs in the product, at the end of a successful
//! `ops::restore_with` (whose own safety backup is the newest counted one).

use std::path::Path;
use std::time::Instant;

use aincient_core::ops::{self, RestoreOptions, AUTOMATIC_SINCE_FILE};

use crate::harness::{Feed, Site, Template};

/// Pre-0.17 automatic backups: before the site started counting.
const OLDER: [&str; 3] = [
    "aincient-before-update-19991101-100000.tar.gz",
    "aincient-before-restore-19991201-100000.tar.gz",
    "aincient-before-stepped-upgrade-19991215-100000.tar.gz",
];
/// When this site's automatic backups started counting.
const SINCE: &str = "20000101-000000";
/// Counted automatic backups, oldest first.
const COUNTED: [&str; 4] = [
    "aincient-before-update-20000102-100000.tar.gz",
    "aincient-before-update-20000103-100000.tar.gz",
    "aincient-before-channel-switch-20000104-100000.tar.gz",
    "aincient-before-update-20000105-100000.tar.gz",
];

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn a_restore_keeps_the_three_newest_counted_backups_and_never_prunes_older_ones() {
    let t = Instant::now();
    let mut site = Site::new("prune");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('restored')");

    // The owner's own backup (no label): never pruned.
    let own = ops::backup(&site.stack, None, &mut Feed::default()).expect("a real backup");
    let dir = site.stack.backups_dir();
    std::fs::write(
        site.stack.home.join(AUTOMATIC_SINCE_FILE),
        format!("{SINCE}\n"),
    )
    .expect("since");
    // Oldest first, so file times agree with the stamps.
    for name in OLDER.iter().chain(COUNTED.iter()) {
        std::fs::copy(&own, dir.join(name)).expect("copy");
    }
    // Restore the oldest counted one: the backup just loaded must survive too.
    let picked = dir.join(COUNTED[0]);
    site.sql("UPDATE scn_marker SET v = 'since'");

    let mut feed = Feed::default();
    ops::restore_with(&site.stack, &picked, &RestoreOptions::default(), &mut feed)
        .expect("restored");
    assert_eq!(site.sql("SELECT v FROM scn_marker"), "restored");

    let names: Vec<String> = ops::list_backups(&site.stack)
        .into_iter()
        .map(|b| b.name)
        .collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    let safety: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("aincient-before-restore-") && !OLDER.contains(&n.as_str()))
        .collect();
    assert_eq!(safety.len(), 1, "the restore's safety backup: {names:?}");

    // Kept: the 3 newest counted (the safety backup, 0105, 0104), the restored
    // pick, every older one, and the owner's.
    for kept in [COUNTED[3], COUNTED[2], COUNTED[0]] {
        assert!(has(kept), "{kept} kept: {names:?}");
    }
    for older in OLDER {
        assert!(has(older), "pre-0.17 {older} is never pruned: {names:?}");
    }
    assert!(has(&file_name(&own)), "the owner's backup stays: {names:?}");
    // Pruned: the one counted backup beyond the newest 3 that wasn't restored.
    assert!(!has(COUNTED[1]), "{} pruned: {names:?}", COUNTED[1]);
    assert!(feed
        .lines
        .iter()
        .any(|l| l == &format!("Removed an older automatic backup: {}", COUNTED[1])));
    assert_eq!(names.len(), 1 + OLDER.len() + 3 + 1, "{names:?}");
    eprintln!("scenario 10 took {:.0?}", t.elapsed());
}

fn file_name(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}
