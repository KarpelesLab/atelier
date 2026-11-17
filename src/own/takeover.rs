//! Taking a session over: the newcomer's side.
//!
//! 1. **What the owner has going is written down** — every process it
//!    started, directly or not, each with its start time — before anything
//!    is asked of it: once the owner is gone its children belong to nobody
//!    and can no longer be found from it.
//! 2. **The owner is asked** over its socket to yield (`session/yield`,
//!    see [`super::hub`]). It interrupts its turn, saves, stops its
//!    background work and exits, which lets go of the lock.
//! 3. **The lock is waited for.** If it is not free a little after the
//!    owner's own deadline, the owner is signalled: TERM, then KILL. Never
//!    without checking that the pid is still the process the record
//!    describes (its start time) and that the record is still the one this
//!    started from — a pid that was recycled, or a session a third atelier
//!    took meanwhile, is left alone.
//! 4. **What the owner left running is stopped**: the processes written
//!    down in step 1 that are still there and still the same, with what
//!    they started since. The coordinator and the local model's server are
//!    not the owner's to stop and are never on that list.
//!
//! The lock comes back held, for the newcomer to own the session with.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;

use super::imp::{genuine, record_of, remove_socket};
use super::{LOCK_FILE, Live, Record, YIELD_GRACE, hub, proc};

/// How long past the owner's own deadline the lock is waited for before
/// the owner is signalled, and how long each signal is given.
const PATIENCE: Duration = Duration::from_secs(2);
/// How long what the owner left behind has to end on TERM.
const SWEEP: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(25);

pub(super) enum Outcome {
    /// The session is free and `lock` holds it. `notes` say what was done.
    Done { lock: File, notes: Vec<String> },
    /// Another atelier owns the session now: not the one this was about.
    Changed,
}

/// Who is taking over, for the owner to say.
fn me() -> String {
    match proc::terminal() {
        Some(tty) => format!("pid {} on {tty}", std::process::id()),
        None => format!("pid {}", std::process::id()),
    }
}

/// The lock on `root`'s session, if it is free before `deadline`.
fn wait_lock(root: &Path, deadline: Instant) -> Option<File> {
    loop {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(LOCK_FILE))
            .ok()?;
        if file.try_lock().is_ok() {
            return Some(file);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Whether the session is still `owner`'s, by the record on disk.
fn still(root: &Path, owner: &Record) -> bool {
    record_of(root).is_none_or(|now| now.pid == owner.pid && now.started == owner.started)
}

pub(super) fn run(live: &Live, root: &Path) -> Result<Outcome> {
    let owner = &live.record;
    let left = proc::descendants(owner.pid);
    let asked = hub::ask(owner, "session/yield", json!({ "by": me() })).is_ok();

    let mut how = "it stopped when asked";
    let patience = if asked {
        YIELD_GRACE + PATIENCE
    } else {
        Duration::ZERO
    };
    let mut lock = wait_lock(root, Instant::now() + patience);
    for signal in ["TERM", "KILL"] {
        if lock.is_some() {
            break;
        }
        if !still(root, owner) {
            return Ok(Outcome::Changed);
        }
        if !genuine(owner) {
            // Gone, and the lock is not free: somebody else's, or about
            // to be free.
            lock = wait_lock(root, Instant::now() + PATIENCE);
            if lock.is_none() && !still(root, owner) {
                return Ok(Outcome::Changed);
            }
            break;
        }
        proc::signal(owner.pid, signal);
        how = match (asked, signal) {
            (true, "TERM") => "it did not stop when asked, and was terminated",
            (false, "TERM") => "it could not be asked, and was terminated",
            _ => "it had to be killed",
        };
        lock = wait_lock(root, Instant::now() + PATIENCE);
    }
    let Some(lock) = lock else {
        if !still(root, owner) {
            return Ok(Outcome::Changed);
        }
        bail!(
            "could not take the session over: {} is still locked, and pid {} is {}",
            LOCK_FILE,
            owner.pid,
            if genuine(owner) {
                "still running"
            } else {
                "not the process that locked it"
            }
        );
    };

    let swept = sweep(&left);
    remove_socket(owner);
    if how != "it stopped when asked" {
        tell_terminal(owner);
    }
    forgotten(owner.pid);

    let mut note = format!("took over the session of pid {} ({how})", owner.pid);
    if swept > 0 {
        note.push_str(&format!("; stopped {swept} process(es) it left running"));
    }
    Ok(Outcome::Done {
        lock,
        notes: vec![format!("{note}.")],
    })
}

/// Stop what the owner left: each of `left` that is still the process it
/// was, and what those started. Returns how many there were.
fn sweep(left: &[proc::Proc]) -> usize {
    let same = |p: &proc::Proc| proc::start_token(p.pid).as_deref() == Some(p.token.as_str());
    let mut targets: Vec<proc::Proc> = Vec::new();
    for p in left.iter().filter(|p| same(p)) {
        for found in std::iter::once(p.clone()).chain(proc::descendants(p.pid)) {
            if !targets.iter().any(|t| t.pid == found.pid) {
                targets.push(found);
            }
        }
    }
    if targets.is_empty() {
        return 0;
    }
    for signal in ["TERM", "KILL"] {
        for p in targets.iter().filter(|p| same(p)) {
            proc::signal(p.pid, signal);
        }
        let deadline = Instant::now() + SWEEP;
        while targets.iter().any(same) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
    }
    targets.len()
}

/// An owner that was signalled could not say goodbye: leave the line on its
/// terminal for it, and the cursor it may have hidden.
fn tell_terminal(owner: &Record) {
    use std::io::Write;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    let Some(tty) = owner.tty.as_deref().filter(|t| t.starts_with("/dev/")) else {
        return;
    };
    if !std::fs::metadata(tty).is_ok_and(|m| m.file_type().is_char_device()) {
        return;
    }
    // Never as this process's controlling terminal.
    let noctty = if cfg!(target_os = "macos") {
        0x20000
    } else {
        0o400
    };
    let Ok(mut out) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(noctty)
        .open(tty)
    else {
        return;
    };
    let line = super::taken_over_line(&me());
    let _ = write!(
        out,
        "\r\n\x1b[0m\x1b[?25h{line}\r\n(it was stopped by a signal: if this terminal \
         misbehaves, type `reset`)\r\n"
    );
}

/// Wait, briefly, for the coordinator to have noticed that the old owner is
/// gone, so that the new one joins under the name it held.
fn forgotten(pid: u32) {
    let Some(dir) = crate::coord::runtime_dir() else {
        return;
    };
    if std::env::var_os(crate::coord::ENV_DISABLE).is_some_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        // Asked of a running coordinator only; none is started for this.
        let Ok(list) = crate::coord::client::control(&dir, "list") else {
            return;
        };
        let listed = list["instances"]
            .as_array()
            .is_some_and(|all| all.iter().any(|i| i["pid"] == pid));
        if !listed {
            return;
        }
        std::thread::sleep(POLL);
    }
}
