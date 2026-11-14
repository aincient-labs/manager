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

mod boot_fatal;
mod cleanup_needed;
mod detached_db;
mod erased_db;
mod prune_backups;
mod restore_safety;
mod settings_applied;
mod shared_tag;
mod stale_endpoint;
mod update_stopped;
