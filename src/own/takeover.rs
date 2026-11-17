//! Taking a session over: the newcomer's side.
//!
//! 1. **What the owner has going is written down** — every process it
//!    started, directly or not, each with its start time — before anything
//!    is asked of it: once the owner is gone its children belong to nobody
//!    and can no longer be found from it.
//! 2. **The place behind the owner is taken** in the coordinator's table
//!    (a claim that waits, see [`crate::coord::claims`]): from here the
//!    session passes to this process the moment the owner's claim ends, and
//!    to nobody else — a third atelier started meanwhile finds it owned
//!    throughout. If the owner is no longer the one this started from, or
//!    another atelier is already behind it, nothing more is done.
//! 3. **The owner is asked** over its socket to yield (`session/yield`,
//!    see [`super::hub`]). It interrupts its turn, saves, stops its
//!    background work and exits, which ends its claim.
//! 4. **The claim is waited for.** If it has not come a little after the
//!    owner's own deadline, the owner is signalled: TERM, then KILL. Never
//!    without checking that the pid is still the process the record
//!    describes (its start time) — a pid that was recycled is left alone.
//!    The take-over is done when the claim is this process's *and* that
//!    process is gone: a claim that came because the coordinator was
//!    replaced under an owner still running is not the end of it.
//! 5. **What the owner left running is stopped**: the processes written
//!    down in step 1 that are still there and still the same, with what
//!    they started since. The coordinator and the local model's server are
//!    not the owner's to stop and are never on that list.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;

use super::imp::{Seat, Turn, genuine, remove_socket};
use super::{Live, Record, YIELD_GRACE, hub, proc};

/// How long past the owner's own deadline the claim is waited for before
/// the owner is signalled, and how long each signal is given.
const PATIENCE: Duration = Duration::from_secs(2);
/// How long what the owner left behind has to end on TERM.
const SWEEP: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(25);

pub(super) enum Outcome {
    /// The session is this process's, and its old owner gone. `notes` say
    /// what was done.
    Done { notes: Vec<String> },
    /// Another atelier owns the session now, or is already taking it: not
    /// what this was about. In a sentence.
    Changed(String),
}

/// Who is taking over, for the owner to say.
fn me() -> String {
    match proc::terminal() {
        Some(tty) => format!("pid {} on {tty}", std::process::id()),
        None => format!("pid {}", std::process::id()),
    }
}

/// How a wait for the session ended.
enum Waited {
    /// It is this process's and `owner` is gone.
    Done,
    /// Not by the deadline.
    NotYet,
    /// It went to somebody else.
    Changed(String),
}

/// Wait until the session is this process's and `owner` gone, or `deadline`.
fn wait(seat: &mut Seat, owner: &Record, deadline: Instant) -> Result<Waited> {
    loop {
        match seat.next(owner, POLL)? {
            Turn::Changed(how) => return Ok(Waited::Changed(how)),
            Turn::Mine if !genuine(owner) => return Ok(Waited::Done),
            // The claim, and its old owner still running: waited out.
            Turn::Mine => std::thread::sleep(POLL),
            Turn::Waiting => {}
        }
        if Instant::now() >= deadline {
            return Ok(Waited::NotYet);
        }
    }
}

pub(super) fn run(live: &Live, seat: &mut Seat) -> Result<Outcome> {
    let owner = &live.record;
    let left = proc::descendants(owner.pid);
    if let Turn::Changed(how) = seat.queue(owner)? {
        return Ok(Outcome::Changed(how));
    }
    let mut how = "it had ended";
    let mut waited = wait(seat, owner, Instant::now())?;
    if let Waited::NotYet = waited {
        how = "it stopped when asked";
        let asked = hub::ask(owner, "session/yield", json!({ "by": me() })).is_ok();
        let patience = if asked {
            YIELD_GRACE + PATIENCE
        } else {
            Duration::ZERO
        };
        waited = wait(seat, owner, Instant::now() + patience)?;
        for signal in ["TERM", "KILL"] {
            if !matches!(waited, Waited::NotYet) {
                break;
            }
            if !genuine(owner) {
                // Gone, and its claim not let go of yet: about to be.
                waited = wait(seat, owner, Instant::now() + PATIENCE)?;
                break;
            }
            proc::signal(owner.pid, signal);
            how = match (asked, signal) {
                (true, "TERM") => "it did not stop when asked, and was terminated",
                (false, "TERM") => "it could not be asked, and was terminated",
                _ => "it had to be killed",
            };
            waited = wait(seat, owner, Instant::now() + PATIENCE)?;
        }
    }
    match waited {
        Waited::Done => {}
        Waited::Changed(how) => return Ok(Outcome::Changed(how)),
        Waited::NotYet => {
            seat.let_go();
            bail!(
                "could not take the session over: pid {} {}",
                owner.pid,
                if genuine(owner) {
                    "is still running"
                } else {
                    "is gone, and the coordinator still has the session as its own"
                }
            );
        }
    }

    let swept = sweep(&left);
    remove_socket(owner);
    if how.contains("terminated") || how.contains("killed") {
        tell_terminal(owner);
    }
    forgotten(owner.pid);

    let mut note = format!("took over the session of pid {} ({how})", owner.pid);
    if swept > 0 {
        note.push_str(&format!("; stopped {swept} process(es) it left running"));
    }
    Ok(Outcome::Done {
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
