//! Scenario 4 — changed settings, same image (A2, fixed in 4c1b422): the owner
//! changes `.env`, and there is no newer version. Update used to find the
//! download was the build the site runs and stop at "Already up to date",
//! leaving the containers on their old settings with no way to apply them.
//! Update must recreate them on the same image (no backup: the version didn't
//! change), and a second update must find nothing to do.
//!
//! "Same image" without a registry: the app's tag exists only on this
//! computer, so the site's compose reads [`Site::local_images`]
//! (`pull_policy: never` for `app`). The Manager's own `compose pull`, its
//! image-ID comparison and its config-hash comparison all run as they are.

use std::time::Instant;

use aincient_core::ops::{self, UpdateOptions, Updated};

use crate::harness::{Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_applies_changed_settings_on_the_same_image_without_a_backup() {
    let t = Instant::now();
    let mut site = Site::new("settings");
    let (app, image_id) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.up_ready();

    let app_container = |site: &Site| site.inspect("app", "{{.Id}}");
    let trusted = |site: &Site| {
        site.inspect("app", "{{range .Config.Env}}{{println .}}{{end}}")
            .lines()
            .find_map(|l| {
                l.strip_prefix("AINCIENT_TRUSTED_HOSTS=")
                    .map(str::to_string)
            })
            .unwrap_or_default()
    };
    let before = app_container(&site);
    assert_eq!(trusted(&site), "");

    // The owner's change: a setting the app reads.
    site.set_env("AINCIENT_TRUSTED_HOSTS", "scn.example");

    let mut feed = Feed::default();
    let done = ops::update_with(&site.stack, &UpdateOptions::default(), &mut feed);
    assert!(
        matches!(done, Ok(Updated::SettingsApplied)),
        "Update must apply the changed settings: {done:?}"
    );
    assert!(feed.lines.iter().any(|l| l == ops::SETTINGS_APPLIED));
    assert_eq!(site.inspect("app", "{{.Image}}"), image_id, "same image");
    let after = app_container(&site);
    assert_ne!(after, before, "app was made again with the new settings");
    assert_eq!(trusted(&site), "scn.example");
    assert!(site.running("app") && site.running("db") && site.running("edge"));
    assert!(
        ops::list_backups(&site.stack).is_empty(),
        "no backup for a settings change: {:?}",
        ops::list_backups(&site.stack)
    );

    // Nothing left to apply: plain up to date, the container untouched.
    let again = ops::update_with(&site.stack, &UpdateOptions::default(), &mut Feed::default());
    assert!(
        matches!(again, Ok(Updated::AlreadyCurrent)),
        "a second update has nothing to do: {again:?}"
    );
    assert_eq!(app_container(&site), after, "not recreated again");
    assert!(ops::list_backups(&site.stack).is_empty());
    eprintln!("scenario 4 took {:.0?}", t.elapsed());
}
