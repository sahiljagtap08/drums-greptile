//! Child-process discipline shared by every spawn site in this crate: process
//! groups (so a kill reaches grandchildren), and bounded pipe drains started
//! before anything waits (so a wait can never hang on an inherited fd, and a
//! chatty child can never grow this process without bound).
//!
//! n15 (Task-3 review round 4): `ship.rs` and `engine.rs` each carried a
//! byte-identical copy of the process-group helpers — two `libc::kill`
//! `unsafe` blocks in ONE crate, exactly the shape that drifts when only one
//! of them is fixed. Cross-crate copies (`engine-repro`, `engine-repair`) are
//! unavoidable; in-crate ones are not.
//!
//! The two rules encoded here are the ones this branch has now had to fix
//! three times, in three different functions:
//!
//! 1. **`kill_on_drop`/`start_kill` reach the DIRECT child only.** A script
//!    that backgrounds a worker (`jest` workers, `node server.js &`) leaves
//!    that grandchild running past the kill, holding ports and CPU with no
//!    record. The cure is spawning the child as its own process-group leader
//!    and signalling the whole group.
//! 2. **`wait_with_output()` returns on pipe EOF, not on child exit.** Any
//!    grandchild holding the inherited stdout/stderr keeps that future pending
//!    for the full timeout, so a command that already succeeded is reported as
//!    a timeout (`ship.rs`'s C2, `engine-repair` before it, and `engine.rs`'s
//!    F7). The cure is: start the drains FIRST, put the timeout on
//!    `child.wait()` ALONE, and give the drains only a bounded grace once the
//!    exit status is known.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Caps what a drained stream RETAINS (oldest bytes dropped first), mirroring
/// `engine_repair`'s `MAX_DRAIN_BYTES`/`BoundedBuf` discipline: output is kept
/// only to explain a failure or count lines, and a looping child must not be
/// able to turn that into unbounded memory in the watch process.
pub(crate) const MAX_DRAIN_BYTES: usize = 256 * 1024;

/// Bounds only the RESIDUAL drain, once the child's exit status is already
/// known — never the wait itself.
pub(crate) const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Make the child the leader of its own new process group, the precondition
/// for [`kill_process_group`] to be able to reach anything it backgrounds.
///
/// The deliberate tradeoff (carried minor m10): a child in its own group no
/// longer receives the terminal's ctrl-c, so the explicit group kill is what
/// replaces that and must not be omitted.
#[cfg(unix)]
pub(crate) fn set_new_process_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}
#[cfg(not(unix))]
pub(crate) fn set_new_process_group(_cmd: &mut tokio::process::Command) {}

/// SIGKILLs the whole process group `pid` leads (valid only because
/// [`set_new_process_group`] made it its own group leader) — reaches
/// grandchildren `Child::start_kill`/`kill_on_drop` cannot.
///
/// WHEN to call this is a per-caller policy decision, not a property of this
/// function: a deploy command is SUPPOSED to leave the service it just started
/// running (`ship.rs`), while nothing a test script backgrounds should outlive
/// verification (`engine.rs`). Both agree on the abandonment case — a genuine
/// timeout or wait error always group-kills, since nothing should be left
/// running unaccounted for.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    // Safety: a negative pid to `kill(2)` signals the process group, not a
    // single process; no memory is touched, only a syscall is made.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}
#[cfg(not(unix))]
pub(crate) fn kill_process_group(_pid: u32) {}

/// Reads a piped stream into `buf` as bytes arrive, capped at
/// [`MAX_DRAIN_BYTES`] (oldest bytes dropped first). Started the INSTANT the
/// child is spawned, never after — a chatty child filling the ~64KiB kernel
/// pipe buffer with nobody reading it blocks the writer in `write(2)` forever,
/// independently of the backgrounded-grandchild issue above.
pub(crate) async fn drain_into<R>(mut r: R, buf: Arc<Mutex<Vec<u8>>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 4096];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut g = buf.lock().unwrap_or_else(|p| p.into_inner());
                g.extend_from_slice(&chunk[..n]);
                if g.len() > MAX_DRAIN_BYTES {
                    let excess = g.len() - MAX_DRAIN_BYTES;
                    g.drain(..excess);
                }
            }
        }
    }
}

/// Lossy UTF-8 view of what a drain has retained so far.
pub(crate) fn take_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let g = buf.lock().unwrap_or_else(|p| p.into_inner());
    String::from_utf8_lossy(&g).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retention cap is real: a stream that writes far more than
    /// [`MAX_DRAIN_BYTES`] leaves the buffer at the cap, holding the NEWEST
    /// bytes (the ones nearest the failure).
    #[tokio::test]
    async fn a_drain_retains_at_most_the_cap_and_keeps_the_newest_bytes() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut data = vec![b'o'; MAX_DRAIN_BYTES * 2];
        data.extend_from_slice(b"LAST");
        drain_into(std::io::Cursor::new(data), buf.clone()).await;
        let text = take_text(&buf);
        assert!(
            text.len() <= MAX_DRAIN_BYTES,
            "retained {} bytes, cap is {MAX_DRAIN_BYTES}",
            text.len()
        );
        assert!(
            text.ends_with("LAST"),
            "the newest bytes must survive, not the oldest"
        );
    }
}
