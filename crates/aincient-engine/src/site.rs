//! What the engine does to the site, and nothing more: the whole reach of the
//! Docker socket as the API uses it. Every argument here is a constant; no
//! request field reaches a Docker or Compose argument (floor 2), and the
//! drush verbs are an enum, not strings.

use crate::proc::run_bounded;
use aincient_core::engine::ENGINE_COMPOSE_FILE;
use aincient_core::Stack;
use anyhow::Result;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

/// Deadlines for what a job runs (floor 8).
pub const DRUSH_DEADLINE: Duration = Duration::from_secs(15 * 60);
pub const COMPOSE_DEADLINE: Duration = Duration::from_secs(120);
pub const PS_DEADLINE: Duration = Duration::from_secs(20);
/// drush inside `app`, as `www-data`, never root (floor 12).
const DRUSH: &[&str] = &["/opt/drupal/vendor/bin/drush", "--root=/opt/drupal/web"];

/// A job may be cut short (a lockdown came in); asked about once a second.
pub type Cut<'a> = &'a dyn Fn() -> bool;

/// The two things the engine asks of Drupal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drush {
    /// `aincient:freeze --label=engine`
    Freeze,
    /// `aincient:use live`
    GoLive,
}

impl Drush {
    fn args(self) -> &'static [&'static str] {
        match self {
            Drush::Freeze => &["aincient:freeze", "--label=engine"],
            Drush::GoLive => &["aincient:use", "live"],
        }
    }
}

pub trait Site: Send + Sync {
    /// `compose ps -a` for the project: an array of `{Service, State, …}`.
    fn states(&self) -> Result<Value>;
    /// Stop `app` and `db`, only ever those two: `compose stop` without names
    /// would stop edge and the engine itself.
    fn stop_app_and_db(&self, cut: Cut) -> Result<()>;
    fn start_db_and_app(&self, cut: Cut) -> Result<()>;
    fn drush(&self, verb: Drush, cut: Cut) -> Result<()>;
    /// Whether Drupal answers through edge: any status but edge's own
    /// 502–504 (an untrusted Host's 4xx counts).
    fn answers(&self) -> bool;
}

/// The real site: `docker compose` over the mounted socket.
pub struct ComposeSite {
    pub stack: Stack,
    /// Edge on the engine network, for the readiness probe.
    pub edge: String,
}

impl ComposeSite {
    /// Both files named explicitly, so the engine never depends on `.env`'s
    /// `COMPOSE_FILE` and never loads a `compose.override.yaml` someone left
    /// in the stack directory.
    fn compose(&self) -> Command {
        let mut c = aincient_core::docker::docker();
        c.arg("compose")
            .args(["-p", &self.stack.project_name()])
            .arg("-f")
            .arg(self.stack.compose_path())
            .arg("-f")
            .arg(self.stack.home.join(ENGINE_COMPOSE_FILE))
            .current_dir(&self.stack.home);
        c
    }
}

impl Site for ComposeSite {
    fn states(&self) -> Result<Value> {
        let mut c = self.compose();
        c.args(["ps", "-a", "--format", "json"]);
        let out = run_bounded(c, "list the services", PS_DEADLINE, &|| false)?;
        // One JSON object per line (newer compose) or one array (older).
        Ok(Value::Array(
            out.lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .flat_map(|v| match v {
                    Value::Array(a) => a,
                    one => vec![one],
                })
                .collect(),
        ))
    }

    fn stop_app_and_db(&self, cut: Cut) -> Result<()> {
        let mut c = self.compose();
        c.args(["stop", "app", "db"]);
        run_bounded(c, "stop the site", COMPOSE_DEADLINE, cut).map(|_| ())
    }

    fn start_db_and_app(&self, cut: Cut) -> Result<()> {
        let mut c = self.compose();
        c.args(["start", "db", "app"]);
        run_bounded(c, "start the site", COMPOSE_DEADLINE, cut).map(|_| ())
    }

    fn drush(&self, verb: Drush, cut: Cut) -> Result<()> {
        let mut c = self.compose();
        c.args(["exec", "-T", "-u", "www-data", "app"])
            .args(DRUSH)
            .args(verb.args());
        run_bounded(
            c,
            &format!("run drush {}", verb.args()[0]),
            DRUSH_DEADLINE,
            cut,
        )
        .map(|_| ())
    }

    fn answers(&self) -> bool {
        http_status(&self.edge, "/user/login").is_some_and(|c| !matches!(c, 502..=504))
    }
}

/// The status code of a plain GET, or `None`.
fn http_status(addr: &str, path: &str) -> Option<u16> {
    use std::net::ToSocketAddrs;
    let to = addr.to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&to, Duration::from_secs(5)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut buf = [0u8; 32];
    let n = s.read(&mut buf).ok()?;
    std::str::from_utf8(&buf[..n])
        .ok()?
        .split(' ')
        .nth(1)?
        .parse()
        .ok()
}

/// `running`, `exited`, … as compose reported it, or `missing`.
pub fn service_state(states: &Value, service: &str) -> String {
    states
        .as_array()
        .and_then(|a| a.iter().find(|v| v["Service"] == service))
        .and_then(|v| v["State"].as_str().map(str::to_string))
        .unwrap_or_else(|| "missing".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drush_verbs_are_constants() {
        assert_eq!(Drush::Freeze.args(), ["aincient:freeze", "--label=engine"]);
        assert_eq!(Drush::GoLive.args(), ["aincient:use", "live"]);
    }

    #[test]
    fn compose_names_both_files_and_the_pinned_project() {
        let mut stack = Stack::at("/srv/site");
        stack.project = Some("atelier".into());
        let site = ComposeSite {
            stack,
            edge: "edge:80".into(),
        };
        let c = site.compose();
        let args: Vec<_> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "compose",
                "-p",
                "atelier",
                "-f",
                "/srv/site/compose.yaml",
                "-f",
                "/srv/site/compose.engine.yaml"
            ]
        );
    }

    #[test]
    fn missing_services_read_as_missing() {
        let v = serde_json::json!([{"Service": "app", "State": "running"}]);
        assert_eq!(service_state(&v, "app"), "running");
        assert_eq!(service_state(&v, "db"), "missing");
        assert_eq!(service_state(&Value::Null, "app"), "missing");
    }
}
