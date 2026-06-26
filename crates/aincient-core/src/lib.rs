//! `aincient-core` — the lifecycle engine for an Atelier CMS appliance.
//!
//! One crate owns every operation the front-ends expose (install, update,
//! check-update, backup, restore, status, …). The [`atelier` CLI] and the Tauri
//! manager GUI both depend on this crate directly, so there is exactly one
//! implementation of the behaviour and no shelling between the two.
//!
//! Everything operates on a [`Stack`] — a `~/.atelier` directory holding the same
//! `compose.yaml` + `.env` the `docker/install.sh` bootstrapper writes.
//!
//! [`atelier` CLI]: ../atelier/index.html

pub mod docker;
pub mod ops;
pub mod stack;

pub use docker::{preflight, Preflight};
pub use ops::{
    admin_password, backup, check_update, down, install, list_backups, logs_command, open_console,
    pull, reinstall, restore, set_admin_password, start, status, stop, up, update, Backup, Status,
    UpdateCheck,
};
pub use stack::{InstallOptions, Stack, DEFAULT_IMAGE, DEFAULT_PORT};
