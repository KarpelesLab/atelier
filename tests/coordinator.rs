//! The coordinator as real processes: instances launch it, and when it is
//! killed they launch another and rejoin it. (Routing between instances is
//! covered by the unit tests in `src/coord`.)

#![cfg(unixlike)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_atelier");

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atl-coordinator-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An atelier at its prompt (the line REPL, since stdin is a pipe), working
/// in `cwd` and using the coordinator in `coord`.
fn instance(coord: &Path, cwd: &Path) -> Child {
    std::fs::create_dir_all(cwd).unwrap();
    Command::new(BIN)
        .env("ATELIER_COORDINATOR_DIR", coord)
        .env("ATELIER_BASE_URL", "http://127.0.0.1:9")
        .env_remove("ATELIER_NO_COORDINATOR")
        .env_remove("ATELIER_BACKEND")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn atelier")
}

/// `atelier coordinator <cmd>`'s output.
fn coordinator(coord: &Path, cmd: &str) -> String {
    let out = Command::new(BIN)
        .args(["coordinator", cmd])
        .env("ATELIER_COORDINATOR_DIR", coord)
        .output()
        .expect("run atelier coordinator");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The running coordinator's pid and the names of its instances.
fn status(coord: &Path) -> Option<(u32, Vec<String>)> {
    let out = coordinator(coord, "status");
    let pid = out.split("(pid ").nth(1)?.split(')').next()?.parse().ok()?;
    let names = out
        .lines()
        .skip(1)
        .filter_map(|l| l.split(" — ").next().filter(|_| l.contains(" — ")))
        .map(String::from)
        .collect();
    Some((pid, names))
}

fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = check() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn instances_launch_the_coordinator_and_relaunch_it_when_it_dies() {
    let dir = temp_dir("relaunch");
    let coord = dir.join("run");
    assert!(coordinator(&coord, "status").contains("not running"));

    // The first instance up starts the coordinator; the second joins it.
    let mut a = instance(&coord, &dir.join("alpha"));
    let mut b = instance(&coord, &dir.join("beta"));
    let both = |pid_not: u32| {
        status(&coord).filter(|(pid, names)| {
            *pid != pid_not
                && names.len() == 2
                && ["alpha", "beta"]
                    .iter()
                    .all(|n| names.iter().any(|x| x == n))
        })
    };
    let (first, _) = wait_for("both instances to join", || both(0));

    // Killed outright, it is replaced, and both instances rejoin on their own.
    Command::new("kill")
        .args(["-9", &first.to_string()])
        .status()
        .unwrap();
    let (second, _) = wait_for("a new coordinator with both instances", || both(first));
    assert_ne!(first, second);

    // An instance still reaches the other through the new one.
    writeln!(a.stdin.as_mut().unwrap(), "/send beta it's still   there").unwrap();
    drop(a.stdin.take()); // EOF: the instance exits
    let out = String::from_utf8_lossy(&a.wait_with_output().unwrap().stdout).into_owned();
    assert!(out.contains("sent to 1 instance(s)"), "{out}");

    // Instances leave when they exit; the coordinator stops when told.
    wait_for("alpha to leave", || {
        status(&coord).filter(|(_, names)| names == &["beta"])
    });
    drop(b.stdin.take());
    b.wait().unwrap();
    assert!(coordinator(&coord, "stop").contains("stopped"));
    wait_for("the coordinator to stop", || {
        coordinator(&coord, "status")
            .contains("not running")
            .then_some(())
    });
    let _ = std::fs::remove_dir_all(&dir);
}
