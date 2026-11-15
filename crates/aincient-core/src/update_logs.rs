//! Save every service's log after a failed update (M2c,
//! `studies/manager-prior-art.md` item 10), before anything restarts the
//! containers and the evidence goes: a restart resets a container's state, and
//! `docker logs` lives only as long as the container. Home Assistant's
//! Supervisor copies its log aside the same way before acting
//! (`home-assistant-rollback.log`).
//!
//! The file is `<site home>/logs/update-<YYYYMMDD-HHMMSS>.log`: `docker compose
//! ps -a`, then per service its container's `State` (from `docker inspect`) and
//! its last [`LINES`] log lines with Docker's timestamps, stdout and stderr in
//! the order they were written. The newest [`KEEP`] such files stay.
//!
//! **No secrets.** Only `State` and `RestartCount` are read from `docker
//! inspect` (never `Config`, so never `Config.Env`), nothing reads `.env`
//! into the file, and as a last guard every secret-looking `.env` value
//! ([`is_secret_key`]: a password, salt, key, token, secret) that a service
//! printed anyway is replaced by `[redacted]`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::docker::{self, compose};
use crate::stack::Stack;

/// The folder in a site's home that holds them.
pub const DIR: &str = "logs";

/// How many saved update logs a site keeps.
pub const KEEP: usize = 10;

/// Log lines saved per service.
pub const LINES: usize = 2000;

/// One Docker call's deadline: Docker may be what failed.
const CALL: Duration = Duration::from_secs(30);

/// What replaces a secret in the file.
const REDACTED: &str = "[redacted]";

/// Where [`save`] writes for `stack`.
pub fn dir(stack: &Stack) -> PathBuf {
    stack.home.join(DIR)
}

/// Write the services' logs and states to a new file in [`dir`] and keep the
/// newest [`KEEP`]. Returns the file.
pub fn save(stack: &Stack) -> Result<PathBuf> {
    let now = chrono::Local::now();
    let mut sections = vec![Section {
        title: "docker compose ps -a".into(),
        body: compose_ps(stack),
    }];
    for (name, service) in containers(stack) {
        sections.push(Section {
            title: format!("{service} ({name}): state"),
            body: state(&name),
        });
        sections.push(Section {
            title: format!("{service} ({name}): last {LINES} log lines"),
            body: logs(&name),
        });
    }
    let text = render(
        &format!(
            "Atelier: service logs saved after a failed update, {}\nSite: {}\n",
            now.format("%Y-%m-%d %H:%M:%S %z"),
            stack.display_name()
        ),
        &sections,
    );
    let text = redact(&text, &stack.read_env());

    let dir = dir(stack);
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    private(&dir, 0o700);
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let mut path = dir.join(format!("update-{stamp}.log"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("update-{stamp}-{n}.log"));
        n += 1;
    }
    // scenario: a_failed_update_saves_every_services_log_without_secrets
    std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
    private(&path, 0o600);
    prune(&dir, KEEP);
    Ok(path)
}

/// Owner-only on Unix (logs can name the site's users and paths).
fn private(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

struct Section {
    title: String,
    body: Result<String, String>,
}

fn render(header: &str, sections: &[Section]) -> String {
    let mut out = String::from(header);
    for s in sections {
        out.push_str(&format!("\n===== {} =====\n", s.title));
        match &s.body {
            Ok(body) if body.trim().is_empty() => out.push_str("(nothing)\n"),
            Ok(body) => {
                out.push_str(body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
            }
            Err(e) => out.push_str(&format!("(couldn't read it: {e})\n")),
        }
    }
    out
}

/// A `.env` key whose value is a secret.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    ["PASSWORD", "PASS", "SECRET", "SALT", "KEY", "TOKEN"]
        .iter()
        .any(|w| k.contains(w))
}

/// `text` with every secret `.env` value ([`is_secret_key`], 4 characters or
/// more, so a short one can't blank out ordinary words) replaced.
fn redact(text: &str, env: &BTreeMap<String, String>) -> String {
    let mut secrets: Vec<&str> = env
        .iter()
        .filter(|(k, v)| is_secret_key(k) && v.trim().len() >= 4)
        .map(|(_, v)| v.trim())
        .collect();
    // Longest first, so one secret inside another goes whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    for s in secrets {
        out = out.replace(s, REDACTED);
    }
    out
}

/// Remove all but the newest `keep` `update-*.log` files in `dir` (their
/// names sort by time).
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("update-") && n.ends_with(".log"))
        })
        .collect();
    logs.sort();
    let extra = logs.len().saturating_sub(keep);
    for old in &logs[..extra] {
        // scenario: none — keeping the newest 10 saved logs; no scenario saves more than one (unit-tested only)
        let _ = std::fs::remove_file(old);
    }
}

/// `cmd`'s stdout, or why not.
fn run(mut cmd: std::process::Command) -> Result<std::process::Output, String> {
    let out = docker::output_within(&mut cmd, CALL).map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn compose_ps(stack: &Stack) -> Result<String, String> {
    let mut c = compose(stack);
    c.args(["ps", "-a"]);
    run(c).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// `(container, service)` for every container of the site, by service.
fn containers(stack: &Stack) -> Vec<(String, String)> {
    let mut c = compose(stack);
    c.args(["ps", "-a", "--format", "json"]);
    let Ok(out) = run(c) else {
        return Vec::new();
    };
    let mut found: Vec<(String, String)> =
        crate::ops::parse_ps(&String::from_utf8_lossy(&out.stdout))
            .into_iter()
            .filter(|e| !e.name.is_empty())
            .map(|e| (e.name, e.service))
            .collect();
    found.sort_by(|a, b| (&a.1, &a.0).cmp(&(&b.1, &b.0)));
    found
}

/// `docker inspect` of the container's `State` and restart count only: never
/// `Config` (its `Env` holds the database password).
fn state(name: &str) -> Result<String, String> {
    let mut c = docker::docker();
    c.args([
        "inspect",
        "--format",
        "{{json .State}}\nRestartCount: {{.RestartCount}}",
        name,
    ]);
    run(c).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The container's last [`LINES`] lines with Docker's timestamps, stdout and
/// stderr merged in the order written.
fn logs(name: &str) -> Result<String, String> {
    let mut c = docker::docker();
    let lines = LINES.to_string();
    c.args(["logs", "--timestamps", "--tail", &lines, name]);
    run(c).map(|o| {
        crate::boot::interleave_timestamped(
            &String::from_utf8_lossy(&o.stdout),
            &String::from_utf8_lossy(&o.stderr),
        )
    })
}

/// [`save`], said through `r` and folded into the update's error, so the
/// failure message (the CLI's last line, the window's failure panel and its
/// Copy text) names the file. A failed boot keeps its type; anything else
/// gets the path as context. Saving is best effort: a failure to save is
/// logged and the update's own error stands.
pub(crate) fn save_after_failure(
    stack: &Stack,
    e: anyhow::Error,
    r: &mut dyn crate::ops::Reporter,
) -> anyhow::Error {
    let path = match save(stack) {
        Ok(p) => p,
        Err(why) => {
            r.log(&format!(
                "Couldn't save the services' logs from the failed update ({why:#})."
            ));
            return e;
        }
    };
    let saved = saved_words(&path);
    r.log(&saved);
    fold(e, &saved)
}

/// `e` naming the saved file: a failed boot keeps its type (the window's
/// failure panel shows its words), anything else gets it as context.
fn fold(e: anyhow::Error, saved: &str) -> anyhow::Error {
    match e.downcast::<crate::boot::BootFailed>() {
        Ok(failed) => crate::boot::BootFailed(format!("{} {saved}", failed.0)).into(),
        Err(e) => e.context(format!("the update didn't finish. {saved}")),
    }
}

/// "The services' logs from the failed update are saved in <path>."
pub fn saved_words(path: &Path) -> String {
    format!(
        "The services' logs from the failed update are saved in {}.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_env_values_never_reach_the_file() {
        let env: BTreeMap<String, String> = [
            ("DB_PASSWORD", "s3cr3t-db-pass"),
            ("HASH_SALT", "abcdef0123456789abcdef"),
            ("SOME_API_KEY", "sk-live-xyz"),
            ("AUTH_TOKEN", "tok_12345"),
            ("HTTP_PORT", "8080"),
            ("AINCIENT_IMAGE", "ghcr.io/aincient-labs/atelier-cms:stable"),
            ("SHORT_PASS", "ab"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let sections = [
            Section {
                title: "app (site-app-1): last 2000 log lines".into(),
                body: Ok(
                    "2026-11-14T10:00:00.000000000Z connecting with s3cr3t-db-pass\n\
                          2026-11-14T10:00:01.000000000Z salt abcdef0123456789abcdef key \
                          sk-live-xyz tok_12345\n\
                          2026-11-14T10:00:02.000000000Z about to listen on 8080 \
                          ghcr.io/aincient-labs/atelier-cms:stable\n"
                        .into(),
                ),
            },
            Section {
                title: "db (site-db-1): state".into(),
                body: Err("No such container".into()),
            },
        ];
        let text = redact(&render("header\n", &sections), &env);
        for secret in [
            "s3cr3t-db-pass",
            "abcdef0123456789abcdef",
            "sk-live-xyz",
            "tok_12345",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert_eq!(text.matches(REDACTED).count(), 4, "{text}");
        // What isn't a secret stays, so the log still reads.
        assert!(text.contains("about to listen on 8080"));
        assert!(text.contains("atelier-cms:stable"));
        assert!(text
            .contains("===== db (site-db-1): state =====\n(couldn't read it: No such container)"));
        // A two-letter "secret" (`ab`) must not blank out "about".
        assert!(!text.contains("[redacted]out"), "{text}");
    }

    #[test]
    fn only_state_is_read_from_inspect_never_config() {
        // The inspect template is the guarantee: it names State and the
        // restart count, nothing under Config (whose Env holds secrets).
        let src = include_str!("update_logs.rs");
        let call = src
            .split("fn state(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap();
        assert!(call.contains("{{json .State}}"));
        assert!(!call.contains("Config"), "{call}");
        assert!(!call.contains("{{json .}}"), "{call}");
    }

    #[test]
    fn the_newest_ten_stay() {
        let dir = std::env::temp_dir().join(format!("aincient-update-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..13 {
            std::fs::write(dir.join(format!("update-20261114-1000{i:02}.log")), "x").unwrap();
        }
        std::fs::write(dir.join("other.txt"), "keep").unwrap();
        prune(&dir, KEEP);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), KEEP + 1, "{left:?}");
        assert_eq!(left[0], "other.txt");
        assert_eq!(
            left[1], "update-20261114-100003.log",
            "the three oldest went"
        );
        assert_eq!(left[KEEP], "update-20261114-100012.log");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_boot_keeps_its_type_and_names_the_file() {
        let path = Path::new("/home/me/.atelier/sites/a/logs/update-20261114-100000.log");
        let saved = saved_words(path);
        assert_eq!(
            saved,
            "The services' logs from the failed update are saved in \
             /home/me/.atelier/sites/a/logs/update-20261114-100000.log."
        );
        let boot = fold(crate::boot::BootFailed("It failed.".into()).into(), &saved);
        let boot = boot
            .downcast::<crate::boot::BootFailed>()
            .expect("still a BootFailed");
        assert_eq!(boot.0, format!("It failed. {saved}"));
        let other = fold(anyhow::anyhow!("step 2 of 3 didn't finish"), &saved);
        assert_eq!(
            format!("{other:#}"),
            format!("the update didn't finish. {saved}: step 2 of 3 didn't finish")
        );
    }

    #[test]
    fn logs_keep_their_timestamps_in_written_order() {
        let merged = crate::boot::interleave_timestamped(
            "2026-11-14T10:00:00.000000001Z a\n2026-11-14T10:00:02.000000000Z c\n",
            "2026-11-14T10:00:01.000000000Z b FATAL\n",
        );
        assert_eq!(
            merged,
            "2026-11-14T10:00:00.000000001Z a\n2026-11-14T10:00:01.000000000Z b FATAL\n\
             2026-11-14T10:00:02.000000000Z c\n"
        );
    }
}
