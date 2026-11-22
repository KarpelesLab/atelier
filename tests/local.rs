//! The local model as real processes: ateliers that ask the coordinator for
//! the model server, share it, have it load their models, and leave it to
//! be stopped — against a stand-in for oxinfer.
//!
//! The stand-in is this test program itself: `fake_oxinfer` below is a
//! test that does nothing unless it is started as the server (by the small
//! script `ATELIER_OXINFER_BIN` names), and then serves what atelier uses of
//! oxinfer's API — the model list, the management API's plan, download and
//! load, and a chat completion that says which model answered and from
//! which process. It writes what it is asked to a log the tests read.

#![cfg(unixlike)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_atelier");
/// Set, for the stand-in: the arguments `oxinfer serve` was given.
const FAKE_ARGS: &str = "ATELIER_TEST_FAKE_OXINFER";
/// Where the stand-in writes what it is asked.
const FAKE_LOG: &str = "ATELIER_TEST_FAKE_LOG";
/// A repository whose weights the stand-in says are not here: 30 GB.
const BIG: &str = "big/model";

// ---- the stand-in ----

/// Not a test: the stand-in oxinfer server, when started as one.
#[test]
fn fake_oxinfer() {
    let Ok(args) = std::env::var(FAKE_ARGS) else {
        return;
    };
    let args: Vec<&str> = args.split_whitespace().collect();
    let listen = args
        .iter()
        .position(|a| *a == "--listen")
        .map(|i| args[i + 1])
        .expect("--listen");
    // As the real one: it starts without a model, and is told not to
    // update itself.
    assert!(!args.contains(&"--model"), "{args:?}");
    assert!(args.contains(&"--no-update-check"), "{args:?}");
    let key = std::env::var("OXINFER_API_KEY").expect("a key in the environment");
    let log = std::env::var(FAKE_LOG).expect("a log");
    let note = move |line: String| {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    };
    let listener = TcpListener::bind(listen).expect("bind");
    note(format!("serve {} {listen}", std::process::id()));
    // (id, source, options, state)
    let models: Arc<Mutex<Vec<[String; 4]>>> = Arc::default();
    let fetched: Arc<Mutex<Vec<String>>> = Arc::default();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (key, note, models, fetched) =
            (key.clone(), note.clone(), models.clone(), fetched.clone());
        std::thread::spawn(move || {
            let _ = answer(stream, &key, &note, &models, &fetched);
        });
    }
}

fn answer(
    stream: TcpStream,
    key: &str,
    note: &dyn Fn(String),
    models: &Mutex<Vec<[String; 4]>>,
    fetched: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let (mut request, mut authorized, mut length) = (String::new(), false, 0usize);
    reader.read_line(&mut request)?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        let lower = line.trim().to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            authorized = line.trim().ends_with(&format!("Bearer {key}"));
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let json = serde_json::Value::to_string;

    let (status, content) = if !authorized {
        (
            "401 Unauthorized",
            r#"{"error":{"message":"no key"}}"#.to_string(),
        )
    } else {
        match (method, path) {
            ("GET", "/v1/models") => {
                let data: Vec<_> = models
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|m| serde_json::json!({"id": m[0], "object": "model"}))
                    .collect();
                (
                    "200 OK",
                    json(&serde_json::json!({"object": "list", "data": data})),
                )
            }
            ("GET", "/api/web/models") => {
                let data: Vec<_> = models
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m[0], "source": m[1], "options": [m[2]], "state": m[3],
                            "load": {"state": m[3], "error": "no such repository"},
                        })
                    })
                    .collect();
                ("200 OK", json(&serde_json::json!({"data": data})))
            }
            ("GET", "/api/web/catalog") => (
                "200 OK",
                json(&serde_json::json!({
                    "calibrated": true, "hardware": {"memory_gb": 64.0}, "models": [],
                })),
            ),
            ("GET", "/api/web/cache") => ("200 OK", r#"{"entries":[]}"#.to_string()),
            ("POST", "/api/web/plan") => {
                let model = text(&body["model"]);
                let missing = if model == BIG && !fetched.lock().unwrap().contains(&model) {
                    30e9
                } else {
                    0.0
                };
                note(format!("plan {model}"));
                (
                    "200 OK",
                    json(&serde_json::json!({
                        "model": model, "total": 30e9, "missing": missing, "free": 500e9,
                    })),
                )
            }
            ("POST", "/api/web/downloads") => {
                let model = text(&body["model"]);
                note(format!("download {model}"));
                fetched.lock().unwrap().push(model);
                ("200 OK", r#"{"id":"dl-1","status":"planning"}"#.to_string())
            }
            ("GET", "/api/web/downloads/dl-1") => (
                "200 OK",
                r#"{"id":"dl-1","status":"done","done":30e9,"total":30e9}"#.to_string(),
            ),
            ("POST", "/api/web/models/load") => {
                let (id, source) = (text(&body["id"]), text(&body["model"]));
                let options = text(&body["options"]);
                note(format!("load {id} {source} [{options}]"));
                let state = if source.starts_with("broken/") {
                    "error"
                } else {
                    "ready"
                };
                let mut models = models.lock().unwrap();
                models.retain(|m| m[0] != id);
                models.push([id.clone(), source, options, state.to_string()]);
                (
                    "200 OK",
                    json(&serde_json::json!({"id": id, "load": {"state": "queued"}})),
                )
            }
            ("POST", "/v1/chat/completions") => {
                let model = text(&body["model"]);
                note(format!("chat {model}"));
                if !models.lock().unwrap().iter().any(|m| m[0] == model) {
                    (
                        "404 Not Found",
                        r#"{"error":{"message":"the model does not exist"}}"#.to_string(),
                    )
                } else {
                    let said = format!("answer from {model} by {}", std::process::id());
                    let chunk = serde_json::json!({
                        "choices": [{"delta": {"content": said}, "finish_reason": null}]
                    });
                    let done = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
                    let sse = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
                    write!(
                        writer,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Connection: close\r\n\r\n{sse}"
                    )?;
                    return writer.shutdown(std::net::Shutdown::Write);
                }
            }
            _ => (
                "404 Not Found",
                r#"{"error":{"message":"no such thing"}}"#.to_string(),
            ),
        }
    };
    write!(
        writer,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{content}",
        content.len()
    )
}

// ---- the tests' own ----

/// One test's directories, and what it started: the coordinator is told to
/// stop at the end, and what is left of the servers is killed.
struct Bench {
    dir: PathBuf,
}

impl Bench {
    fn new(name: &str) -> Bench {
        let dir = std::env::temp_dir().join(format!("atl-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let bench = Bench { dir };
        bench.oxinfer("oxinfer", "0.0.16");
        bench
    }

    /// The script that stands for the oxinfer program: it says its version,
    /// and serves by becoming `fake_oxinfer`.
    fn oxinfer(&self, name: &str, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.dir.join(name);
        let me = std::env::current_exe().unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo \"oxinfer {version}\";;\n\
             serve) shift; {FAKE_ARGS}=\"$*\" exec \"{}\" --exact fake_oxinfer --nocapture \
             --test-threads=1;;\n*) echo 'usage: oxinfer serve';;\nesac\n",
            me.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn coord(&self) -> PathBuf {
        self.dir.join("run")
    }

    /// An atelier set to use this bench's coordinator and stand-in, and
    /// nothing of the user's.
    fn atelier(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env("ATELIER_COORDINATOR_DIR", self.coord())
            .env("ATELIER_OXINFER_BIN", self.dir.join("oxinfer"))
            .env(FAKE_LOG, self.dir.join("fake.log"))
            // The server is stopped a second after its last user leaves.
            .env("ATELIER_LOCAL_IDLE", "1")
            .env("ATELIER_BASE_URL", "http://127.0.0.1:9")
            .env("ATELIER_APPROVE", "all")
            .env("HOME", self.dir.join("home"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("ATELIER_STATE_DIR", self.dir.join("state/atelier"))
            .env("XDG_RUNTIME_DIR", self.dir.join("xdg-run"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env_remove("ATELIER_NO_COORDINATOR")
            .env_remove("ATELIER_BACKEND")
            .env_remove("ATELIER_LOCAL_MODEL")
            .env_remove("ATELIER_LOCAL_ACCEPT")
            .env_remove("ATELIER_API_KEY")
            .env_remove("ATELIER_MODEL")
            .env_remove("ATELIER_MODE")
            .env_remove(FAKE_ARGS);
        cmd
    }

    /// An atelier with the local model `model` at its prompt (the line
    /// REPL, since stdin is a pipe), working in a directory called `name`:
    /// it uses the server for as long as it lives.
    fn session(&self, name: &str, model: &str) -> Child {
        let cwd = self.dir.join("work").join(name);
        std::fs::create_dir_all(&cwd).unwrap();
        self.atelier()
            .arg("--local")
            .env("ATELIER_LOCAL_MODEL", model)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn atelier")
    }

    /// `atelier --local --print` with `model`, to its end.
    fn print(&self, name: &str, model: &str, env: &[(&str, &str)]) -> Output {
        let cwd = self.dir.join("work").join(name);
        std::fs::create_dir_all(&cwd).unwrap();
        self.atelier()
            .args(["--local", "--print", "say something"])
            .env("ATELIER_LOCAL_MODEL", model)
            .envs(env.iter().copied())
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .expect("run atelier")
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self
            .atelier()
            .args(args)
            .current_dir(self.dir.join("work"))
            .stdin(Stdio::null())
            .output()
            .expect("run atelier");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// What the stand-in was asked, a line each, starting with `what`.
    fn asked(&self, what: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("fake.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with(what))
            .map(String::from)
            .collect()
    }

    /// Wait until the stand-in has been asked to load `n` models: a
    /// session is a user of the server before its model is there.
    fn loaded(&self, n: usize) {
        wait_for("the model to be loaded", || {
            (self.asked("load ").len() >= n).then_some(())
        });
    }

    /// The pids of the servers started so far.
    fn servers(&self) -> Vec<u32> {
        self.asked("serve ")
            .iter()
            .map(|l| l.split(' ').nth(1).unwrap().parse().unwrap())
            .collect()
    }

    /// The running coordinator's pid.
    fn coordinator(&self) -> Option<u32> {
        let out = self.run(&["coordinator", "status"]);
        out.split("(pid ").nth(1)?.split(')').next()?.parse().ok()
    }

    /// How many ateliers the coordinator counts as using the server, and
    /// the server's pid; `None` when it runs none.
    fn in_use(&self) -> Option<(u32, usize)> {
        let out = self.run(&["coordinator", "status"]);
        let line = out.lines().find(|l| l.starts_with("local model server"))?;
        let pid = line
            .split("(pid ")
            .nth(1)?
            .split(')')
            .next()?
            .parse()
            .ok()?;
        let users = line.rsplit("used by ").next()?.split(' ').next()?;
        Some((pid, users.parse().ok()?))
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = self.run(&["coordinator", "stop"]);
        for pid in self.servers() {
            if alive(pid) {
                kill(pid);
            }
        }
        // (Left in place when the test failed: the logs say why.)
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn kill(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stderr(Stdio::null())
        .status();
}

/// Whether the process is there (a zombie is not).
fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = check() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// End a session the way its user does: the end of its input.
fn quit(mut session: Child) {
    drop(session.stdin.take());
    session.wait().unwrap();
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The name the server knows a model by, as `atelier local status` lists it.
fn id_of(bench: &Bench, source: &str) -> String {
    let status = bench.run(&["local", "status"]);
    let line = status
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{source} (")))
        .unwrap_or_else(|| panic!("{source} is not listed in:\n{status}"));
    line.split('(')
        .nth(1)
        .unwrap()
        .split(')')
        .next()
        .unwrap()
        .to_string()
}

/// Two ateliers use one server, each with the model it asked for: the
/// second neither starts a server nor is refused for wanting another model,
/// and a third that wants the first one's model does not load it again.
#[test]
fn ateliers_share_one_server_and_each_has_its_model() {
    let bench = Bench::new("share");
    let a = bench.session("alpha", "org/one,quant=q4");
    let (server, _) = wait_for("the first session's lease", || {
        bench.in_use().filter(|(_, users)| *users == 1)
    });
    bench.loaded(1);
    let b = bench.session("beta", "org/two");
    wait_for("the second session's lease", || {
        bench.in_use().filter(|(_, users)| *users == 2)
    });
    bench.loaded(2);
    assert_eq!(bench.servers(), [server], "one server for both");

    // Each had the server load its own, under a name of its own, with the
    // options apart from the source.
    let (one, two) = (id_of(&bench, "org/one"), id_of(&bench, "org/two"));
    assert_ne!(one, two);
    assert_eq!(
        bench.asked("load "),
        [
            format!("load {one} org/one [quant=q4]"),
            format!("load {two} org/two []")
        ]
    );
    let status = bench.run(&["local", "status"]);
    assert!(status.contains("used by 2 ateliers"), "{status}");

    // A run with the first model is answered by it, from that same server,
    // which is not asked to load it again; and one with the second, by the
    // second.
    let (out, err) = text(&bench.print("gamma", "org/one,quant=q4", &[]));
    assert_eq!(
        out.trim(),
        format!("answer from {one} by {server}"),
        "{err}"
    );
    let (out, err) = text(&bench.print("delta", "org/two", &[]));
    assert_eq!(
        out.trim(),
        format!("answer from {two} by {server}"),
        "{err}"
    );
    assert_eq!(bench.asked("load ").len(), 2);
    assert_eq!(bench.servers(), [server]);

    // The key is in the server's environment, not among its arguments, and
    // what is written of it is the user's alone.
    let args = Command::new("ps")
        .args(["-o", "args=", "-p", &server.to_string()])
        .output()
        .unwrap();
    let record = std::fs::read_to_string(bench.coord().join("oxinfer-server.json")).unwrap();
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    let key = record["key"].as_str().unwrap();
    assert!(!String::from_utf8_lossy(&args.stdout).contains(key));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(bench.coord().join("oxinfer-server.json")), 0o600);
        assert_eq!(mode(bench.coord().join("coordinator.sock")), 0o600);
        assert_eq!(mode(bench.coord()), 0o700);
    }
    // And `coordinator status`, which anybody of the user's may ask, does
    // not print it.
    assert!(!bench.run(&["coordinator", "status"]).contains(key));

    quit(a);
    quit(b);
}

/// The server is stopped once nobody uses it — a session that ended, and
/// one that was killed, alike — and the coordinator does not leave before.
#[test]
fn the_server_stops_after_its_last_user_and_a_crash_counts_as_gone() {
    let bench = Bench::new("idle");
    let a = bench.session("alpha", "org/one");
    bench.loaded(1);
    let mut b = bench.session("beta", "org/one");
    let (server, _) = wait_for("both leases", || {
        bench.in_use().filter(|(_, users)| *users == 2)
    });
    // Both asked for the same model: it is loaded once. (The second is at
    // its prompt once it has looked.)
    let again = bench.print("gamma", "org/one", &[]);
    assert!(again.status.success());
    assert_eq!(bench.asked("load ").len(), 1);

    // Killed outright, the second one's lease is gone with its connection.
    b.kill().unwrap();
    b.wait().unwrap();
    wait_for("the killed session's lease to end", || {
        bench.in_use().filter(|(_, users)| *users == 1)
    });
    // The first one keeps the server well past the idle time.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(alive(server));
    assert_eq!(bench.in_use(), Some((server, 1)));

    quit(a);
    wait_for("the server to be stopped", || {
        (!alive(server)).then_some(())
    });
    assert!(!bench.coord().join("oxinfer-server.json").exists());
    assert_eq!(bench.in_use(), None);
    assert!(
        bench
            .run(&["local", "status"])
            .contains("server: not running")
    );

    // The next session gets a new one.
    let c = bench.session("gamma", "org/one");
    let (second, _) = wait_for("a new server", || bench.in_use());
    assert_ne!(second, server);
    assert_eq!(bench.servers(), [server, second]);
    // `atelier local stop` ends it under its user.
    assert!(bench.run(&["local", "stop"]).contains("stopped"));
    wait_for("the server to be stopped", || {
        (!alive(second)).then_some(())
    });
    assert!(bench.run(&["local", "stop"]).contains("not running"));
    quit(c);
}

/// The server outlives the coordinator: a new coordinator finds it again,
/// with its models, and its users say again that they use it.
#[test]
fn the_server_survives_the_coordinator_and_is_adopted() {
    let bench = Bench::new("adopt");
    let a = bench.session("alpha", "org/one");
    let (server, _) = wait_for("the lease", || bench.in_use());
    bench.loaded(1);
    let first = bench.coordinator().unwrap();
    let one = id_of(&bench, "org/one");

    // Killed outright: the session starts another coordinator, which finds
    // the server (not a new one) and counts the session as its user again.
    kill(first);
    let second = wait_for("a new coordinator", || {
        bench.coordinator().filter(|pid| *pid != first)
    });
    assert_ne!(first, second);
    wait_for("the lease, made again", || {
        bench.in_use().filter(|(_, users)| *users == 1)
    });
    assert_eq!(bench.in_use(), Some((server, 1)));
    assert!(alive(server));
    // Well past the idle time: the lease made again keeps it.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(bench.in_use(), Some((server, 1)));

    // Another atelier is served by that server, with the model it had.
    let (out, err) = text(&bench.print("beta", "org/one", &[]));
    assert_eq!(
        out.trim(),
        format!("answer from {one} by {server}"),
        "{err}"
    );
    assert_eq!(bench.servers(), [server]);
    assert_eq!(bench.asked("load ").len(), 1);

    // The coordinator and the last user both killed: nobody is left to say
    // the server is unused. The next coordinator there is — any atelier
    // starts one — finds it, waits the idle time for a user to come back,
    // and stops it.
    let mut a = a;
    kill(second);
    a.kill().unwrap();
    a.wait().unwrap();
    assert!(alive(server));
    let plain = bench
        .atelier()
        .current_dir(bench.dir.join("work"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the orphaned server to be stopped", || {
        (!alive(server)).then_some(())
    });
    assert!(!bench.coord().join("oxinfer-server.json").exists());
    quit(plain);
}

/// A coordinator that is told to stop takes an unused server with it, and
/// leaves one that is in use to the coordinator its users start next.
#[test]
fn a_coordinator_that_ends_stops_a_server_nobody_uses() {
    let bench = Bench::new("stop");
    let a = bench.session("alpha", "org/one");
    let (server, _) = wait_for("the lease", || bench.in_use());
    let first = bench.coordinator().unwrap();
    assert!(bench.run(&["coordinator", "stop"]).contains("stopped"));
    wait_for("the next coordinator, with the lease", || {
        bench
            .in_use()
            .filter(|_| bench.coordinator().is_some_and(|pid| pid != first))
    });
    assert_eq!(bench.in_use(), Some((server, 1)));

    quit(a);
    // Unused now — and stopped with the coordinator, without the wait.
    wait_for("the lease to end", || {
        bench.in_use().filter(|(_, users)| *users == 0)
    });
    assert!(bench.run(&["coordinator", "stop"]).contains("stopped"));
    wait_for("the server to be stopped", || {
        (!alive(server)).then_some(())
    });
    assert!(!bench.coord().join("oxinfer-server.json").exists());
}

/// Weights that are not here are fetched only once the user agrees — where
/// nobody can be asked, by `ATELIER_LOCAL_ACCEPT=1`.
#[test]
fn weights_are_not_downloaded_unasked() {
    let bench = Bench::new("consent");
    let out = bench.print("alpha", BIG, &[]);
    let (_, err) = text(&out);
    assert!(!out.status.success());
    assert!(
        err.contains("big/model is not on this machine yet: a 30 GB download"),
        "{err}"
    );
    assert!(err.contains("ATELIER_LOCAL_ACCEPT=1"), "{err}");
    assert_eq!(bench.asked("plan "), ["plan big/model"]);
    assert!(bench.asked("download ").is_empty());
    assert!(bench.asked("load ").is_empty());

    let out = bench.print("alpha", BIG, &[("ATELIER_LOCAL_ACCEPT", "1")]);
    let (said, err) = text(&out);
    assert!(out.status.success(), "{err}");
    assert!(said.contains("answer from model-"), "{said}");
    assert_eq!(bench.asked("download "), ["download big/model"]);
    assert_eq!(bench.asked("load ").len(), 1);

    // A model the server cannot load at all is an error at once, with why.
    let out = bench.print("beta", "broken/model", &[]);
    let (_, err) = text(&out);
    assert!(!out.status.success());
    assert!(
        err.contains("oxinfer could not load broken/model: no such repository"),
        "{err}"
    );
}

/// Without the coordinator there is nobody to run the server: the local
/// model says so, and names the switch. An oxinfer too old to serve
/// without a model, when it is not atelier's to update, says so too.
#[test]
fn the_local_model_needs_the_coordinator_and_a_recent_oxinfer() {
    let bench = Bench::new("off");
    let out = bench.print("alpha", "org/one", &[("ATELIER_NO_COORDINATOR", "1")]);
    let (_, err) = text(&out);
    assert!(!out.status.success());
    assert!(err.contains("ATELIER_NO_COORDINATOR"), "{err}");
    assert!(err.contains("run by the coordinator"), "{err}");
    assert!(bench.servers().is_empty());
    assert_eq!(bench.coordinator(), None);

    let old = bench.oxinfer("oxinfer-old", "0.0.15");
    let out = bench.print(
        "alpha",
        "org/one",
        &[("ATELIER_OXINFER_BIN", old.to_str().unwrap())],
    );
    let (_, err) = text(&out);
    assert!(!out.status.success());
    assert!(
        err.contains("is oxinfer 0.0.15, and atelier needs 0.0.16 or newer"),
        "{err}"
    );
    assert!(bench.servers().is_empty());
    // The failure is the start's, not the coordinator's: a good program
    // is served right after.
    std::thread::sleep(Duration::from_millis(3100));
    let out = bench.print("alpha", "org/one", &[]);
    assert!(out.status.success(), "{}", text(&out).1);
}
