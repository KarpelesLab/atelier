//! A model on this machine: atelier runs [oxinfer] for the user who has no
//! endpoint, key or subscription to point it at.
//!
//! oxinfer is not linked in. It is a separate program, taken from its GitHub
//! releases the first time it is needed and run as `oxinfer serve`, an
//! OpenAI-compatible server on a loopback port — so the rest of atelier
//! talks to it like to any other provider (it becomes the provider named
//! `local`).
//!
//! Three things happen on the way to the first answer, each only once:
//!
//! 1. **The program** ([`binary`]): the newest release that has an archive
//!    for this platform is downloaded, checked (its size, and its SHA-256
//!    where GitHub lists one), unpacked and test-run, into
//!    `$XDG_DATA_HOME/atelier/oxinfer/`. `ATELIER_OXINFER_BIN` names a
//!    program to use instead.
//! 2. **The weights** ([`download`]): tens of gigabytes for the default
//!    model, so never fetched without the user saying yes ([`Plan`] carries
//!    what to tell them). oxinfer does the fetching, into its own cache.
//! 3. **The server** ([`connect`]): one per user, shared by every atelier
//!    running — a model of this size is not something to load twice.
//!
//! # One server, as long as someone uses it
//!
//! The server belongs to a small supervisor, `atelier local-serve`, which
//! instances start detached when none is running (a lock file keeps it
//! unique, as for the coordinator). It starts oxinfer on a port of its own
//! choosing with a key of its own making, and writes both to a private
//! state file once the server answers.
//!
//! Who is using it is told by a lock, not by a list: every instance holds a
//! *shared* lock on a lease file for as long as it lives, and the supervisor
//! shuts the server down once it has been able to take that lock
//! *exclusively* for [`IDLE`] — that is, once nobody holds it. The kernel
//! drops an instance's lock however it dies, so a crash never leaves the
//! model in memory.
//!
//! [oxinfer]: https://github.com/KarpelesLab/oxinfer

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Where oxinfer's releases are published.
const REPO: &str = "KarpelesLab/oxinfer";
/// The model run when the user names none: Qwen3.8 27B, which reads images
/// as well as text and calls tools, at 4 bits (about 19 GB of memory).
pub const DEFAULT_MODEL: &str = "Qwen/Qwen3.8-27B,quant=q4";
/// The name the server knows the model by, whatever it is.
pub const MODEL_ID: &str = "local";
/// Names a model to run instead of [`DEFAULT_MODEL`], as oxinfer writes
/// them: `SOURCE[,KEY=VALUE...]`.
pub const ENV_MODEL: &str = "ATELIER_LOCAL_MODEL";
/// Names an oxinfer executable to use instead of a downloaded one.
pub const ENV_BIN: &str = "ATELIER_OXINFER_BIN";
/// `1` agrees to the weights download where nobody can be asked.
pub const ENV_ACCEPT: &str = "ATELIER_LOCAL_ACCEPT";

/// How long the server outlives its last user.
const IDLE: Duration = Duration::from_secs(60);
/// How long a server gets to start answering.
const START_WAIT: Duration = Duration::from_secs(180);
/// The release archives are tens of megabytes.
const DOWNLOAD_TIME: Duration = Duration::from_secs(900);

/// The model to run: the source oxinfer loads, and its options.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// `SOURCE[,KEY=VALUE...]`, as given to `oxinfer serve --model`.
    pub spec: String,
}

impl Model {
    /// The model the environment or the user's own settings name, else the
    /// default. A project's `atelier.toml` has no say: what to download and
    /// load is the user's choice.
    pub fn configured(user: &crate::settings::Settings) -> Self {
        let spec = std::env::var(ENV_MODEL)
            .ok()
            .or_else(|| user.local.model.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Self { spec }
    }

    /// The repository, folder or file, without the options.
    pub fn source(&self) -> &str {
        self.spec.split(',').next().unwrap_or(&self.spec)
    }
}

/// What starting the model would take, for the user to agree to.
#[derive(Debug, Default)]
pub struct Plan {
    /// Gigabytes to download; `None` when the weights are here already (or
    /// oxinfer does not know the model, and will say on its own).
    pub download_gb: Option<f64>,
    /// Gigabytes free where the weights go.
    pub disk_free_gb: Option<f64>,
    /// Gigabytes of memory the model takes, and those available.
    pub memory_gb: Option<f64>,
    pub memory_available_gb: Option<f64>,
    /// Where it would run (`GPU`, `cpu`, …) and how fast, as oxinfer
    /// estimates.
    pub runs_on: Option<String>,
    pub tokens_per_s: Option<f64>,
}

impl Plan {
    /// One line per thing worth knowing before saying yes.
    pub fn lines(&self, model: &Model) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(gb) = self.download_gb {
            let free = self
                .disk_free_gb
                .map(|f| format!(" ({f:.0} GB free)"))
                .unwrap_or_default();
            out.push(format!(
                "{} is not on this machine yet: a {gb:.0} GB download{free}",
                model.source()
            ));
            if self.disk_free_gb.is_some_and(|free| free < gb) {
                out.push("  that is more than the disk has free".to_string());
            }
        }
        if let Some(need) = self.memory_gb {
            let speed = match (&self.runs_on, self.tokens_per_s) {
                (Some(on), Some(t)) => format!(", on the {on} at about {t:.0} tokens/s"),
                (Some(on), None) => format!(", on the {on}"),
                _ => String::new(),
            };
            out.push(format!("it takes about {need:.0} GB of memory{speed}"));
            if self.memory_available_gb.is_some_and(|have| have < need) {
                out.push(format!(
                    "  only {:.0} GB are available: expect it to be very slow, or to fail — \
                     {ENV_MODEL} names a smaller model",
                    self.memory_available_gb.unwrap_or_default()
                ));
            }
        }
        out
    }
}

/// A running server this process may use, for as long as it keeps this.
pub struct Local {
    /// `http://127.0.0.1:<port>/v1`.
    pub base_url: String,
    pub key: String,
    /// The shared lock that tells the supervisor someone is here.
    _lease: std::fs::File,
}

/// `$XDG_DATA_HOME/atelier/oxinfer` (else `~/.local/share/…`): the program.
fn data_dir() -> Result<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let base = var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("no home directory to install oxinfer in (set XDG_DATA_HOME or HOME)")?;
    Ok(base.join("atelier").join("oxinfer"))
}

/// Where the supervisor's lock, lease, state and log live: next to the
/// coordinator's socket, in a directory only the user can enter.
fn run_dir() -> Result<PathBuf> {
    crate::coord::runtime_dir().context("no runtime directory (set XDG_RUNTIME_DIR or HOME)")
}

/// The release archive names to try for this platform, best first: the
/// glibc build can use a GPU, the static one runs where there is no glibc.
fn archive_suffixes() -> &'static [&'static str] {
    let linux = cfg!(any(target_os = "linux", target_os = "fullrust"));
    if linux && cfg!(target_arch = "x86_64") {
        &["linux-x86_64.tar.gz", "linux-x86_64-static.tar.gz"]
    } else if linux && cfg!(target_arch = "aarch64") {
        &["linux-aarch64.tar.gz"]
    } else if cfg!(target_os = "macos") {
        &["macos-universal.tar.gz"]
    } else {
        &[]
    }
}

fn http_get(url: &str, key: Option<&str>, time: Duration) -> Result<Vec<u8>> {
    let mut req = rsurl::Request::new("GET", url)
        .with_context(|| format!("building request for {url}"))?
        .header("user-agent", &format!("atelier/{}", crate::update::VERSION))
        .header(
            "accept",
            "application/vnd.github+json, application/octet-stream",
        )
        .follow_redirects(true)
        .connect_timeout(Duration::from_secs(15))
        .max_time(time);
    if let Some(key) = key {
        req = req.header("authorization", &format!("Bearer {key}"));
    }
    let resp = req.send().with_context(|| format!("fetching {url}"))?;
    if !(200..300).contains(&resp.status) {
        bail!("{url}: HTTP {}", resp.status);
    }
    Ok(resp.body)
}

/// The oxinfer executable: the one `ATELIER_OXINFER_BIN` names, the one
/// installed earlier, or one installed now. `say` gets a line per step.
pub fn binary(say: &mut dyn FnMut(&str)) -> Result<PathBuf> {
    if let Some(bin) = std::env::var_os(ENV_BIN).filter(|v| !v.is_empty()) {
        let bin = PathBuf::from(bin);
        if !bin.is_file() {
            bail!("{ENV_BIN} names {}, which is not a file", bin.display());
        }
        return Ok(bin);
    }
    let bin = data_dir()?.join("oxinfer");
    if bin.is_file() {
        return Ok(bin);
    }
    install(&bin, say)?;
    Ok(bin)
}

/// Download, check and unpack the newest release that has an archive for
/// this platform, as `bin`.
fn install(bin: &Path, say: &mut dyn FnMut(&str)) -> Result<()> {
    let suffixes = archive_suffixes();
    if suffixes.is_empty() {
        bail!(
            "oxinfer publishes no build for this platform; install it yourself and point \
             {ENV_BIN} at it"
        );
    }
    let api = std::env::var("ATELIER_UPDATE_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string());
    let listing = http_get(
        &format!(
            "{}/repos/{REPO}/releases?per_page=10",
            api.trim_end_matches('/')
        ),
        None,
        Duration::from_secs(60),
    )
    .context("listing oxinfer's releases")?;
    let releases: Value = serde_json::from_slice(&listing).context("parsing the releases")?;

    // A release is published before its archives are built: take the newest
    // that has one for us. Then each kind of archive in turn, since the
    // first may not run here (no glibc).
    let mut last = None;
    for suffix in suffixes {
        let found = releases.as_array().into_iter().flatten().find_map(|r| {
            let tag = r["tag_name"].as_str()?;
            let name = format!("oxinfer-{tag}-{suffix}");
            let asset = r["assets"]
                .as_array()?
                .iter()
                .find(|a| a["name"] == name.as_str())?;
            Some((tag.to_string(), asset.clone()))
        });
        let Some((tag, asset)) = found else { continue };
        say(&format!(
            "downloading oxinfer {tag} ({:.0} MB)…",
            asset["size"].as_f64().unwrap_or(0.0) / 1e6
        ));
        match fetch_into(bin, &asset) {
            Ok(()) => {
                say(&format!("installed {}", bin.display()));
                return Ok(());
            }
            Err(e) => last = Some(e.context(format!("oxinfer {tag} ({suffix})"))),
        }
    }
    Err(last.unwrap_or_else(|| {
        anyhow::anyhow!("none of oxinfer's recent releases has an archive for this platform")
    }))
}

/// One release archive: downloaded, verified, unpacked to `bin`, test-run.
fn fetch_into(bin: &Path, asset: &Value) -> Result<()> {
    let url = asset["browser_download_url"]
        .as_str()
        .context("the release lists no download URL")?;
    let archive = http_get(url, None, DOWNLOAD_TIME).context("downloading")?;
    if let Some(size) = asset["size"].as_u64()
        && size != archive.len() as u64
    {
        bail!(
            "the download is {} bytes, the release says {size}",
            archive.len()
        );
    }
    // GitHub lists a digest for newer uploads; where it does, it must match.
    if let Some(want) = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        let got: String = purecrypto::hash::sha256(&archive)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if !got.eq_ignore_ascii_case(want) {
            bail!("checksum mismatch: not installing it");
        }
    }
    let tar = compcol::vec::decompress_to_vec_capped::<compcol::gzip::Gzip>(&archive, 1 << 30)
        .map_err(|e| anyhow::anyhow!("unpacking: {e}"))?;
    let program = untar(&tar, "oxinfer").context("the archive holds no `oxinfer`")?;

    let dir = bin.parent().context("no directory to install in")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".oxinfer.new-{}", std::process::id()));
    std::fs::write(&tmp, program).with_context(|| format!("writing {}", tmp.display()))?;
    let done = (|| {
        #[cfg(unixlike)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        // It must at least start on this machine.
        let out = std::process::Command::new(&tmp)
            .args(["serve", "--help"])
            .output()
            .context("running it")?;
        if !String::from_utf8_lossy(&out.stdout).contains("oxinfer serve")
            && !String::from_utf8_lossy(&out.stderr).contains("oxinfer serve")
        {
            bail!("it does not run here");
        }
        std::fs::rename(&tmp, bin).with_context(|| format!("installing {}", bin.display()))
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// The contents of the regular file called `name` (in any directory) of a
/// tar archive.
fn untar<'a>(tar: &'a [u8], name: &str) -> Option<&'a [u8]> {
    let mut at = 0;
    while at + 512 <= tar.len() {
        let header = &tar[at..at + 512];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let text = |range: std::ops::Range<usize>| {
            let field = &header[range];
            let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
            String::from_utf8_lossy(&field[..end]).into_owned()
        };
        let size = usize::from_str_radix(text(124..136).trim(), 8).ok()?;
        let path = text(0..100);
        let regular = matches!(header[156], b'0' | 0);
        let start = at + 512;
        let end = start.checked_add(size)?;
        if end > tar.len() {
            return None;
        }
        if regular && path.rsplit('/').next() == Some(name) {
            return Some(&tar[start..end]);
        }
        at = start + size.div_ceil(512) * 512;
    }
    None
}

/// Ask oxinfer what `model` would take on this machine. A model it has no
/// table entry for (a local folder, a repository it has not measured) gives
/// an empty plan: oxinfer fetches and reports as it goes.
pub fn plan(bin: &Path, model: &Model) -> Plan {
    let out = std::process::Command::new(bin)
        .args(["--models", "--model", model.source(), "--json"])
        .output();
    let Ok(out) = out else {
        return Plan::default();
    };
    let Ok(report) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Plan::default();
    };
    let Some(entry) = report["models"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["model"] == model.source())
    else {
        return Plan::default();
    };
    // The variant the options ask for (`quant=q4` is listed as `--quant
    // q4`), else the one oxinfer recommends.
    let quant = model
        .spec
        .split(',')
        .find_map(|o| o.strip_prefix("quant="))
        .map(|q| format!("--quant {q}"));
    let variants = entry["variants"].as_array().cloned().unwrap_or_default();
    let variant = variants
        .iter()
        .find(|v| quant.as_deref().is_some_and(|q| v["args"] == q))
        .or_else(|| variants.iter().find(|v| v["recommended"] == true));
    let hardware = &report["hardware"];
    Plan {
        download_gb: (entry["on_disk"] != true)
            .then(|| entry["download_gb"].as_f64())
            .flatten(),
        disk_free_gb: hardware["disk_free_gb"].as_f64(),
        memory_gb: variant.and_then(|v| v["memory_gb"].as_f64()),
        memory_available_gb: hardware["memory_available_gb"].as_f64(),
        runs_on: variant
            .and_then(|v| v["runs_on"].as_str())
            .map(str::to_string),
        tokens_per_s: variant.and_then(|v| v["decode_tok_s"].as_f64()),
    }
}

/// Fetch the model's weights with oxinfer (`--download-only`), passing on a
/// line of its progress now and then.
pub fn download(bin: &Path, model: &Model, say: &mut dyn FnMut(&str)) -> Result<()> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = std::process::Command::new(bin)
        .args(["--model", model.source(), "--download-only"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting oxinfer")?;
    let mut stderr = child.stderr.take().expect("piped");
    // Progress is redrawn in place (carriage returns): cut on either line
    // ending and say the latest one every so often, the last ones always.
    let (mut line, mut tail, mut told) = (Vec::new(), String::new(), Instant::now());
    let mut buf = [0u8; 4096];
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for b in &buf[..n] {
            if matches!(b, b'\r' | b'\n') {
                let text = String::from_utf8_lossy(&line).trim().to_string();
                line.clear();
                if text.is_empty() {
                    continue;
                }
                if told.elapsed() >= Duration::from_secs(20) {
                    say(&text);
                    told = Instant::now();
                }
                tail = text;
            } else {
                line.push(*b);
            }
        }
    }
    let status = child.wait().context("waiting for oxinfer")?;
    if !status.success() {
        bail!("the download failed: {tail}");
    }
    Ok(())
}

/// Whether the server described by `state` answers.
fn answers(state: &Value) -> bool {
    let (Some(port), Some(key)) = (state["port"].as_u64(), state["key"].as_str()) else {
        return false;
    };
    http_get(
        &format!("http://127.0.0.1:{port}/v1/models"),
        Some(key),
        Duration::from_secs(5),
    )
    .is_ok()
}

fn read_state(dir: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(dir.join("oxinfer.json")).ok()?).ok()
}

/// This process's lease: the shared lock that tells the supervisor the
/// server has a user. Taken before looking for a server, so one found
/// running is not shut down under us.
#[cfg(unixlike)]
fn lease(dir: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let lease = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("oxinfer.lease"))
        .context("opening the lease")?;
    lease.lock_shared().context("taking the lease")?;
    Ok(lease)
}

/// The server already running `model`, if there is one: the quick way in,
/// with nothing to install, measure or start. An error when a server is
/// running another model for somebody else.
#[cfg(unixlike)]
pub fn join(model: &Model) -> Result<Option<Local>> {
    let dir = run_dir()?;
    let lease = lease(&dir)?;
    let Some(state) = read_state(&dir).filter(answers) else {
        return Ok(None);
    };
    if state["model"] != model.spec.as_str() {
        // Another model. If nobody else holds the lease the server is only
        // waiting out its idle minute: end it and start the one wanted.
        lease.unlock().context("releasing the lease")?;
        if lease.try_lock().is_err() {
            bail!(
                "the local server is running {} for another atelier; quit that one, or \
                 use the same model",
                state["model"].as_str().unwrap_or("another model")
            );
        }
        stop(&state);
        let _ = std::fs::remove_file(dir.join("oxinfer.json"));
        return Ok(None);
    }
    local(&state, lease).map(Some)
}

#[cfg(not(unixlike))]
pub fn join(_: &Model) -> Result<Option<Local>> {
    bail!("the local model is not supported on this platform")
}

/// A server running `model`, started if there is none, and this process's
/// lease on it. `say` gets a line when one has to be started.
#[cfg(unixlike)]
pub fn connect(bin: &Path, model: &Model, say: &mut dyn FnMut(&str)) -> Result<Local> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    if let Some(running) = join(model)? {
        return Ok(running);
    }
    let dir = run_dir()?;
    let lease = lease(&dir)?;
    let usable = |state: &Value| state["model"] == model.spec.as_str() && answers(state);

    say(&format!(
        "starting {} (it is loaded on the first request)…",
        model.source()
    ));
    let exe = std::env::current_exe().context("locating the atelier executable")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join("oxinfer.log"))
        .map_or_else(|_| Stdio::null(), Stdio::from);
    let mut child = std::process::Command::new(exe)
        .arg("local-serve")
        .arg(bin)
        .arg(&model.spec)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // Its own process group: the terminal's signals don't reach it.
        .process_group(0)
        .spawn()
        .context("starting the local server's supervisor")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(state) = read_state(&dir).filter(|s| usable(s)) {
            return local(&state, lease);
        }
        if Instant::now() >= deadline {
            bail!(
                "the local server did not come up (see {})",
                dir.join("oxinfer.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(not(unixlike))]
pub fn connect(_: &Path, _: &Model, _: &mut dyn FnMut(&str)) -> Result<Local> {
    bail!("the local model is not supported on this platform")
}

fn local(state: &Value, lease: std::fs::File) -> Result<Local> {
    Ok(Local {
        base_url: format!(
            "http://127.0.0.1:{}/v1",
            state["port"]
                .as_u64()
                .context("no port in the state file")?
        ),
        key: state["key"]
            .as_str()
            .context("no key in the state file")?
            .to_string(),
        _lease: lease,
    })
}

/// `atelier local-serve <oxinfer> <model>`: the supervisor. Runs the server,
/// says where it is, and ends it once nobody has used it for [`IDLE`].
#[cfg(unixlike)]
pub fn supervise(args: &[String]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::Stdio;

    let [bin, spec] = args else {
        bail!("usage: atelier local-serve <oxinfer> <model> (started by atelier itself)");
    };
    let dir = run_dir()?;
    let state_file = dir.join("oxinfer.json");
    let lock =
        std::fs::File::create(dir.join("oxinfer.lock")).context("opening the supervisor's lock")?;
    if lock.try_lock().is_err() {
        return Ok(()); // another supervisor is at it
    }
    // A server whose supervisor was killed is still there, holding its
    // memory: end it, if it really is that server (it answers to its key).
    if let Some(old) = read_state(&dir) {
        if answers(&old)
            && let Some(pid) = old["server_pid"].as_u64()
        {
            kill(pid);
        }
        let _ = std::fs::remove_file(&state_file);
    }

    // A free port: bound, read, released for the server to take.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .context("finding a free port")?
        .port();
    let key = crate::claude::new_uuid();
    let mut server = std::process::Command::new(bin)
        .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
        .args(["--model", &format!("{MODEL_ID}={spec}")])
        // Screenshots are megabytes once in base64.
        .args(["--max-body-mb", "64"])
        // In the environment, not on the command line for `ps` to show.
        .env("OXINFER_API_KEY", &key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting {bin}"))?;

    let state = json!({
        "port": port,
        "key": key,
        "model": spec,
        "pid": std::process::id(),
        "server_pid": server.id(),
    });
    let deadline = Instant::now() + START_WAIT;
    while !answers(&state) {
        if let Some(status) = server.try_wait()? {
            bail!("oxinfer ended at once ({status})");
        }
        if Instant::now() >= deadline {
            let _ = server.kill();
            bail!("oxinfer did not start answering");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Written whole, private (it holds the key), then moved into place.
    let tmp = dir.join(format!("oxinfer.json.{}", std::process::id()));
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(state.to_string().as_bytes())?;
    }
    std::fs::rename(&tmp, &state_file)?;
    eprintln!("serving {spec} on 127.0.0.1:{port}");

    let lease = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("oxinfer.lease"))?;
    let mut unused_since = Instant::now();
    let ended = loop {
        std::thread::sleep(Duration::from_secs(2));
        if let Some(status) = server.try_wait()? {
            break format!("oxinfer ended ({status})");
        }
        // Nobody holds the lease if we can have it to ourselves.
        if lease.try_lock().is_ok() {
            let _ = lease.unlock();
            if unused_since.elapsed() >= IDLE {
                break "nobody is using it".to_string();
            }
        } else {
            unused_since = Instant::now();
        }
    };
    let _ = std::fs::remove_file(&state_file);
    let _ = server.kill();
    let _ = server.wait();
    eprintln!("stopped: {ended}");
    Ok(())
}

#[cfg(not(unixlike))]
pub fn supervise(_: &[String]) -> Result<()> {
    bail!("the local model is not supported on this platform")
}

/// End the server `state` describes, and its supervisor: the supervisor
/// first, which would otherwise see the server gone and say so.
fn stop(state: &Value) {
    for pid in [&state["pid"], &state["server_pid"]] {
        if let Some(pid) = pid.as_u64() {
            kill(pid);
        }
    }
    // The port, and the memory, are free once it no longer answers.
    let deadline = Instant::now() + Duration::from_secs(10);
    while answers(state) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn kill(pid: u64) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

/// `atelier local [status|download|stop]`: look at, prepare or end the
/// local model from a shell — `download` in particular, where the progress
/// of a long download reads better than inside a session.
pub fn run_command(args: &[String]) -> Result<()> {
    let user = crate::settings::Settings::load_user().unwrap_or_default();
    let model = Model::configured(&user);
    let mut say = |line: &str| eprintln!("{line}");
    match args.first().map(String::as_str) {
        None | Some("status") => {
            println!("model: {}", model.spec);
            let installed = std::env::var_os(ENV_BIN)
                .map(PathBuf::from)
                .or_else(|| data_dir().ok().map(|d| d.join("oxinfer")))
                .filter(|p| p.is_file());
            match &installed {
                Some(bin) => {
                    println!("oxinfer: {}", bin.display());
                    for line in plan(bin, &model).lines(&model) {
                        println!("{line}");
                    }
                }
                None => println!("oxinfer: not installed yet (`atelier local download`)"),
            }
            match run_dir().ok().and_then(|d| read_state(&d)) {
                Some(state) if answers(&state) => println!(
                    "server: running {} on 127.0.0.1:{}",
                    state["model"].as_str().unwrap_or("?"),
                    state["port"]
                ),
                _ => println!("server: not running"),
            }
        }
        Some("download") => {
            let bin = binary(&mut say)?;
            let plan = plan(&bin, &model);
            for line in plan.lines(&model) {
                eprintln!("{line}");
            }
            if plan.download_gb.is_none() && plan.memory_gb.is_some() {
                println!("{} is already on this machine", model.source());
                return Ok(());
            }
            // Here oxinfer has the terminal to itself: its own progress.
            let status = std::process::Command::new(&bin)
                .args(["--model", model.source(), "--download-only"])
                .status()
                .context("starting oxinfer")?;
            if !status.success() {
                bail!("the download failed");
            }
        }
        Some("stop") => match run_dir().ok().and_then(|d| read_state(&d)) {
            Some(state) if answers(&state) => {
                stop(&state);
                println!("stopped");
            }
            _ => println!("not running"),
        },
        Some(other) => {
            bail!("unknown local command '{other}' (usage: atelier local [status|download|stop])")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tar archive of the given `(path, type flag, content)` entries.
    fn tar(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, kind, content) in entries {
            let mut header = [0u8; 512];
            header[..path.len()].copy_from_slice(path.as_bytes());
            let size = format!("{:011o}", content.len());
            header[124..135].copy_from_slice(size.as_bytes());
            header[156] = *kind;
            out.extend_from_slice(&header);
            out.extend_from_slice(content);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.resize(out.len() + 1024, 0);
        out
    }

    #[test]
    fn untar_finds_the_program_wherever_it_is() {
        let big = vec![7u8; 1300];
        let archive = tar(&[
            ("README.md", b'0', b"read me"),
            ("oxinfer-v1/", b'5', b""),
            // A directory or a link of that name is not the program.
            ("docs/oxinfer", b'5', b""),
            ("oxinfer-v1/bin/oxinfer", b'0', &big),
            ("after", b'0', b"x"),
        ]);
        assert_eq!(untar(&archive, "oxinfer"), Some(&big[..]));
        assert_eq!(untar(&archive, "README.md"), Some(&b"read me"[..]));
        assert_eq!(untar(&archive, "missing"), None);
        // A header promising more than there is, is not followed.
        assert_eq!(untar(&archive[..512 + 3], "README.md"), None);
        assert_eq!(untar(&[], "oxinfer"), None);
    }

    #[test]
    fn the_model_is_the_users_choice_then_the_default() {
        let mut user = crate::settings::Settings::default();
        // (The environment variable is not set in the test environment.)
        assert!(std::env::var_os(ENV_MODEL).is_none());
        let model = Model::configured(&user);
        assert_eq!(model.spec, DEFAULT_MODEL);
        assert_eq!(model.source(), "Qwen/Qwen3.8-27B");
        user.local.model = Some(" google/gemma-4-E4B-it,quant=q8 ".into());
        let model = Model::configured(&user);
        assert_eq!(model.spec, "google/gemma-4-E4B-it,quant=q8");
        assert_eq!(model.source(), "google/gemma-4-E4B-it");
    }

    #[test]
    fn the_plan_says_what_matters() {
        let model = Model {
            spec: DEFAULT_MODEL.into(),
        };
        // Nothing known: nothing said.
        assert!(Plan::default().lines(&model).is_empty());
        let plan = Plan {
            download_gb: Some(55.6),
            disk_free_gb: Some(20.0),
            memory_gb: Some(19.0),
            memory_available_gb: Some(8.0),
            runs_on: Some("cpu".into()),
            tokens_per_s: Some(4.2),
        };
        let text = plan.lines(&model).join("\n");
        assert!(text.contains("a 56 GB download (20 GB free)"), "{text}");
        assert!(text.contains("more than the disk has free"), "{text}");
        assert!(text.contains("about 19 GB of memory, on the cpu at about 4 tokens/s"));
        assert!(text.contains("only 8 GB are available"), "{text}");
        // On disk, and fitting: just what it takes.
        let plan = Plan {
            download_gb: None,
            memory_gb: Some(19.0),
            memory_available_gb: Some(90.0),
            ..Plan::default()
        };
        assert_eq!(plan.lines(&model), ["it takes about 19 GB of memory"]);
    }
}
