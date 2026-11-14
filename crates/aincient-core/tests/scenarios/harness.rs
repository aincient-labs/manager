//! The scenario harness: a tiny real compose project shaped like an appliance,
//! driven by the Manager's own `ops`/`doctor` functions against real Docker.
//!
//! A [`Site`] is a site directory (`compose.yaml`, `edge.conf`, `.env`,
//! `site-id`) written by the Manager's own scaffold into a temp dir, with
//! three services as the shipped template declares them:
//!
//! - `db`: the template's `db` with its image swapped for `postgres:16-alpine`
//!   (smaller than pgvector; the same `pg_isready` healthcheck).
//! - `app`: [`APP_DOCKERFILE`], a small Alpine image built per test. Its
//!   entrypoint waits for `db` by name, prints the real converge lines (or
//!   FATAL, see [`Site::fail_boot`]) and serves 200 on `/user/login`, so the
//!   Manager's readiness wait judges it like the real app. Its `drush` does only
//!   what the Manager's backup and restore scripts call, on the real database.
//! - `edge`: the template's `nginx:1.30-alpine-slim` with the shipped
//!   `edge.conf`, published on a free port.
//!
//! Guarantees: every compose project is `atelier-scn-<test>-<n>` and nothing
//! else is ever addressed ([`project_name`] refuses any other); on drop (pass or
//! panic) the project is taken down with its volumes and every image tag and
//! image this site built is removed. Images it did not build are never removed.
//!
//! The Manager's user home (where it finds `~/.atelier/sites.toml`, which
//! clean-up reads) is a temp dir for the whole run ([`user_home`]): no scenario
//! reads or writes the owner's real registry.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use aincient_core::docker;
use aincient_core::ops::{self, Reporter, Stage};
use aincient_core::stack::{InstallOptions, Stack, COMPOSE_TEMPLATE, DB_IMAGE};

/// Every project the harness makes starts with this; it refuses any other.
pub const PREFIX: &str = "atelier-scn-";

/// The small Postgres the scenario `db` runs instead of pgvector.
pub const SCN_DB_IMAGE: &str = "postgres:16-alpine";

/// The compose overlay [`Site::local_images`] adds: `app` is never pulled
/// (its tag exists only on this computer, like a local or fork build), `db`
/// and `edge` only when missing. The Manager's update still runs its real
/// `compose pull`, then compares the image the tag names with the one `app`
/// runs; compose just doesn't ask a registry for a tag it can't have.
pub const LOCAL_IMAGES: &str = "services:\n  app:\n    pull_policy: never\n  db:\n    pull_policy: missing\n  edge:\n    pull_policy: missing\n";

/// The image repository every app image the harness builds is tagged in.
pub const APP_REPO: &str = "atelier-scn-app";

/// The test app: waits for `db` by name like `converge.sh` does, then prints
/// converge's own success (or FATAL) lines and serves `/user/login`, as the
/// real entrypoint starts the web server after a failed converge too. Alpine
/// with the Postgres client, so [`APP_DRUSH`] can work on the real database,
/// and GNU tar as the real image has (the backup packs with several `-C`).
/// `ARG MARK` comes after the package layer, so every build shares it.
const APP_DOCKERFILE: &str = r#"FROM alpine:3.22
RUN apk add --no-cache busybox-extras postgresql16-client tar \
 && adduser -D -H -u 82 -G www-data -s /sbin/nologin www-data
ARG MARK=a
ENV SCN_MARK=$MARK
RUN mkdir -p /www/user /opt/drupal/web/sites/default/files /opt/drupal/private \
    /opt/drupal/vendor/bin \
 && echo "login $MARK" > /www/user/login
COPY entrypoint.sh /entrypoint.sh
COPY drush /opt/drupal/vendor/bin/drush
RUN chmod +x /entrypoint.sh /opt/drupal/vendor/bin/drush
STOPSIGNAL SIGTERM
ENTRYPOINT ["/entrypoint.sh"]
"#;

/// [`fail_boot`](Site::fail_boot) leaves this flag (with the build's mark
/// appended) in the `private` volume: that build then fails its converge, as
/// a release that can't migrate a site does. Another build ignores it.
pub const BOOT_FAILS_FLAG: &str = "/opt/drupal/private/scn-boot-fails-";

const APP_ENTRYPOINT: &str = r#"#!/bin/sh
# converge.sh's database wait: `db` by name on the site's network.
tries=0
until nc -z -w 2 db 5432; do
  tries=$((tries + 1))
  if [ "$tries" -ge 20 ]; then
    echo "[converge] FATAL: database not reachable after ${tries}s" >&2
    echo "[entrypoint] converge FAILED — starting server on the rolled-back state for inspection" >&2
    break
  fi
  sleep 1
done
if [ "$tries" -lt 20 ]; then
  # Out of memory during converge (scenarios 11, 11b, 12): an allocation
  # that doubles a string until the container's memory limit kills it.
  hog='BEGIN { s = "xxxxxxxxxxxxxxxx"; while (1) s = s s }'
  if [ -e "/opt/drupal/private/scn-boot-oom-$SCN_MARK" ]; then
    echo "[converge] running database updates (scenario: PID 1 runs out of memory)"
    exec awk "$hog"
  fi
  if [ -e "/opt/drupal/private/scn-boot-oom-child-$SCN_MARK" ]; then
    echo "[converge] running database updates (scenario: a child runs out of memory)"
    awk "$hog"
    rc=$?
    echo "[converge] FATAL: drush updatedb failed (exit $rc)" >&2
    echo "[entrypoint] converge FAILED — starting server on the rolled-back state for inspection" >&2
  elif [ -e "/opt/drupal/private/scn-boot-db-oom-$SCN_MARK" ]; then
    echo "[converge] running database updates (scenario: the database runs out of memory)"
    psql "postgresql://${DATABASE_URL#pgsql://}" -v ON_ERROR_STOP=1 -qAt \
      -c "SELECT length(repeat('x', 400000000))"
    rc=$?
    echo "[converge] FATAL: drush updatedb failed (exit $rc)" >&2
    echo "[entrypoint] converge FAILED — starting server on the rolled-back state for inspection" >&2
  elif [ -e "/opt/drupal/private/scn-boot-fails-$SCN_MARK" ]; then
    echo "[converge] FATAL: post-update health check failed (scenario build $SCN_MARK)" >&2
    echo "[entrypoint] converge FAILED — starting server on the rolled-back state for inspection" >&2
  else
    echo "[converge] converge OK — scenario app $SCN_MARK"
    echo "[entrypoint] converge succeeded"
  fi
fi
httpd -f -p 80 -h /www &
pid=$!
trap 'kill $pid; exit 0' TERM INT
wait $pid
"#;

/// Not Drupal: only the drush calls the Manager's backup and restore scripts
/// make (`sql:dump --gzip --result-file`, `sql:drop`, `sql:cli`,
/// `cache:rebuild`), done on the site's real database (`DATABASE_URL`) with
/// the Postgres tools. Anything else fails loudly.
const APP_DRUSH: &str = r#"#!/bin/sh
set -e
cmd=
result=
gz=
for a in "$@"; do
  case "$a" in
    --root=*|-y) ;;
    --gzip) gz=1 ;;
    --result-file=*) result="${a#--result-file=}" ;;
    -*) ;;
    *) [ -n "$cmd" ] || cmd="$a" ;;
  esac
done
url="${DATABASE_URL#pgsql://}"
export PGUSER="${url%%:*}"
rest="${url#*:}"
export PGPASSWORD="${rest%%@*}"
rest="${rest#*@}"
export PGHOST="${rest%%/*}"
export PGDATABASE="${rest#*/}"
case "$cmd" in
  sql:dump)
    [ -n "$result" ] || { echo "drush (scenario): sql:dump needs --result-file" >&2; exit 1; }
    pg_dump --no-owner --no-privileges > "$result"
    if [ -n "$gz" ]; then gzip -f "$result"; fi
    ;;
  sql:drop) psql -v ON_ERROR_STOP=1 -q -c 'DROP SCHEMA public CASCADE; CREATE SCHEMA public;' ;;
  sql:cli) exec psql -v ON_ERROR_STOP=1 -q ;;
  cache:rebuild|cr) echo "[drush] caches rebuilt (scenario)" ;;
  *) echo "drush (scenario): $cmd is not one of the calls the Manager's backup/restore make" >&2; exit 1 ;;
esac
"#;

static SEQ: AtomicU32 = AtomicU32::new(0);

/// `atelier-scn-<test>-<n>`, unique per process and call. Panics on a test
/// name that would make anything else.
pub fn project_name(test: &str) -> String {
    assert!(
        !test.is_empty()
            && test
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "scenario name must be [a-z0-9-]: {test:?}"
    );
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let name = format!("{PREFIX}{test}-{}{n}", std::process::id());
    refuse_foreign(&name);
    name
}

/// The one guard every Docker call that changes something goes through.
pub fn refuse_foreign(project: &str) {
    assert!(
        project.starts_with(PREFIX) && project.len() > PREFIX.len(),
        "the scenario harness only touches `{PREFIX}*` projects, not {project:?}"
    );
}

/// The user home the Manager sees for the whole run: a temp dir, so its
/// registry (`<home>/.atelier/sites.toml`) is the scenario's own, never the
/// owner's. Set once as `HOME` (what `dirs::home_dir` and so
/// `sites::user_home` read); `DOCKER_CONFIG` is pinned to the real Docker
/// config first, so the docker CLI keeps its context and plugins.
pub fn user_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let real = std::env::var_os("HOME").map(PathBuf::from);
        if std::env::var_os("DOCKER_CONFIG").is_none() {
            if let Some(cfg) = real.map(|h| h.join(".docker")).filter(|d| d.is_dir()) {
                std::env::set_var("DOCKER_CONFIG", cfg);
            }
        }
        let tmp = std::env::temp_dir();
        sweep_dead_homes(&tmp);
        let home = tmp.join(format!("{PREFIX}home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scenario user home");
        // The tests run one at a time (`--test-threads=1`); nothing else in
        // this process reads the environment while it changes.
        std::env::set_var("HOME", &home);
        home
    })
    .clone()
}

/// Remove the user homes earlier runs left (`atelier-scn-home-<pid>` of a
/// process that is gone): a run can't clean its own up at exit.
fn sweep_dead_homes(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(&format!("{PREFIX}home-")) else {
            continue;
        };
        let alive = Command::new("kill")
            .args(["-0", pid])
            .output()
            .is_ok_and(|o| o.status.success());
        if !alive && pid.chars().all(|c| c.is_ascii_digit()) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// An update that ends Ready runs the automatic clean-up on this computer
/// (`cleanup::free(AfterUpdate)`), which only ever takes Atelier's own
/// downloads and the old base tags Atelier shipped: never anything a scenario
/// builds. So before such an update, refuse to run unless that clean-up would
/// remove nothing here (with the scenario's empty registry), so a scenario
/// never removes an image the owner has. CI runners have none.
pub fn assert_cleanup_removes_nothing() {
    user_home();
    let found = aincient_core::cleanup::find(aincient_core::cleanup::Scope::AfterUpdate)
        .expect("ask what the automatic clean-up would remove");
    assert!(
        found.images.is_empty(),
        "this computer has unused Atelier downloads the update's automatic clean-up would \
         remove ({:?}); the scenario won't run an update that removes images it didn't make",
        found
            .images
            .iter()
            .map(|i| if i.names.is_empty() {
                &i.id
            } else {
                &i.names[0]
            })
            .collect::<Vec<_>>()
    );
}

/// Docker is here and answering; otherwise the scenario can't run at all.
pub fn require_docker() {
    user_home();
    let p = aincient_core::preflight();
    assert!(
        p.ok(),
        "scenario tests need a running Docker with the compose plugin: {:?}",
        p.problem()
    );
}

/// `docker <args>`, its stdout; panics with Docker's words on failure.
pub fn docker(args: &[&str]) -> String {
    try_docker(args).unwrap_or_else(|e| panic!("docker {}: {e}", args.join(" ")))
}

/// `docker <args>`: stdout, or stderr as the error.
pub fn try_docker(args: &[&str]) -> Result<String, String> {
    let mut c = docker::docker();
    c.args(args);
    output(c)
}

fn output(mut c: Command) -> Result<String, String> {
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free local port")
}

/// Which `compose.yaml` the site is written with.
pub enum Template {
    /// This Manager's [`COMPOSE_TEMPLATE`].
    Current,
    /// A superseded template a real site may still carry (an older Manager
    /// wrote it), so `refresh_compose` rewrites it on the next start.
    Shipped(&'static str),
}

/// A [`Reporter`] that captures Docker's output (as the GUI does) and keeps
/// every line for assertions; `--nocapture` shows them as they happen.
#[derive(Default)]
pub struct Feed {
    pub lines: Vec<String>,
}

impl Reporter for Feed {
    fn stage(&mut self, _stage: Stage, message: &str, _fraction: Option<f32>) {
        eprintln!("  [stage] {message}");
        self.lines.push(message.to_string());
    }
    fn log(&mut self, line: &str) {
        eprintln!("  [log] {line}");
        self.lines.push(line.to_string());
    }
    fn captures_output(&self) -> bool {
        true
    }
}

/// One throwaway site. Dropping it removes everything it made.
pub struct Site {
    pub stack: Stack,
    project: String,
    /// Tags this site created (removed on drop).
    tags: Vec<String>,
    /// Image IDs this site built (removed on drop, once untagged).
    image_ids: Vec<String>,
}

impl Site {
    /// A site directory for `test` in the temp dir, nothing in Docker yet.
    pub fn new(test: &str) -> Self {
        require_docker();
        let project = project_name(test);
        let home = std::env::temp_dir().join(&project);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("site dir");
        let stack = Stack {
            home,
            slug: Some(project.clone()),
            label: None,
            project: Some(project.clone()),
        };
        eprintln!("scenario site {project} at {}", stack.home.display());
        Site {
            stack,
            project,
            tags: Vec::new(),
            image_ids: Vec::new(),
        }
    }

    /// Build the test app as `atelier-scn-app:<project>-<tag>` (stamped
    /// `mark`, so two builds are two images): `(image, image ID)`. The same
    /// `tag` again moves that tag to the new build.
    pub fn build_app(&mut self, tag: &str, mark: &str) -> (String, String) {
        let image = format!("{APP_REPO}:{}-{tag}", self.project);
        let ctx = self.stack.home.join(format!(".app-build-{mark}"));
        std::fs::create_dir_all(&ctx).expect("build context");
        std::fs::write(ctx.join("Dockerfile"), APP_DOCKERFILE).expect("Dockerfile");
        std::fs::write(ctx.join("entrypoint.sh"), APP_ENTRYPOINT).expect("entrypoint");
        std::fs::write(ctx.join("drush"), APP_DRUSH).expect("drush");
        let ctx_s = ctx.to_string_lossy().to_string();
        let mark_arg = format!("MARK={mark}");
        let id = docker(&[
            "build",
            "-q",
            "--build-arg",
            &mark_arg,
            "-t",
            &image,
            &ctx_s,
        ]);
        let _ = std::fs::remove_dir_all(&ctx);
        if !self.tags.contains(&image) {
            self.tags.push(image.clone());
        }
        self.image_ids.push(id.clone());
        (image, id)
    }

    /// Write the site the way the Manager does ([`Stack::ensure_scaffold_with`]:
    /// compose.yaml, edge.conf, .env with a database password, plus site-id),
    /// on `app_image` and a free port, then give it `template` with the
    /// scenario db image.
    pub fn scaffold(&self, app_image: &str, template: Template) {
        let opts = InstallOptions {
            image: Some(app_image.to_string()),
            http_port: Some(free_port()),
        };
        self.stack
            .ensure_scaffold_with(&opts, || Some(false))
            .expect("scaffold");
        self.stack.ensure_site_id().expect("site-id");
        let compose = match template {
            Template::Current => COMPOSE_TEMPLATE,
            Template::Shipped(t) => t,
        };
        std::fs::write(self.stack.compose_path(), with_scn_db(compose)).expect("compose.yaml");
    }

    /// Add `name` with `body` next to compose.yaml and have compose read both
    /// (`COMPOSE_FILE` in `.env`, as the engine overlay is read).
    /// Several overlays stack, read in the order they were added.
    pub fn overlay(&self, name: &str, body: &str) {
        std::fs::write(self.stack.home.join(name), body).expect("overlay");
        let env = std::fs::read_to_string(self.stack.env_path()).expect(".env");
        let files = env
            .lines()
            .find_map(|l| l.strip_prefix("COMPOSE_FILE="))
            .unwrap_or("compose.yaml")
            .to_string();
        self.set_env("COMPOSE_FILE", &format!("{files}:{name}"));
    }

    /// Give `service` a memory limit of `size` (`48m`), swap included
    /// (`memswap_limit` = `mem_limit`, or swap would absorb the allocation on a
    /// host that has it). The shipped restart policy stays.
    pub fn limit_memory(&self, service: &str, size: &str) {
        self.overlay(
            &format!("scn-memory-{service}.yaml"),
            &format!("services:\n  {service}:\n    mem_limit: {size}\n    memswap_limit: {size}\n"),
        );
    }

    /// Leave `flag` (a file name the scenario app's entrypoint looks for,
    /// with build `mark` appended) in the `private` volume. Needs a running `app`.
    pub fn flag(&self, flag: &str, mark: &str) {
        docker(&[
            "exec",
            &self.container("app"),
            "touch",
            &format!("/opt/drupal/private/{flag}{mark}"),
        ]);
    }

    /// `docker compose <args>` for this site, through the Manager's own builder.
    pub fn compose(&self, args: &[&str]) -> String {
        refuse_foreign(&self.stack.project_name());
        let mut c = docker::compose(&self.stack);
        c.args(args);
        output(c).unwrap_or_else(|e| panic!("compose {}: {e}", args.join(" ")))
    }

    /// First boot: `compose up -d` as an install does, then the Manager's
    /// readiness wait must say Ready.
    pub fn up_ready(&self) {
        self.compose(&["up", "-d", "--quiet-pull"]);
        let boot =
            ops::wait_until_ready(&self.stack, Duration::from_secs(120), &mut Feed::default());
        assert_eq!(
            boot,
            aincient_core::Boot::Ready,
            "first boot of {}",
            self.project
        );
    }

    /// The container name of `service` (running or not).
    pub fn container(&self, service: &str) -> String {
        let id = self.compose(&["ps", "-a", "-q", service]);
        let id = id.lines().next().unwrap_or_default().trim().to_string();
        assert!(!id.is_empty(), "{service} has no container");
        docker(&["inspect", "-f", "{{.Name}}", &id])
            .trim_start_matches('/')
            .to_string()
    }

    /// `docker inspect -f <format>` of `service`'s container.
    pub fn inspect(&self, service: &str, format: &str) -> String {
        docker(&["inspect", "-f", format, &self.container(service)])
    }

    /// `network → IPAddress` of `service`'s container, as Docker's own settings
    /// for it list them.
    pub fn networks(&self, service: &str) -> Vec<(String, String)> {
        let out = self.inspect(
            service,
            "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{$v.IPAddress}}\n{{end}}",
        );
        let mut nets: Vec<(String, String)> = out
            .lines()
            .filter_map(|l| {
                let mut p = l.split_whitespace();
                Some((p.next()?.to_string(), p.next().unwrap_or("").to_string()))
            })
            .collect();
        nets.sort();
        nets
    }

    pub fn running(&self, service: &str) -> bool {
        self.inspect(service, "{{.State.Running}}") == "true"
    }

    /// Whether `app` reaches the database by its service name, from inside.
    pub fn app_reaches_db(&self) -> bool {
        try_docker(&[
            "exec",
            &self.container("app"),
            "nc",
            "-z",
            "-w",
            "3",
            "db",
            "5432",
        ])
        .is_ok()
    }

    /// The project's network for compose key `key` (`default` → `<project>_default`).
    pub fn network(&self, key: &str) -> String {
        format!("{}_{key}", self.project)
    }

    /// This site's compose project (`atelier-scn-…`).
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Have compose use the images on this computer ([`LOCAL_IMAGES`]). Call
    /// after [`scaffold`](Self::scaffold), before the first boot.
    pub fn local_images(&self) {
        self.overlay("scn-local-images.yaml", LOCAL_IMAGES);
    }

    /// Set `key=value` in the site's `.env`, replacing the key's line if it
    /// has one, as the owner editing their settings does.
    pub fn set_env(&self, key: &str, value: &str) {
        let path = self.stack.env_path();
        let text = std::fs::read_to_string(&path).expect(".env");
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| l.split_once('=').map(|(k, _)| k.trim()) != Some(key))
            .map(str::to_string)
            .collect();
        lines.push(format!("{key}={value}"));
        std::fs::write(&path, lines.join("\n") + "\n").expect(".env");
    }

    /// From now on build `mark` fails its converge on this site's data
    /// ([`BOOT_FAILS_FLAG`] in the `private` volume), as a release that can't
    /// migrate the site does; other builds boot. Needs a running `app`.
    pub fn fail_boot(&self, mark: &str) {
        let flag = BOOT_FAILS_FLAG.trim_start_matches("/opt/drupal/private/");
        self.flag(flag, mark);
    }

    /// `query` run by `psql` in `db` as the site's user: the rows, unaligned.
    pub fn sql(&self, query: &str) -> String {
        docker(&[
            "exec",
            &self.container("db"),
            "psql",
            "-U",
            "aincient",
            "-d",
            "aincient",
            "-v",
            "ON_ERROR_STOP=1",
            "-tAc",
            query,
        ])
    }

    /// Remove `tag` with this site (a tag the test put on an image it
    /// built). Refuses a name without [`PREFIX`] in it.
    pub fn owns_tag(&mut self, tag: &str) {
        assert!(tag.contains(PREFIX), "not a scenario tag: {tag:?}");
        if !self.tags.iter().any(|t| t == tag) {
            self.tags.push(tag.to_string());
        }
    }
}

/// `template` with the scenario db image in place of the shipped one. The
/// Manager reads the db image from the file the same way (`refreshed_compose`
/// carries a changed image line over), so a shipped template stays one.
pub fn with_scn_db(template: &str) -> String {
    let from = format!("    image: {DB_IMAGE}\n");
    assert!(template.contains(&from), "the template names {DB_IMAGE}");
    template.replace(&from, &format!("    image: {SCN_DB_IMAGE}\n"))
}

impl Drop for Site {
    fn drop(&mut self) {
        // Never anything but our own project, even on the way out of a panic.
        if !self.project.starts_with(PREFIX) || self.stack.project_name() != self.project {
            eprintln!("refusing to clean up a foreign project {:?}", self.project);
            return;
        }
        let mut down = docker::compose(&self.stack);
        down.args(["down", "-v", "--remove-orphans", "-t", "2"]);
        if let Err(e) = output(down) {
            eprintln!("cleanup: compose down of {}: {e}", self.project);
        }
        for tag in &self.tags {
            if tag.contains(PREFIX) {
                let _ = try_docker(&["image", "rm", tag]);
            }
        }
        for id in &self.image_ids {
            // Only once untagged by the loop above: an ID still tagged elsewhere stays.
            let _ = try_docker(&["image", "rm", id]);
        }
        remove_site_dir(&self.stack.home);
    }
}

fn remove_site_dir(home: &Path) {
    let tmp: PathBuf = std::env::temp_dir();
    if home.starts_with(&tmp)
        && home
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PREFIX))
    {
        // A scenario may have made its backups folder read-only.
        writable(&home.join("backups"));
        let _ = std::fs::remove_dir_all(home);
    }
}

/// Give `dir` back its owner's write permission (best effort).
pub fn writable(dir: &Path) {
    set_mode(dir, 0o755);
}

/// Set `dir`'s Unix permission bits (best effort; nothing elsewhere).
pub fn set_mode(dir: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if dir.is_dir() {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode));
        }
    }
    #[cfg(not(unix))]
    let _ = (dir, mode);
}
