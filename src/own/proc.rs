//! The process table, as much of it as a take-over needs: whether a pid is
//! still the process it was, what a process started, and a signal for one.
//!
//! Read from `/proc` where there is one (Linux, fullrust included) and asked
//! of `ps` elsewhere; signals go through the `kill` program, there being no
//! libc here to call (as `tools::bash` and `hooks` do).

use std::process::{Command, Stdio};

/// One process, as the table has it.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub parent: u32,
    /// What tells this process from a later one with the same pid (see
    /// [`start_token`]).
    pub token: String,
}

/// What `/proc/<pid>/stat` says after the command's name: the state, then
/// the numbered fields from the fourth on.
fn stat(pid: u32) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name is in parentheses and may hold anything, spaces and
    // parentheses included: the fields start after the last one.
    let rest = &text[text.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// Whether this system has a `/proc` to read processes from.
fn has_proc() -> bool {
    std::path::Path::new("/proc/self/stat").exists()
}

fn ps(args: &[&str]) -> Option<String> {
    let out = Command::new("ps")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What identifies the process `pid` among all that ever had that number:
/// when it started (in clock ticks since boot from `/proc`, as `ps` prints
/// it elsewhere). `None` when there is no such process — a zombie counts as
/// none: it holds nothing any more — or when it cannot be told.
pub fn start_token(pid: u32) -> Option<String> {
    if has_proc() {
        let fields = stat(pid)?;
        // Field 3 is the state, field 22 the start time.
        if fields.first()? == "Z" {
            return None;
        }
        return fields.get(19).cloned();
    }
    let started = ps(&["-o", "lstart=", "-p", &pid.to_string()])?;
    let started = started.trim();
    (!started.is_empty()).then(|| started.to_string())
}

/// Whether a process `pid` is there at all, where that is all there is to
/// know of it.
pub fn exists(pid: u32) -> bool {
    let probe = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status();
    probe.is_ok_and(|s| s.success())
}

/// Every process there is.
fn table() -> Vec<Proc> {
    if has_proc() {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        return dir
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| {
                let fields = stat(pid)?;
                if fields.first()? == "Z" {
                    return None;
                }
                Some(Proc {
                    pid,
                    parent: fields.get(1)?.parse().ok()?,
                    token: fields.get(19)?.clone(),
                })
            })
            .collect();
    }
    let listed = ps(&["-axo", "pid=,ppid="]).unwrap_or_default();
    listed
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid: u32 = words.next()?.parse().ok()?;
            Some(Proc {
                pid,
                parent: words.next()?.parse().ok()?,
                token: start_token(pid)?,
            })
        })
        .collect()
}

/// The arguments `pid` was started with, less the program's name.
fn arguments(pid: u32) -> Vec<String> {
    if has_proc() {
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return Vec::new();
        };
        return raw
            .split(|&b| b == 0)
            .skip(1)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
    }
    let line = ps(&["-o", "args=", "-p", &pid.to_string()]).unwrap_or_default();
    line.split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// The helper an atelier starts that is not its own to stop: the
/// coordinator, which serves every instance — and runs the local model's
/// server, which it ends once nobody uses it.
const SHARED_HELPERS: [&str; 1] = ["coordinator"];

/// Everything `root` started, directly or not, that is still running: its
/// children, theirs, and so on — less the helpers other instances share
/// (with what those started), and less this process and its own ancestors,
/// should it be one of them (an `atelier` started from inside a command).
pub fn descendants(root: u32) -> Vec<Proc> {
    let table = table();
    let me = std::process::id();
    // This process and what it descends from.
    let mut mine = vec![me];
    while let Some(parent) = table
        .iter()
        .find(|p| Some(&p.pid) == mine.last())
        .map(|p| p.parent)
        .filter(|parent| *parent != 0 && !mine.contains(parent))
    {
        mine.push(parent);
    }
    let mut found: Vec<Proc> = Vec::new();
    let mut from = vec![root];
    while let Some(parent) = from.pop() {
        for child in table.iter().filter(|p| p.parent == parent) {
            if mine.contains(&child.pid) || found.iter().any(|f| f.pid == child.pid) {
                continue;
            }
            let shared = arguments(child.pid)
                .first()
                .is_some_and(|a| SHARED_HELPERS.contains(&a.as_str()));
            if shared {
                continue;
            }
            found.push(child.clone());
            from.push(child.pid);
        }
    }
    found
}

/// Send `signal` (`TERM`, `KILL`) to `pid`.
pub fn signal(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The terminal this process's input is, if it is one (`/dev/pts/3`).
pub fn terminal() -> Option<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let path = std::fs::read_link("/proc/self/fd/0")
        .or_else(|_| std::fs::read_link("/dev/fd/0"))
        .ok()?;
    let path = path.to_string_lossy().into_owned();
    path.starts_with("/dev/").then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_is_told_from_the_next_one_with_its_pid() {
        let me = std::process::id();
        let token = start_token(me).expect("this process exists");
        assert_eq!(start_token(me).as_deref(), Some(token.as_str()));

        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let born = start_token(pid).expect("the child exists");
        assert!(
            descendants(me)
                .iter()
                .any(|p| p.pid == pid && p.token == born),
            "the child is among this process's descendants"
        );
        child.kill().unwrap();
        // Killed and not yet waited for: a zombie, which is nobody.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while start_token(pid).is_some() {
            assert!(std::time::Instant::now() < deadline, "still there");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(descendants(me).iter().all(|p| p.pid != pid));
        child.wait().unwrap();
        assert_eq!(start_token(pid), None);
    }

    #[test]
    fn this_process_is_not_its_own_descendant_nor_its_parents() {
        // Asked about an ancestor, the walk leaves out the line that leads
        // to this process.
        let me = std::process::id();
        let parent = table().iter().find(|p| p.pid == me).unwrap().parent;
        assert!(descendants(parent).iter().all(|p| p.pid != me));
    }
}
