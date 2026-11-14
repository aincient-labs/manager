//! The scenario harness: a tiny real compose project shaped like an appliance,
//! driven by the Manager's own `ops`/`doctor` functions against real Docker.
//!
//! A [`Site`] is a site directory (`compose.yaml`, `edge.conf`, `.env`,
//! `site-id`) written by the Manager's own scaffold into a temp dir, with
//! three services as the shipped template declares them:
//!
//! - `db`: the template's `db` with its image swapped for `postgres:16-alpine`
//!   (smaller than pgvector; the same `pg_isready` healthcheck).
//! - `app`: [`APP_DOCKERFILE`], a busybox image built per test. Its entrypoint
//!   waits for `db` by name, prints the real converge lines and serves 200 on
//!   `/user/login`, so the Manager's readiness wait judges it like the real app.
//! - `edge`: the template's `nginx:1.30-alpine-slim` with the shipped
//!   `edge.conf`, published on a free port.
//!
//! Guarantees: every compose project is `atelier-scn-<test>-<n>` and nothing
//! else is ever addressed ([`project_name`] refuses any other); on drop (pass or
//! panic) the project is taken down with its volumes and every image tag and
//! image this site built is removed. Images it did not build are never removed.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use aincient_core::docker;
use aincient_core::ops::{self, Reporter, Stage};
use aincient_core::stack::{InstallOptions, Stack, COMPOSE_TEMPLATE, DB_IMAGE};

/// Every project the harness makes starts with this; it refuses any other.
pub const PREFIX: &str = "atelier-scn-";

/// The small Postgres the scenario `db` runs instead of pgvector.
pub const SCN_DB_IMAGE: &str = "postgres:16-alpine";

/// The image repository every app image the harness builds is tagged in.
pub const APP_REPO: &str = "atelier-scn-app";

/// The test app: waits for `db` by name like `converge.sh` does, then prints
/// converge's own success (or FATAL) lines and serves `/user/login`, as the
/// real entrypoint starts the web server after a failed converge too.
const APP_DOCKERFILE: &str = r#"FROM busybox:1.37
ARG MARK=a
ENV SCN_MARK=$MARK
RUN mkdir -p /www/user /opt/drupal/web/sites/default/files /opt/drupal/private \
 && echo "login $MARK" > /www/user/login
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
"#;

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
  echo "[converge] converge OK — scenario app $SCN_MARK"
  echo "[entrypoint] converge succeeded"
fi
httpd -f -p 80 -h /www &
pid=$!
trap 'kill $pid; exit 0' TERM INT
wait $pid
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

/// Docker is here and answering; otherwise the scenario can't run at all.
pub fn require_docker() {
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
        };
        std::fs::write(self.stack.compose_path(), with_scn_db(compose)).expect("compose.yaml");
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
            if tag.starts_with(APP_REPO) {
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
        let _ = std::fs::remove_dir_all(home);
    }
}
