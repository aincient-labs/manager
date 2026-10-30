//! The file protocol between Drupal and the engine, in two volumes:
//!
//! - `engine-inbox`: Drupal writes, the engine reads. `app` mounts it
//!   read-write, so every file in it is attacker-controlled once Drupal is
//!   compromised (design: threat "Drupal RCE").
//! - `engine-outbox`: the engine writes, `app` mounts it read-only. What the
//!   console shows an admin (the pairings list) comes from here, so an
//!   attacker in Drupal can't hide its own pairing from the list (floor 13).
//!
//! | file              | box | written by | meaning                            |
//! |-------------------|-----|------------|------------------------------------|
//! | `revoke.request`  | in  | console    | `{"id": "…"}`: revoke that pairing |
//! | `pairings.json`   | out | engine     | the pairings list, without hashes  |
//!
//! The inbox carries only `revoke.request`: nothing here pairs (E-02, floor
//! 10). Reads refuse anything but a small regular file (no symlink, no FIFO,
//! no directory) and parse only the field above; whatever was there is
//! removed, so a malformed request can't wedge the protocol.

use crate::state::write_atomic;
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The most the engine reads from an inbox file.
pub const MAX_REQUEST: u64 = 4096;
/// The one request file the inbox carries.
pub const REVOKE_REQUEST: &str = "revoke.request";
/// The uid `www-data` has in the app image, so Drupal can write requests.
const APP_UID: u32 = 33;

pub struct Inbox {
    pub dir: PathBuf,
    pub out: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRequest {
    pub id: String,
}

impl Inbox {
    pub fn at(dir: impl Into<PathBuf>, out: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            out: out.into(),
        }
    }

    /// The inbox is owned by `www-data` with no access for anyone else in the
    /// container; the outbox is root's, readable by all.
    pub fn prepare(&self) {
        let _ = fs::create_dir_all(&self.out);
        let _ = fs::set_permissions(&self.out, fs::Permissions::from_mode(0o755));
        let _ = fs::create_dir_all(&self.dir);
        if let Ok(c) = std::ffi::CString::new(self.dir.as_os_str().as_encoded_bytes()) {
            // SAFETY: a valid NUL-terminated path; chown has no memory effects.
            unsafe { libc::chown(c.as_ptr(), APP_UID, APP_UID) };
        }
        let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o770));
    }

    /// Take the revoke request, if there is one: read it safely, then remove
    /// it whatever it held.
    pub fn take_revoke(&self) -> Option<Result<RevokeRequest, String>> {
        let path = self.dir.join(REVOKE_REQUEST);
        let meta = fs::symlink_metadata(&path).ok()?;
        let r = read_small_regular(&path).and_then(|bytes| {
            serde_json::from_slice::<RevokeRequest>(&bytes)
                .map_err(|e| format!("not a {REVOKE_REQUEST}: {e}"))
        });
        // A planted directory would otherwise sit there for good and block
        // the request it is named after. `remove_dir_all` doesn't follow links.
        let _ = if meta.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        Some(r)
    }

    /// Write to the outbox, which only the engine can write.
    pub fn publish(&self, name: &str, bytes: &[u8]) {
        if let Err(e) = write_atomic(&self.out, name, bytes, 0o644) {
            eprintln!("engine: outbox write {name} failed: {e:#}");
        }
    }
}

/// Open without following a link and without blocking on a FIFO, then check
/// what was actually opened: a regular file of at most [`MAX_REQUEST`] bytes.
fn read_small_regular(path: &Path) -> Result<Vec<u8>, String> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("open: {e}"))?;
    let meta = f.metadata().map_err(|e| format!("stat: {e}"))?;
    if !meta.file_type().is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_REQUEST {
        return Err("too large".into());
    }
    let mut buf = Vec::new();
    f.take(MAX_REQUEST + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    if buf.len() as u64 > MAX_REQUEST {
        return Err("too large".into());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::TempDir;

    fn inbox(d: &TempDir) -> Inbox {
        Inbox::at(&d.0, d.0.join("out"))
    }

    #[test]
    fn a_request_is_read_once_and_removed() {
        let d = TempDir::new();
        fs::write(d.0.join(REVOKE_REQUEST), r#"{"id": "abc"}"#).unwrap();
        assert_eq!(inbox(&d).take_revoke().unwrap().unwrap().id, "abc");
        assert!(inbox(&d).take_revoke().is_none());
    }

    #[test]
    fn a_planted_directory_is_cleared() {
        let d = TempDir::new();
        fs::create_dir_all(d.0.join("revoke.request/x")).unwrap();
        assert!(inbox(&d).take_revoke().unwrap().is_err());
        assert!(!d.0.join(REVOKE_REQUEST).exists());
    }

    #[test]
    fn a_symlinked_request_is_refused_and_its_target_untouched() {
        let d = TempDir::new();
        let secret = d.0.join("secret");
        fs::write(&secret, r#"{"id": "abc"}"#).unwrap();
        std::os::unix::fs::symlink(&secret, d.0.join(REVOKE_REQUEST)).unwrap();
        assert!(inbox(&d).take_revoke().unwrap().is_err());
        assert!(secret.exists(), "unlink removes the link, not the target");
    }

    #[test]
    fn a_fifo_request_does_not_block_the_engine() {
        let d = TempDir::new();
        let p = std::ffi::CString::new(d.0.join(REVOKE_REQUEST).to_str().unwrap()).unwrap();
        // SAFETY: a valid path.
        assert_eq!(unsafe { libc::mkfifo(p.as_ptr(), 0o600) }, 0);
        assert!(inbox(&d).take_revoke().unwrap().is_err());
    }

    #[test]
    fn oversized_malformed_or_extra_fields_are_dropped() {
        let d = TempDir::new();
        for body in [
            vec![b' '; 5000],
            br#"{"id": 5}"#.to_vec(),
            br#"{"id": "abc", "primary": true}"#.to_vec(),
        ] {
            fs::write(d.0.join(REVOKE_REQUEST), body).unwrap();
            assert!(inbox(&d).take_revoke().unwrap().is_err());
            assert!(!d.0.join(REVOKE_REQUEST).exists());
        }
    }

    #[test]
    fn the_old_pairing_door_is_not_read() {
        // The POC's console pairing files: nothing reads them any more.
        let d = TempDir::new();
        fs::write(d.0.join("pair.request"), r#"{"uid": 1}"#).unwrap();
        assert!(inbox(&d).take_revoke().is_none());
        assert!(d.0.join("pair.request").exists(), "ignored, not acted on");
    }
}
