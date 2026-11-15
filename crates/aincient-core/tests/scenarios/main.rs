//! Real-Docker scenario tests (umbrella DECISIONS 0528): every incident or
//! regression in code that changes the owner's Docker becomes one scenario,
//! reproduced on a tiny real compose project and run through the Manager's own
//! `ops` / `doctor` functions. No Docker is mocked.
//!
//! Ignored by default (they need Docker and take a minute each):
//!
//! ```text
//! cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1
//! ```
//!
//! What the harness guarantees and how to add one: `README.md` → "Scenario tests".

mod harness;

mod app_down;
mod boot_fatal;
mod cleanup_needed;
mod container_gone;
mod db_image_move;
mod detached_db;
mod disk_full;
mod doctor_start;
mod endpoint_clear;
mod erased_db;
mod older_backups;
mod oom_app;
mod oom_db;
mod prune_backups;
mod reconnect_restart;
mod restore_safety;
mod settings_applied;
mod shared_tag;
mod site_busy;
mod stale_endpoint;
mod update_blocked;
mod update_failed_logs;
mod update_local_build;
mod update_running_backup_fails;
mod update_stopped;
