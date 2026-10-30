//! The audit log (floor 7): one JSON line per pairing, request, approval,
//! revoke and verb, in `engine-state`, which `app` never mounts. No API verb
//! rewrites or truncates it; it rotates past [`AUDIT_MAX`] and keeps
//! [`AUDIT_GENERATIONS`] older files (ten 1 MB files in all). An entry's
//! outcome is cut at [`AUDIT_ENTRY_MAX`] bytes, since some carry text an
//! attacker wrote (drush's errors).

use crate::codes::now;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

pub const AUDIT_MAX: u64 = 1024 * 1024;
pub const AUDIT_GENERATIONS: u32 = 9;
pub const AUDIT_ENTRY_MAX: usize = 1024;
/// How much of the log's end [`Audit::tail`] reads.
const TAIL_BYTES: u64 = 256 * 1024;

pub struct Audit {
    dir: PathBuf,
    /// One writer at a time in this process, so a rotation and an append
    /// can't interleave.
    write: Mutex<()>,
}

impl Audit {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            write: Mutex::new(()),
        }
    }

    fn log(&self) -> PathBuf {
        self.dir.join("audit.log")
    }

    fn generation(&self, n: u32) -> PathBuf {
        self.dir.join(format!("audit.log.{n}"))
    }

    /// Append one entry. Never fails the caller: a log that can't be written
    /// is reported on stderr.
    pub fn write(&self, actor: &str, ip: &str, verb: &str, outcome: &str) {
        let outcome = match outcome.char_indices().nth(AUDIT_ENTRY_MAX) {
            Some((cut, _)) => format!("{}… (cut)", &outcome[..cut]),
            None => outcome.to_string(),
        };
        let mut line = serde_json::json!({
            "at": now(), "actor": actor, "ip": ip, "verb": verb, "outcome": outcome,
        })
        .to_string();
        line.push('\n');
        let _one = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let log = self.log();
        if fs::symlink_metadata(&log).is_ok_and(|m| m.is_file() && m.len() > AUDIT_MAX) {
            for n in (1..AUDIT_GENERATIONS).rev() {
                let _ = fs::rename(self.generation(n), self.generation(n + 1));
            }
            let _ = fs::rename(&log, self.generation(1));
        }
        // One write call for the whole line: O_APPEND keeps it in one piece
        // even with an admin command appending at the same time.
        let r = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&log)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = r {
            eprintln!("engine: audit write failed: {e}");
        }
    }

    /// The newest `n` entries, reading only the end of the log.
    pub fn tail(&self, n: usize) -> Vec<serde_json::Value> {
        let mut text = String::new();
        if let Ok(mut f) = File::open(self.log()) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
            let mut bytes = Vec::new();
            let _ = f.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..]
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::TempDir;

    #[test]
    fn long_outcomes_are_cut() {
        let d = TempDir::new();
        let a = Audit::at(&d.0);
        a.write("x", "-", "v", &"é".repeat(5000));
        let e = &a.tail(1)[0];
        let o = e["outcome"].as_str().unwrap();
        assert!(o.ends_with("… (cut)") && o.chars().count() < AUDIT_ENTRY_MAX + 10);
    }

    #[test]
    fn retention_keeps_ten_files_and_drops_the_oldest() {
        let d = TempDir::new();
        let a = Audit::at(&d.0);
        for round in 0..AUDIT_GENERATIONS + 3 {
            fs::write(a.log(), vec![b'\n'; AUDIT_MAX as usize + 1]).unwrap();
            a.write("x", "-", "v", &format!("round {round}"));
        }
        let files = fs::read_dir(&d.0)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("audit.log")
            })
            .count();
        assert_eq!(files, AUDIT_GENERATIONS as usize + 1);
        assert!(!a.generation(AUDIT_GENERATIONS + 1).exists());
        assert_eq!(a.tail(5).len(), 1, "the live log holds the newest entry");
        assert!(a.tail(1)[0]["outcome"]
            .as_str()
            .unwrap()
            .contains(&format!("round {}", AUDIT_GENERATIONS + 2)));
    }

    #[test]
    fn a_planted_symlink_is_not_written_through() {
        let d = TempDir::new();
        let victim = d.0.join("victim");
        fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, d.0.join("audit.log")).unwrap();
        Audit::at(&d.0).write("x", "-", "v", "o");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    }

    #[test]
    fn tail_reads_only_the_newest() {
        let d = TempDir::new();
        let a = Audit::at(&d.0);
        for i in 0..300 {
            a.write("x", "-", "v", &i.to_string());
        }
        let t = a.tail(200);
        assert_eq!(t.len(), 200);
        assert_eq!(t[199]["outcome"], "299");
    }
}
