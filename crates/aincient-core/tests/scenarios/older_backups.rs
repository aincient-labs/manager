//! Scenario 24 — Free up space removing older automatic backups (11dd0cc, new
//! in 0.17): automatic backups taken before this site started counting are
//! never pruned behind the owner's back; Free up space lists them
//! (`cleanup::older_backups_of`) and removes only the ones the owner ticked
//! (`cleanup::remove_older_backups`), as `atelier app cleanup --older-backups`
//! and the app do. It asks afresh: a ticked path that isn't an older automatic
//! backup (the owner's own, a counted one) is never removed, whatever the list
//! it was handed says.
//!
//! Real backup files: copies of one real `ops::backup` archive under the names
//! the Manager gives its automatic backups, in the site's real backups folder.

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use aincient_core::cleanup::{older_backups_of, remove_older_backups};
use aincient_core::ops::{self, AUTOMATIC_SINCE_FILE};

use crate::harness::{Feed, Site, Template};

/// Before the site started counting (older Managers' automatic backups).
const OLDER: [&str; 3] = [
    "aincient-before-update-19991101-100000.tar.gz",
    "aincient-before-restore-19991201-100000.tar.gz",
    "aincient-before-stepped-upgrade-19991215-100000.tar.gz",
];
const SINCE: &str = "20000101-000000";
/// Counted automatic backups (the prune's, never Free up space's).
const COUNTED: [&str; 2] = [
    "aincient-before-update-20000102-100000.tar.gz",
    "aincient-before-update-20000103-100000.tar.gz",
];

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn free_up_space_removes_only_the_ticked_older_backups() {
    let t = Instant::now();
    let mut site = Site::new("older-backups");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();

    let own = ops::backup(&site.stack, None, &mut Feed::default()).expect("a real backup");
    let dir = site.stack.backups_dir();
    std::fs::write(
        site.stack.home.join(AUTOMATIC_SINCE_FILE),
        format!("{SINCE}\n"),
    )
    .expect("since");
    for name in OLDER.iter().chain(COUNTED.iter()) {
        std::fs::copy(&own, dir.join(name)).expect("copy");
    }

    // What Free up space shows for this site: the older ones only.
    let shown = older_backups_of(&site.stack).expect("older backups are listed");
    let mut listed: Vec<&str> = shown.backups.iter().map(|b| b.name.as_str()).collect();
    listed.sort_unstable();
    let mut want = OLDER.to_vec();
    want.sort_unstable();
    assert_eq!(listed, want, "only the pre-counting automatic backups");
    let size = std::fs::metadata(&own).unwrap().len();
    assert_eq!(shown.total_bytes, size * OLDER.len() as u64);

    // The owner ticks two of the three, and the list handed over also names
    // the owner's own backup and a counted one.
    let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let ticked: HashSet<String> = [
        path(OLDER[0]),
        path(OLDER[2]),
        own.to_string_lossy().into_owned(),
        path(COUNTED[0]),
    ]
    .into_iter()
    .collect();
    let done = remove_older_backups(std::slice::from_ref(&site.stack), &ticked);

    let mut removed = done.removed.clone();
    removed.sort_unstable();
    let mut want = vec![OLDER[0].to_string(), OLDER[2].to_string()];
    want.sort_unstable();
    assert_eq!(removed, want, "the two ticked older backups: {done:?}");
    assert!(done.failed.is_empty(), "{done:?}");
    assert_eq!(done.total_bytes, size * 2);

    let names: Vec<String> = ops::list_backups(&site.stack)
        .into_iter()
        .map(|b| b.name)
        .collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    assert!(
        !has(OLDER[0]) && !has(OLDER[2]),
        "ticked ones gone: {names:?}"
    );
    assert!(has(OLDER[1]), "the unticked older one stays: {names:?}");
    for kept in COUNTED {
        assert!(has(kept), "counted {kept} stays: {names:?}");
    }
    assert!(has(&file_name(&own)), "the owner's backup stays: {names:?}");
    assert_eq!(names.len(), 1 + 1 + COUNTED.len(), "{names:?}");

    // Listed again: only the one left.
    let left = older_backups_of(&site.stack).expect("one older backup left");
    assert_eq!(
        left.backups
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>(),
        [OLDER[1]]
    );
    eprintln!("scenario 24 took {:.0?}", t.elapsed());
}

fn file_name(p: &Path) -> String {
    p.file_name().unwrap().to_string_lossy().into_owned()
}
