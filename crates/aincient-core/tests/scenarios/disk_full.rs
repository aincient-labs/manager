//! Scenario 13 — Docker's disk is full when an update backs the site up first
//! (M2b). The backup stages its dump inside the app container (`/tmp`, on
//! Docker's disk) before copying it out; here that `/tmp` is a 1 MB tmpfs
//! volume (`driver_opts type=tmpfs`) and the database holds several MB, so
//! `pg_dump` hits a real "No space left on device". The update must refuse
//! ([`BackupBeforeUpdateFailed`], as since 247e10b) and say in plain words
//! that Docker's disk is full: the refusal quoted pg_dump's raw error, and
//! the classifier that knows the words only matched Docker's lower-case
//! spelling, not libc's ("No space…").

use std::time::Instant;

use aincient_core::ops::{self, BackupBeforeUpdateFailed, UpdateOptions};

use crate::harness::{Feed, Site, Template};

const SMALL_TMP: &str = "services:
  app:
    volumes:
      - scn-tmp:/tmp
volumes:
  scn-tmp:
    driver_opts:
      type: tmpfs
      device: tmpfs
      o: size=1m
";

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn an_update_whose_backup_fills_dockers_disk_says_the_disk_is_full() {
    let t = Instant::now();
    let mut site = Site::new("disk-full");
    let (app, old) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.overlay("scn-small-tmp.yaml", SMALL_TMP);
    site.up_ready();
    // ~6 MB of rows that don't compress away: more than the 1 MB staging space.
    site.sql(
        "CREATE TABLE scn_fill AS SELECT md5(i::text) || md5((i * 7)::text) AS t \
         FROM generate_series(1, 100000) AS i",
    );
    let (_, new) = site.build_app("app", "b");
    assert_ne!(old, new);

    let refused = ops::update_with(&site.stack, &UpdateOptions::default(), &mut Feed::default())
        .expect_err("an update whose backup can't be written must stop");
    assert!(
        refused.downcast_ref::<BackupBeforeUpdateFailed>().is_some(),
        "the typed refusal: {refused:#}"
    );
    let said = format!("{refused:#}");
    assert!(
        said.contains("No space left on device"),
        "a real full disk: {said}"
    );
    assert!(
        said.contains("Docker has run out of disk space. Free up space"),
        "says the disk is full in plain words: {said}"
    );
    assert_eq!(site.inspect("app", "{{.Image}}"), old, "not updated");
    assert!(site.running("app"), "the site keeps running");
    eprintln!("scenario 13 took {:.0?}", t.elapsed());
}
