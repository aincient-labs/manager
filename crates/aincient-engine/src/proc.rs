//! Bounded subprocesses (floor 8). drush runs Drupal's code, which an attacker
//! in `app` may control, and compose can hang on a wedged daemon: nothing the
//! engine runs is unbounded. Every subprocess gets a deadline and an output
//! cap, runs in its own process group, and the whole group is killed on the
//! deadline or when the caller cuts it short (a lockdown). Output is read
//! without extra threads: they count against the container's process cap.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The most of a subprocess's output (each of stdout and stderr) kept; the
/// rest is read and dropped.
pub const OUTPUT_MAX: usize = 64 * 1024;
const POLL: Duration = Duration::from_millis(100);
/// `cut` is asked about once a second.
const CUT_EVERY: u32 = 10;

/// Run `cmd`, returning its stdout (at most [`OUTPUT_MAX`] bytes), or fail
/// with its stderr (also capped). On `deadline`, or when `cut` says so, the
/// process group is killed and this fails. (A child that exits on its own is
/// reaped first; its group is not signalled then, since its id may be reused.)
pub fn run_bounded(
    mut cmd: Command,
    what: &str,
    deadline: Duration,
    cut: &dyn Fn() -> bool,
) -> Result<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("start a process to {what}"))?;
    let mut out_pipe = child.stdout.take().expect("piped");
    let mut err_pipe = child.stderr.take().expect("piped");
    for fd in [out_pipe.as_raw_fd(), err_pipe.as_raw_fd()] {
        // SAFETY: valid descriptors we own; fcntl has no memory effects.
        unsafe {
            libc::fcntl(
                fd,
                libc::F_SETFL,
                libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
            )
        };
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let t0 = Instant::now();
    let mut polls = 0u32;
    let stopped = loop {
        drain(&mut out_pipe, &mut out);
        drain(&mut err_pipe, &mut err);
        if let Some(status) = child.try_wait()? {
            drain(&mut out_pipe, &mut out);
            drain(&mut err_pipe, &mut err);
            break Ok(status);
        }
        polls += 1;
        if t0.elapsed() >= deadline {
            break Err(format!("gave up after {}s", deadline.as_secs_f32()));
        }
        if polls % CUT_EVERY == 0 && cut() {
            break Err("cut short by a lockdown".to_string());
        }
        thread::sleep(POLL);
    };
    let status = match stopped {
        Ok(s) => s,
        Err(why) => {
            // Not reaped yet, so the group id is still ours: everything in it
            // goes, a backgrounded grandchild included.
            kill_group(child.id());
            let _ = child.wait();
            bail!("failed to {what}: {why}");
        }
    };
    if !status.success() {
        bail!(
            "failed to {what}:\n{}",
            String::from_utf8_lossy(&err).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn kill_group(leader: u32) {
    // SAFETY: signals the group our own child leads; no memory effects. The
    // group id is the child's pid (process_group(0)), never 0 or 1.
    if leader > 1 {
        unsafe { libc::kill(-(leader as i32), libc::SIGKILL) };
    }
}

/// Read what a non-blocking pipe has, keeping at most [`OUTPUT_MAX`] bytes.
fn drain(r: &mut impl Read, buf: &mut Vec<u8>) {
    let mut chunk = [0u8; 8192];
    while let Ok(n @ 1..) = r.read(&mut chunk) {
        let room = OUTPUT_MAX.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..n.min(room)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::TempDir;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    fn never() -> impl Fn() -> bool {
        || false
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks; no memory effects.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn output_is_returned_and_capped() {
        let out = run_bounded(sh("echo hello"), "echo", Duration::from_secs(5), &never()).unwrap();
        assert_eq!(out, "hello\n");
        let out = run_bounded(
            sh("head -c 1000000 /dev/zero | tr '\\0' a"),
            "flood",
            Duration::from_secs(20),
            &never(),
        )
        .unwrap();
        assert_eq!(out.len(), OUTPUT_MAX, "a megabyte kept as 64 KB");
    }

    #[test]
    fn a_failure_carries_capped_stderr() {
        let e = run_bounded(
            sh("head -c 1000000 /dev/zero | tr '\\0' e >&2; exit 3"),
            "fail",
            Duration::from_secs(20),
            &never(),
        )
        .unwrap_err()
        .to_string();
        assert!(e.starts_with("failed to fail:"));
        assert!(e.len() < OUTPUT_MAX + 100);
    }

    #[test]
    fn the_deadline_kills_the_whole_process_group() {
        let d = TempDir::new();
        let pidfile = d.0.join("pid");
        let script = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
        let t0 = Instant::now();
        let e = run_bounded(sh(&script), "hang", Duration::from_secs(1), &never())
            .unwrap_err()
            .to_string();
        assert!(e.contains("gave up"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let t0 = Instant::now();
        while alive(pid) && t0.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(pid), "the backgrounded grandchild was killed too");
    }

    #[test]
    fn a_cut_ends_it_early() {
        let t0 = Instant::now();
        let e = run_bounded(sh("sleep 60"), "hang", Duration::from_secs(60), &|| {
            t0.elapsed() > Duration::from_millis(300)
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("cut short"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        let e = run_bounded(
            Command::new("/nonexistent/atelier-test"),
            "run nothing",
            Duration::from_secs(1),
            &never(),
        );
        assert!(e.is_err());
    }
}
