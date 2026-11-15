//! Update refuses while the site isn't ready (M2c, `studies/manager-prior-art.md`
//! verdict 2). Home Assistant's Supervisor declares the conditions its Core
//! update needs and refuses before any work
//! (<https://github.com/home-assistant/supervisor/blob/98a2322374ae3d0c248079d11f7435ad759935fa/supervisor/homeassistant/core.py#L369-L380>,
//! checked at <https://github.com/home-assistant/supervisor/blob/98a2322374ae3d0c248079d11f7435ad759935fa/supervisor/jobs/decorator.py#L418-L421>);
//! repairs are separate jobs. So here: cheap facts are gathered once at the
//! start of an update ([`gather`]), [`ready_to_update`] turns them into
//! [`Blocker`]s, and any blocker refuses the update ([`UpdateBlocked`]) before
//! anything is downloaded, backed up or changed. Each blocker says its own way
//! forward in the app (DECISIONS 0528 rule 3). The update never repairs one
//! mid-way: repairing is Start's, Restore's, Free up space's job.
//!
//! A stopped site is not a blocker: the update starts it for its backup.

use std::collections::BTreeSet;

use crate::boot::{self, Detached};
use crate::disk::{self, Free, Level};
use crate::docker::Preflight;
use crate::stack::Stack;

/// What an update looks at before it starts. Gathered once ([`gather`]); a
/// fact that couldn't be read is left at "fine" (`false`, empty, `None`), so an
/// unanswered probe never refuses an update on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateFacts {
    /// `docker info` answered.
    pub docker_answering: bool,
    /// The engine's word for the site (`locked`, `resting`, …), if it has one.
    pub engine_state: Option<String>,
    /// The site was backed up before, yet its database volume is gone
    /// ([`crate::ops::DatabaseErased`]).
    pub database_erased: bool,
    /// Containers that lost a network they declare ([`boot::detached_now`]).
    pub detached: Vec<Detached>,
    /// Services Docker stopped for lack of memory now: `State.OOMKilled`, or an
    /// `oom` event of their current container in the last [`OOM_RECENT`]
    /// (a restart policy's start clears the flag within a second).
    pub out_of_memory: BTreeSet<String>,
    /// Services Docker is restarting now (`State.Restarting`): crash-looping.
    pub restarting: BTreeSet<String>,
    /// Where to raise Docker's memory, read only when something ran out.
    pub memory_remedy: Option<String>,
    /// This computer's disk at the backups folder.
    pub host_disk: Option<Free>,
    /// Docker's disk, read in a running container; `None` on a stopped site.
    pub docker_disk: Option<Free>,
}

/// How far back an `oom` event of a current container still counts as now.
pub const OOM_RECENT: &str = "2m";

/// One reason the site isn't ready to update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// Docker isn't answering.
    DockerNotAnswering,
    /// Less than [`disk::BLOCK_BELOW`] free on this computer or on Docker's disk.
    LowDisk(Free),
    /// The database volume is gone; a restore is the way back.
    DatabaseErased,
    /// Part of the site lost its network; Start reconnects it.
    Detached(Vec<Detached>),
    /// Part of the site ran out of memory; `remedy` says where to raise it.
    OutOfMemory {
        services: BTreeSet<String>,
        remedy: String,
    },
    /// Part of the site keeps restarting, with no sign of memory trouble.
    CrashLooping(BTreeSet<String>),
    /// The engine has the site locked.
    EngineLocked,
}

/// The step in the app that clears a [`Blocker`]: the window's button for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WayForward {
    /// Restart Docker (the existing Restart button).
    RestartDocker,
    /// System → Disk space → Free up space.
    FreeUpSpace,
    /// The Backups tab (Restore).
    Restore,
    /// Start the site (it reconnects a detached container).
    Start,
    /// Run a checkup.
    Checkup,
    /// Unlock the site (Experimental features).
    Unlock,
}

impl WayForward {
    /// The window's action for it (`data-action` in `app/ui/main.js`).
    pub fn action(self) -> &'static str {
        match self {
            WayForward::RestartDocker => "restart-docker",
            WayForward::FreeUpSpace => "free-space-open",
            WayForward::Restore => "backups-open",
            WayForward::Start => "startstop-from-publish",
            WayForward::Checkup => "checkup-now",
            WayForward::Unlock => "engine-unlock",
        }
    }
}

const SAFE: &str = "Your site's data is safe.";

impl Blocker {
    /// What is wrong and what to do, in plain words, CLI commands included.
    pub fn message(&self) -> String {
        match self {
            Blocker::DockerNotAnswering => {
                "Docker isn't answering. Restart Docker (`atelier docker restart`), then \
                 update again."
                    .into()
            }
            Blocker::LowDisk(free) => free.remedy(true),
            Blocker::DatabaseErased => "Your site's database was removed from Docker, so there \
                 is nothing to update. Restore a backup first (Backups tab, or `atelier data \
                 restore <file>`)."
                .into(),
            Blocker::Detached(lost) => format!(
                "Part of your site has lost its connection to the site's network ({}). Start \
                 your site (`atelier app start`): Atelier reconnects it. Then update again. \
                 {SAFE}",
                lost.iter()
                    .map(|d| format!("{} has no connection to {}", d.container, d.network))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Blocker::OutOfMemory { services, remedy } => format!(
                "{} Update once it runs again.",
                boot::out_of_memory(services, remedy)
            ),
            Blocker::CrashLooping(services) => {
                format!("{} Update once it runs again.", boot::crash_loop(services))
            }
            Blocker::EngineLocked => crate::ops::SITE_LOCKED.to_string(),
        }
    }

    /// The app's step for it; `None` when the step is outside the app (Docker's
    /// memory setting).
    pub fn way_forward(&self) -> Option<WayForward> {
        match self {
            Blocker::DockerNotAnswering => Some(WayForward::RestartDocker),
            Blocker::LowDisk(_) => Some(WayForward::FreeUpSpace),
            Blocker::DatabaseErased => Some(WayForward::Restore),
            Blocker::Detached(_) => Some(WayForward::Start),
            Blocker::OutOfMemory { .. } => None,
            Blocker::CrashLooping(_) => Some(WayForward::Checkup),
            Blocker::EngineLocked => Some(WayForward::Unlock),
        }
    }
}

/// The blockers `facts` add up to, in the order the owner should clear them
/// (Docker first: nothing else can be read without it). Empty: ready.
/// Whether a container's own `OOMKilled` flag blocks an update. Only while it
/// is restarting: a container whose child was killed keeps the flag until its
/// next start while it runs fine, so the flag alone would block every update
/// until the owner restarted the site ("Update once it runs again" while it
/// already runs). A kill in the last [`OOM_RECENT`] still blocks through its
/// `oom` event.
fn oom_blocks(oom_killed: bool, restarting: bool) -> bool {
    oom_killed && restarting
}

pub fn ready_to_update(facts: &UpdateFacts) -> Vec<Blocker> {
    let mut out = Vec::new();
    if !facts.docker_answering {
        out.push(Blocker::DockerNotAnswering);
    }
    for free in [&facts.host_disk, &facts.docker_disk].into_iter().flatten() {
        if free.level() == Level::TooLow {
            out.push(Blocker::LowDisk(free.clone()));
        }
    }
    if !facts.docker_answering {
        // The rest is Docker's to say.
        return out;
    }
    if facts.engine_state.as_deref() == Some("locked") {
        out.push(Blocker::EngineLocked);
    }
    if facts.database_erased {
        out.push(Blocker::DatabaseErased);
    }
    if !facts.detached.is_empty() {
        out.push(Blocker::Detached(facts.detached.clone()));
    }
    if !facts.out_of_memory.is_empty() {
        out.push(Blocker::OutOfMemory {
            services: facts.out_of_memory.clone(),
            remedy: facts
                .memory_remedy
                .clone()
                .unwrap_or_else(|| "Give Docker more memory in its settings".into()),
        });
    }
    let looping: BTreeSet<String> = facts
        .restarting
        .difference(&facts.out_of_memory)
        .cloned()
        .collect();
    if !looping.is_empty() {
        out.push(Blocker::CrashLooping(looping));
    }
    out
}

/// Read [`UpdateFacts`] for `stack` (`pf`: the update's own Docker check). A
/// few `docker` calls, no change; on a stopped site Docker's disk is unknown.
pub fn gather(stack: &Stack, pf: &Preflight) -> UpdateFacts {
    let host_disk = disk::host_free(stack);
    if !pf.docker_running {
        return UpdateFacts {
            host_disk,
            ..UpdateFacts::default()
        };
    }
    let (mut out_of_memory, mut restarting) = memory_and_restarts(stack);
    // Docker can report a memory kill's exit (`Restarting`) a moment before
    // its `oom` (the flag and the event): a crash-looping service with no
    // memory proof yet is looked at again before it is called a crash loop.
    for _ in 0..UNPROVEN_LOOKS {
        if restarting.is_subset(&out_of_memory) {
            break;
        }
        std::thread::sleep(UNPROVEN_PAUSE);
        let (oom, again) = memory_and_restarts(stack);
        out_of_memory.extend(oom);
        restarting.extend(again);
    }
    let memory_remedy = (!out_of_memory.is_empty()).then(crate::docker_runtime::memory_remedy_here);
    UpdateFacts {
        docker_answering: true,
        engine_state: crate::ops::engine_state_of(stack),
        database_erased: crate::ops::stack_database_erased(stack),
        detached: boot::detached_now(stack).unwrap_or_default(),
        out_of_memory,
        restarting,
        memory_remedy,
        host_disk,
        docker_disk: disk::docker_free(stack),
    }
}

/// Extra looks at a crash-looping service with no memory proof yet, and the
/// pause before each.
const UNPROVEN_LOOKS: u32 = 3;
const UNPROVEN_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// One look: services out of memory now (flag while restarting, or a recent
/// `oom` event of their current container), and services restarting now.
fn memory_and_restarts(stack: &Stack) -> (BTreeSet<String>, BTreeSet<String>) {
    let now = boot::services_now(stack);
    let mut out_of_memory: BTreeSet<String> = now
        .iter()
        .filter(|s| oom_blocks(s.oom_killed, s.restarting))
        .map(|s| s.service.clone())
        .collect();
    if !now.is_empty() {
        out_of_memory.extend(boot::events_since(stack, OOM_RECENT, Some(&now)).oom);
    }
    let restarting = now
        .iter()
        .filter(|s| s.restarting)
        .map(|s| s.service.clone())
        .collect();
    (out_of_memory, restarting)
}

/// An update was refused before any work because the site isn't ready
/// ([`ready_to_update`]). Nothing was downloaded, backed up or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateBlocked {
    pub blockers: Vec<Blocker>,
}

impl UpdateBlocked {
    /// What `atelier` exits with for it (after 3 and 4, [`crate::ops::Overridable`]),
    /// so a Manager driving a server's CLI tells it apart without its words.
    pub const EXIT_CODE: i32 = 5;

    /// Whether `e` is this refusal: the typed error here, or a server's
    /// `atelier` that ended with [`Self::EXIT_CODE`].
    pub fn is(e: &anyhow::Error) -> bool {
        e.is::<UpdateBlocked>()
            || e.downcast_ref::<crate::remote::RemoteExit>()
                .is_some_and(|x| x.code == Self::EXIT_CODE)
    }
}

/// The first line of the refusal.
pub const NOT_READY: &str =
    "Your site isn't ready to update, so nothing was downloaded, backed up or changed.";

impl std::fmt::Display for UpdateBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(NOT_READY)?;
        for b in &self.blockers {
            write!(f, "\n- {}", b.message())?;
        }
        Ok(())
    }
}

impl std::error::Error for UpdateBlocked {}

/// Refuse with [`UpdateBlocked`] when `facts` hold any blocker.
pub fn require_ready(facts: &UpdateFacts) -> anyhow::Result<()> {
    let blockers = ready_to_update(facts);
    if blockers.is_empty() {
        Ok(())
    } else {
        Err(UpdateBlocked { blockers }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_oom_flag_on_a_running_container_does_not_block() {
        assert!(oom_blocks(true, true), "OOM-killed and restarting blocks");
        assert!(
            !oom_blocks(true, false),
            "a child killed long ago, container running fine: no dead end"
        );
        assert!(
            !oom_blocks(false, true),
            "restarting without OOM is a crash loop, not memory"
        );
    }
    use crate::disk::{Place, BLOCK_BELOW, WARN_BELOW};

    fn healthy() -> UpdateFacts {
        UpdateFacts {
            docker_answering: true,
            host_disk: Some(Free {
                place: Place::Host("/home/me/.atelier/backups".into()),
                bytes: 40 * BLOCK_BELOW,
            }),
            docker_disk: Some(Free {
                place: Place::Docker,
                bytes: 40 * BLOCK_BELOW,
            }),
            ..UpdateFacts::default()
        }
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_healthy_site_is_ready() {
        assert!(ready_to_update(&healthy()).is_empty());
        assert!(require_ready(&healthy()).is_ok());
    }

    #[test]
    fn a_stopped_site_is_not_a_blocker() {
        // Stopped: no container runs, so Docker's disk is unknown and nothing
        // is restarting or out of memory. A resting engine site the same.
        let stopped = UpdateFacts {
            docker_disk: None,
            engine_state: Some("resting".into()),
            ..healthy()
        };
        assert!(ready_to_update(&stopped).is_empty());
    }

    #[test]
    fn docker_not_answering_blocks_and_hides_what_only_docker_can_say() {
        let facts = UpdateFacts {
            docker_answering: false,
            // Never read without Docker; set here to show they are ignored.
            database_erased: true,
            engine_state: Some("locked".into()),
            ..healthy()
        };
        let b = ready_to_update(&facts);
        assert_eq!(b, vec![Blocker::DockerNotAnswering]);
        assert_eq!(b[0].way_forward(), Some(WayForward::RestartDocker));
        assert!(b[0].message().contains("`atelier docker restart`"));
    }

    #[test]
    fn low_disk_blocks_in_either_place_but_only_below_the_block_floor() {
        let mut facts = healthy();
        facts.host_disk.as_mut().unwrap().bytes = BLOCK_BELOW - 1;
        let b = ready_to_update(&facts);
        assert!(matches!(
            &b[..],
            [Blocker::LowDisk(Free {
                place: Place::Host(_),
                ..
            })]
        ));
        assert_eq!(b[0].way_forward(), Some(WayForward::FreeUpSpace));
        assert!(
            b[0].message().starts_with("This computer has"),
            "{}",
            b[0].message()
        );

        let mut facts = healthy();
        facts.docker_disk.as_mut().unwrap().bytes = BLOCK_BELOW / 4;
        let b = ready_to_update(&facts);
        assert!(matches!(
            &b[..],
            [Blocker::LowDisk(Free {
                place: Place::Docker,
                ..
            })]
        ));
        assert!(
            b[0].message().starts_with("Docker has 0.5 GB"),
            "{}",
            b[0].message()
        );

        // Low (below the warning floor) is the checkup's to say, not a refusal.
        let mut facts = healthy();
        facts.docker_disk.as_mut().unwrap().bytes = WARN_BELOW - 1;
        facts.host_disk.as_mut().unwrap().bytes = BLOCK_BELOW;
        assert!(ready_to_update(&facts).is_empty());

        // Docker gone still says the computer's disk is full.
        let mut facts = healthy();
        facts.docker_answering = false;
        facts.host_disk.as_mut().unwrap().bytes = 0;
        assert_eq!(ready_to_update(&facts).len(), 2);
    }

    #[test]
    fn an_erased_database_blocks_with_restore() {
        let b = ready_to_update(&UpdateFacts {
            database_erased: true,
            ..healthy()
        });
        assert_eq!(b, vec![Blocker::DatabaseErased]);
        assert_eq!(b[0].way_forward(), Some(WayForward::Restore));
        assert!(b[0].message().contains("`atelier data restore <file>`"));
    }

    #[test]
    fn a_detached_container_blocks_with_start_and_is_named() {
        let lost = Detached {
            network: "atelier_default".into(),
            container: "atelier-db-1".into(),
            service: "db".into(),
            running: false,
        };
        let b = ready_to_update(&UpdateFacts {
            detached: vec![lost.clone()],
            ..healthy()
        });
        assert_eq!(b, vec![Blocker::Detached(vec![lost])]);
        assert_eq!(b[0].way_forward(), Some(WayForward::Start));
        let said = b[0].message();
        assert!(
            said.contains("atelier-db-1 has no connection to atelier_default")
                && said.contains("`atelier app start`"),
            "{said}"
        );
    }

    #[test]
    fn out_of_memory_blocks_with_the_memory_text_and_no_button() {
        let b = ready_to_update(&UpdateFacts {
            out_of_memory: set(&["app"]),
            restarting: set(&["app"]),
            memory_remedy: Some("Give Docker more memory in OrbStack's settings".into()),
            ..healthy()
        });
        // Restarting because of memory is one blocker, not two.
        assert_eq!(b.len(), 1, "{b:?}");
        let said = b[0].message();
        assert!(
            said.starts_with(
                "Your site ran out of memory, so Docker stopped part of it (the app). Give \
                 Docker more memory in OrbStack's settings, or stop other sites"
            ),
            "{said}"
        );
        assert_eq!(b[0].way_forward(), None);
    }

    #[test]
    fn a_crash_loop_without_memory_trouble_blocks_with_a_checkup() {
        let b = ready_to_update(&UpdateFacts {
            restarting: set(&["db"]),
            ..healthy()
        });
        assert_eq!(b, vec![Blocker::CrashLooping(set(&["db"]))]);
        assert_eq!(b[0].way_forward(), Some(WayForward::Checkup));
        assert!(b[0].message().contains("keeps stopping and starting again"));
    }

    #[test]
    fn a_locked_engine_site_blocks_with_unlock() {
        let b = ready_to_update(&UpdateFacts {
            engine_state: Some("locked".into()),
            ..healthy()
        });
        assert_eq!(b, vec![Blocker::EngineLocked]);
        assert_eq!(b[0].way_forward(), Some(WayForward::Unlock));
        assert_eq!(b[0].message(), crate::ops::SITE_LOCKED);
    }

    #[test]
    fn the_refusal_lists_every_blocker_and_is_told_apart_by_type_or_exit_code() {
        let e = require_ready(&UpdateFacts {
            database_erased: true,
            engine_state: Some("locked".into()),
            ..healthy()
        })
        .unwrap_err();
        assert!(UpdateBlocked::is(&e));
        let said = e.to_string();
        assert!(said.starts_with(NOT_READY), "{said}");
        assert_eq!(said.lines().count(), 3, "{said}");
        let remote = anyhow::Error::new(crate::remote::RemoteExit {
            host: "me@server".into(),
            what: "`atelier app update`".into(),
            code: UpdateBlocked::EXIT_CODE,
        });
        assert!(UpdateBlocked::is(&remote));
        assert!(!UpdateBlocked::is(&anyhow::anyhow!("anything else")));
        // Not one of the overridable refusals' codes.
        assert!(![3, 4].contains(&UpdateBlocked::EXIT_CODE));
    }
}
