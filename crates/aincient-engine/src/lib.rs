//! The Atelier engine sidecar: a closed HTTP API in front of the host's
//! Docker socket, which makes it **root on the host**. EXPERIMENTAL and
//! opt-in only (DECISIONS 0483). Built fresh from the `engine-poc` proof of
//! concept with tests (plan m3-1, m3-2).
#![cfg(unix)]

pub mod audit;
pub mod codes;
pub mod inbox;
pub mod state;
