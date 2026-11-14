//! Free disk space in the two places a site writes (M2c, `studies/manager-prior-art.md`
//! § Disk): this computer's disk where the site's backups are saved, and Docker's
//! own disk (on a Mac or Windows a virtual machine's disk, which a roomy computer
//! disk says nothing about). The update gate ([`crate::gate`]) refuses below
//! [`BLOCK_BELOW`] in either place; the checkup warns below [`WARN_BELOW`].

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::docker::{self, compose};
use crate::stack::Stack;

const GIB: u64 = 1024 * 1024 * 1024;

/// Below this an update is refused. Home Assistant's Supervisor blocks its Core
/// update and every backup below 2 GiB free on its data dir:
/// <https://github.com/home-assistant/supervisor/blob/98a2322374ae3d0c248079d11f7435ad759935fa/supervisor/resolution/const.py#L18>,
/// checked at
/// <https://github.com/home-assistant/supervisor/blob/98a2322374ae3d0c248079d11f7435ad759935fa/supervisor/jobs/decorator.py#L513-L523>.
pub const BLOCK_BELOW: u64 = 2 * GIB;

/// Below this the checkup warns. DDEV warns (never blocks) below 5 GiB measured
/// inside Docker:
/// <https://github.com/ddev/ddev/blob/5a1e8cc04dcb6cf9d025c9f5a4c19bfcf537b88c/pkg/dockerutil/requirements.go#L64-L80>.
pub const WARN_BELOW: u64 = 5 * GIB;

/// Where the space was measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// This computer's disk, at the site's backups folder (or its nearest
    /// existing parent).
    Host(PathBuf),
    /// Docker's disk, read inside a running container of the site.
    Docker,
}

/// Free space found in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Free {
    pub place: Place,
    pub bytes: u64,
}

/// How [`Free::bytes`] compares to the floors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Below [`WARN_BELOW`].
    Low,
    /// Below [`BLOCK_BELOW`].
    TooLow,
}

pub fn level(bytes: u64) -> Level {
    if bytes < BLOCK_BELOW {
        Level::TooLow
    } else if bytes < WARN_BELOW {
        Level::Low
    } else {
        Level::Ok
    }
}

/// `bytes` the way the checkup has always said it ("41.0 GB").
pub fn human(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GIB as f64)
}

impl Free {
    pub fn level(&self) -> Level {
        level(self.bytes)
    }

    /// What it is short of, and the way forward. `for_update`: said as the
    /// reason an update was refused; otherwise as the checkup's remedy.
    pub fn remedy(&self, for_update: bool) -> String {
        let floor = if self.level() == Level::TooLow {
            BLOCK_BELOW
        } else {
            WARN_BELOW
        };
        let need = if for_update {
            format!(
                "an update needs at least {} for its download and the backup it takes first",
                human(floor)
            )
        } else if floor == BLOCK_BELOW {
            format!("updates are refused below {}", human(floor))
        } else {
            format!(
                "an update needs room for its download and a backup; below {} it is refused",
                human(BLOCK_BELOW)
            )
        };
        let after = if for_update {
            "then update again"
        } else {
            "then run the checkup again"
        };
        match &self.place {
            Place::Host(dir) => format!(
                "This computer has {} free where your site's backups are saved ({}); {need}. \
                 Delete files you don't need on this computer, or move older backups \
                 elsewhere, {after}.",
                human(self.bytes),
                dir.display()
            ),
            Place::Docker => format!(
                "Docker has {} of disk space free; {need}. Free up space (System → Disk space \
                 in the app, or `atelier app cleanup`), or give Docker more disk space in its \
                 settings, {after}.",
                human(self.bytes)
            ),
        }
    }
}

/// Free space on this computer's disk where `stack`'s backups are saved. `None`
/// when it can't be read (not a Unix, `df` failed).
pub fn host_free(stack: &Stack) -> Option<Free> {
    let dir = stack.backups_dir();
    let probed = nearest_existing(&dir)?;
    let kb = df_available_kb(&probed)?;
    Some(Free {
        place: Place::Host(probed),
        bytes: kb.saturating_mul(1024),
    })
}

/// The path, or its nearest parent that exists (a site's backups folder is made
/// by its first backup).
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    Some(p.to_path_buf())
}

#[cfg(unix)]
fn df_available_kb(path: &Path) -> Option<u64> {
    let out = Command::new("df").arg("-Pk").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df_available_kb(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(unix))]
fn df_available_kb(_path: &Path) -> Option<u64> {
    None
}

/// Where inside a running container Docker's free space is read: the
/// database's own data dir (a volume, on Docker's disk, where a migration
/// writes) when `db` runs, else the app's root filesystem.
pub(crate) fn docker_probe_target(running: &[String]) -> Option<(&'static str, &'static str)> {
    if running.iter().any(|s| s == "db") {
        Some(("db", "/var/lib/postgresql/data"))
    } else if running.iter().any(|s| s == "app") {
        Some(("app", "/"))
    } else {
        None
    }
}

/// Free space on Docker's disk, read by `df` inside a running container of the
/// site ([`docker_probe_target`]). No container is started for it: on a
/// stopped site (or when Docker doesn't answer) it is `None`, "unknown".
pub fn docker_free(stack: &Stack) -> Option<Free> {
    let mut ps = compose(stack);
    ps.args(["ps", "--format", "json"]);
    let running: Vec<String> = crate::ops::parse_ps(&docker::try_capture(ps)?)
        .into_iter()
        .filter(|e| e.state.to_lowercase().contains("running"))
        .map(|e| e.service)
        .collect();
    let (service, path) = docker_probe_target(&running)?;
    let mut c = compose(stack);
    c.args(["exec", "-T", service, "df", "-Pk", path]);
    let kb = parse_df_available_kb(&docker::try_capture(c)?)?;
    Some(Free {
        place: Place::Docker,
        bytes: kb.saturating_mul(1024),
    })
}

/// The "Available" column (4th) of `df -Pk`'s data row, in KB. POSIX output mode
/// guarantees one row per filesystem on a single line, so no continuation
/// handling is needed. Busybox (the containers) prints the same columns.
pub(crate) fn parse_df_available_kb(out: &str) -> Option<u64> {
    let row = out.lines().nth(1)?;
    row.split_whitespace().nth(3)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_floors_are_has_and_ddevs() {
        assert_eq!(level(BLOCK_BELOW - 1), Level::TooLow);
        assert_eq!(level(BLOCK_BELOW), Level::Low);
        assert_eq!(level(WARN_BELOW - 1), Level::Low);
        assert_eq!(level(WARN_BELOW), Level::Ok);
        assert_eq!(level(0), Level::TooLow);
    }

    #[test]
    fn df_available_column_is_the_fourth() {
        // Verbatim `df -Pk /` on macOS 15.
        let out = "Filesystem 1024-blocks      Used Available Capacity  Mounted on\n\
                   /dev/disk3s1s1 971350180 10485760 123456789    8%    /\n";
        assert_eq!(parse_df_available_kb(out), Some(123_456_789));
        // Busybox `df -Pk` inside a container on OrbStack, 2026-11-14.
        let busybox = "Filesystem           1024-blocks    Used Available Capacity Mounted on\n\
                       /dev/vdb1             68747264  59939896   8807368  87% /data\n";
        assert_eq!(parse_df_available_kb(busybox), Some(8_807_368));
    }

    #[test]
    fn df_garbage_is_not_a_number() {
        assert_eq!(parse_df_available_kb(""), None);
        assert_eq!(parse_df_available_kb("Filesystem 1024-blocks\n"), None);
    }

    #[test]
    fn docker_is_read_in_the_database_first_then_the_app_and_never_started() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            docker_probe_target(&s(&["app", "db", "edge"])),
            Some(("db", "/var/lib/postgresql/data"))
        );
        assert_eq!(
            docker_probe_target(&s(&["app", "edge"])),
            Some(("app", "/"))
        );
        assert_eq!(docker_probe_target(&s(&["edge"])), None);
        assert_eq!(docker_probe_target(&[]), None);
    }

    #[test]
    fn the_two_places_name_their_own_disk_and_way_forward() {
        let host = Free {
            place: Place::Host(PathBuf::from("/Users/me/.atelier/backups")),
            bytes: GIB,
        };
        let said = host.remedy(true);
        assert!(
            said.starts_with(
                "This computer has 1.0 GB free where your site's backups are saved \
                 (/Users/me/.atelier/backups); an update needs at least 2.0 GB"
            ),
            "{said}"
        );
        assert!(!said.contains("Docker"), "{said}");
        let docker = Free {
            place: Place::Docker,
            bytes: GIB / 2,
        };
        let said = docker.remedy(true);
        assert!(
            said.starts_with("Docker has 0.5 GB of disk space free"),
            "{said}"
        );
        assert!(
            said.contains("Free up space (System → Disk space"),
            "{said}"
        );
        assert!(said.ends_with("then update again."), "{said}");
        // The checkup's warning names the refusal to come.
        let low = Free {
            place: Place::Docker,
            bytes: 3 * GIB,
        };
        assert_eq!(low.level(), Level::Low);
        assert!(low.remedy(false).contains("below 2.0 GB it is refused"));
    }
}
