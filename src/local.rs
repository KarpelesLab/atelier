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
//!    `$XDG_DATA_HOME/atelier/oxinfer/`, and kept up to date in the
//!    background ([`spawn_update_check`]). `ATELIER_OXINFER_BIN` names a
//!    program to use instead.
//! 2. **The weights** ([`download`]) of the model the user names, else of
//!    the one chosen for the machine ([`choose`]): tens of gigabytes, so
//!    never fetched without the user saying yes ([`Plan`] carries what to
//!    tell them). oxinfer does the fetching, into its own cache.
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
/// The model run when the user names none and nothing is known of the
/// machine: Qwen3.8 27B, which reads images as well as text and calls
/// tools, at 4 bits (about 19 GB of memory). Otherwise [`choose`] picks
/// among [`CANDIDATES`].
pub const DEFAULT_MODEL: &str = "Qwen/Qwen3.8-27B,quant=q4";
/// The name the server knows the model by, whatever it is.
pub const MODEL_ID: &str = "local";
/// Names a model to run instead of the one chosen for the machine, as
/// oxinfer writes them: `SOURCE[,KEY=VALUE...]`.
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
    /// The model the environment or the user's own settings name; `None`
    /// leaves it to [`choose`]. A project's `atelier.toml` has no say: what
    /// to download and load is the user's choice.
    pub fn configured(user: &crate::settings::Settings) -> Option<Self> {
        std::env::var(ENV_MODEL)
            .ok()
            .or_else(|| user.local.model.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|spec| Self { spec })
    }

    /// The repository, folder or file, without the options.
    pub fn source(&self) -> &str {
        self.spec.split(',').next().unwrap_or(&self.spec)
    }

    /// The entry of [`CANDIDATES`] this is, if it is one.
    fn candidate(&self) -> Option<&'static Candidate> {
        CANDIDATES.iter().find(|c| c.spec == self.spec)
    }
}

/// A model atelier may choose for the machine.
#[derive(Debug)]
pub struct Candidate {
    pub spec: &'static str,
    /// What to call it, to the user.
    pub label: &'static str,
    /// Gigabytes of memory it takes, as oxinfer estimates (its own estimate
    /// is used where it has one) …
    memory_gb: f64,
    /// … and to download.
    download_gb: f64,
    /// Only worth it on a GPU: on the CPU, which reads every weight for every
    /// token, twice the bytes is half the speed.
    gpu_only: bool,
    /// GGUF weights, which oxinfer runs on Apple's GPU but not on an NVIDIA
    /// one.
    gguf: bool,
    /// For a model oxinfer's table does not know, the specs `oxinfer models
    /// list` shows once its files are all in the cache.
    files: &'static [&'static str],
}

/// The models to choose among, the largest first: all Qwen3.8 27B, the
/// smallest model that does well as an agent (docs/local.md), at the
/// precision the machine can hold. bf16 is left out: twice the memory of
/// 8 bits, half its speed, for no difference we could see.
pub const CANDIDATES: &[Candidate] = &[
    Candidate {
        spec: "Qwen/Qwen3.8-27B,quant=q8",
        label: "Qwen3.8 27B at 8 bits",
        memory_gb: 33.4,
        download_gb: 55.6,
        gpu_only: true,
        gguf: false,
        files: &[],
    },
    Candidate {
        spec: DEFAULT_MODEL,
        label: "Qwen3.8 27B at 4 bits",
        memory_gb: 18.9,
        download_gb: 55.6,
        gpu_only: false,
        gguf: false,
        files: &[],
    },
    Candidate {
        spec: "unsloth/Qwen3.8-27B-GGUF:UD-Q3_K_XL,mmproj=unsloth/Qwen3.8-27B-GGUF/mmproj-F16.gguf",
        label: "Qwen3.8 27B at 3 bits (GGUF)",
        // The 13.1 GB file and its 0.9 GB image reader, and room to work.
        memory_gb: 15.0,
        download_gb: 14.1,
        gpu_only: false,
        gguf: true,
        // The listing names a GGUF by the last part of its file name, and
        // leaves the image reader out: it comes with the first start, in
        // the download agreed to with the weights.
        files: &["unsloth/Qwen3.8-27B-GGUF:Q3_K_XL"],
    },
];

/// What the machine has to run a model with.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hardware {
    /// Gigabytes of RAM in all.
    pub memory_gb: Option<f64>,
    /// The GPU oxinfer would use, if it found one.
    pub gpu: Option<Gpu>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    pub name: String,
    /// `cuda`, `vulkan`, `metal`, …
    pub backend: String,
    pub memory_gb: f64,
    /// Its memory is the system's (Apple silicon).
    pub unified: bool,
}

impl Hardware {
    /// What oxinfer's `--models --json` report says of the machine — it
    /// knows the GPUs it can use, from the libraries it loads — else the
    /// RAM as the system tells it, with no GPU.
    pub fn detect(report: Option<&Value>) -> Self {
        let hw = report.map(|r| &r["hardware"]);
        let gpu = hw.and_then(|hw| {
            let gpu = &hw["gpu"];
            Some(Gpu {
                name: gpu["name"].as_str().unwrap_or("GPU").to_string(),
                backend: gpu["backend"].as_str()?.to_string(),
                memory_gb: gpu["memory_gb"].as_f64()?,
                unified: gpu["unified_memory"] == true,
            })
        });
        Self {
            memory_gb: hw
                .and_then(|hw| hw["memory_gb"].as_f64())
                .or_else(system_memory_gb),
            gpu,
        }
    }
}

/// The machine's RAM in gigabytes: `/proc/meminfo` on Linux, `sysctl` on
/// macOS.
fn system_memory_gb() -> Option<f64> {
    if cfg!(any(target_os = "linux", target_os = "fullrust")) {
        meminfo_total_gb(&std::fs::read_to_string("/proc/meminfo").ok()?)
    } else if cfg!(target_os = "macos") {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        let bytes: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(bytes / 1e9)
    } else {
        None
    }
}

/// `MemTotal` of `/proc/meminfo` (in kB), in gigabytes.
fn meminfo_total_gb(meminfo: &str) -> Option<f64> {
    let kb: f64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024.0 / 1e9)
}

/// The share of the RAM a model may take and still be held in memory:
/// oxinfer streams weights from storage beyond 60%, and the rest is the
/// headroom the system and the user's other programs need.
const RAM_SHARE: f64 = 0.6;

/// The model chosen for the machine, and why, for the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub model: Model,
    pub why: String,
}

/// The largest of [`CANDIDATES`] that fits the machine with room to spare:
/// first whole on the GPU (in 92% of its memory less 1 GB, oxinfer's own
/// rule), else in RAM ([`RAM_SHARE`]); the smallest when none does. The
/// memory each takes is oxinfer's estimate in `report` where it has one.
pub fn choose(hw: &Hardware, report: Option<&Value>) -> Choice {
    let need = |c: &Candidate| {
        let model = Model {
            spec: c.spec.to_string(),
        };
        table_variant(report, &model)
            .and_then(|v| v["memory_gb"].as_f64())
            .unwrap_or(c.memory_gb)
    };
    let ram_fits = |gb: f64| hw.memory_gb.is_some_and(|ram| gb <= ram * RAM_SHARE);
    let pick = |c: &'static Candidate, why: String| Choice {
        model: Model {
            spec: c.spec.to_string(),
        },
        why: format!(
            "{}, chosen for this machine: {why} ({ENV_MODEL} or `[local] model` runs another)",
            c.label
        ),
    };

    if let Some(gpu) = &hw.gpu {
        let room = gpu.memory_gb * 0.92 - 1.0;
        let found = CANDIDATES.iter().find(|c| {
            let gb = need(c);
            (!c.gguf || gpu.backend == "metal") && gb <= room && (!gpu.unified || ram_fits(gb))
        });
        if let Some(c) = found {
            return pick(
                c,
                format!(
                    "it takes about {:.0} GB, and the GPU ({}) has {:.0} GB",
                    need(c),
                    gpu.name,
                    gpu.memory_gb
                ),
            );
        }
    }
    let Some(ram) = hw.memory_gb else {
        let c = CANDIDATES
            .iter()
            .find(|c| c.spec == DEFAULT_MODEL)
            .expect("the default is a candidate");
        return pick(c, "how much memory this machine has is not known".into());
    };
    let gpu_note = if hw.gpu.is_some() {
        " (the GPU's is too small for it)"
    } else {
        ""
    };
    if let Some(c) = CANDIDATES.iter().find(|c| !c.gpu_only && ram_fits(need(c))) {
        return pick(
            c,
            format!(
                "it takes about {:.0} GB of this machine's {ram:.0} GB, on the CPU{gpu_note}",
                need(c)
            ),
        );
    }
    let c = CANDIDATES.last().expect("there are candidates");
    pick(
        c,
        format!(
            "the smallest; it takes about {:.0} GB, too much of this machine's {ram:.0} GB to \
             run well — expect it to be slow",
            need(c)
        ),
    )
}

/// The model to run: the one the user names, else the one chosen for the
/// machine, with the reason to tell them.
pub fn resolve(
    user: &crate::settings::Settings,
    report: Option<&Value>,
) -> (Model, Option<String>) {
    match Model::configured(user) {
        Some(model) => (model, None),
        None => {
            let choice = choose(&Hardware::detect(report), report);
            (choice.model, Some(choice.why))
        }
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
                    "  only {:.0} GB are available: expect it to be very slow, or to fail \
                     ({ENV_MODEL} names another model, but smaller ones do poorly as agents)",
                    self.memory_available_gb.unwrap_or_default()
                ));
            }
        }
        out
    }
}

/// A running server this process may use, for as long as it keeps this.
pub struct Local {
    /// The model it runs.
    pub model: Model,
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
    let bin = installed_path()?;
    if bin.is_file() {
        return Ok(bin);
    }
    let api = api_base();
    if install(&api, &bin, None, say)?.is_none() {
        bail!("none of oxinfer's recent releases has an archive for this platform");
    }
    Ok(bin)
}

/// Where atelier installs oxinfer.
fn installed_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("oxinfer"))
}

/// GitHub's API, or what `ATELIER_UPDATE_API` names instead (a mirror).
fn api_base() -> String {
    std::env::var("ATELIER_UPDATE_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

/// The newest release in `releases` (GitHub's listing, newest first) with
/// an archive named for `suffix`, and newer than `than` when given: its tag
/// and that asset. A release is published before its archives are built,
/// so the newest may have none yet.
fn pick_release(
    releases: &Value,
    suffix: &str,
    than: Option<(u64, u64, u64)>,
) -> Option<(String, Value)> {
    releases.as_array()?.iter().find_map(|r| {
        let tag = r["tag_name"].as_str()?;
        if let Some(than) = than
            && crate::update::parse_version(tag).is_none_or(|v| v <= than)
        {
            return None;
        }
        let name = format!("oxinfer-{tag}-{suffix}");
        let asset = r["assets"]
            .as_array()?
            .iter()
            .find(|a| a["name"] == name.as_str())?;
        Some((tag.to_string(), asset.clone()))
    })
}

/// Download, check and unpack as `bin` the newest release of `api` that has
/// an archive for this platform — only one newer than `than`, when given.
/// `Ok(None)` when there is none; `Ok(Some(tag))` once installed.
fn install(
    api: &str,
    bin: &Path,
    than: Option<(u64, u64, u64)>,
    say: &mut dyn FnMut(&str),
) -> Result<Option<String>> {
    let suffixes = archive_suffixes();
    if suffixes.is_empty() {
        bail!(
            "oxinfer publishes no build for this platform; install it yourself and point \
             {ENV_BIN} at it"
        );
    }
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

    // Each kind of archive in turn, since the first may not run here (no
    // glibc).
    let mut last = None;
    for suffix in suffixes {
        let Some((tag, asset)) = pick_release(&releases, suffix, than) else {
            continue;
        };
        say(&format!(
            "downloading oxinfer {tag} ({:.0} MB)…",
            asset["size"].as_f64().unwrap_or(0.0) / 1e6
        ));
        match fetch_into(bin, &asset) {
            Ok(()) => {
                say(&format!("installed {}", bin.display()));
                return Ok(Some(tag));
            }
            Err(e) => last = Some(e.context(format!("oxinfer {tag} ({suffix})"))),
        }
    }
    match last {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// The version an oxinfer executable says it is (`oxinfer 0.0.14`).
fn version_of(bin: &Path) -> Option<(u64, u64, u64)> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_version_line(&String::from_utf8_lossy(&out.stdout))
}

fn parse_version_line(text: &str) -> Option<(u64, u64, u64)> {
    let version = text.trim().strip_prefix("oxinfer ")?;
    crate::update::parse_version(version.split_whitespace().next()?)
}

/// What an update of the installed oxinfer came to.
#[derive(Debug, PartialEq)]
enum Updated {
    UpToDate,
    Installed { from: String, to: String },
}

/// Replace `bin` with the newest release, if it is newer than `bin` says
/// it is. The new program is written beside it, checked and test-run, then
/// renamed over it: a server running the old one keeps running it — the
/// kernel holds on to a file a process runs — and the next server started
/// is the new one. So nothing in use is stopped or broken, and nothing has
/// to wait for the server to be idle.
fn update(api: &str, bin: &Path, say: &mut dyn FnMut(&str)) -> Result<Updated> {
    let from = version_of(bin).context("the installed oxinfer does not say its version")?;
    match install(api, bin, Some(from), say)? {
        None => Ok(Updated::UpToDate),
        Some(tag) => Ok(Updated::Installed {
            from: format!("{}.{}.{}", from.0, from.1, from.2),
            to: tag.trim_start_matches('v').to_string(),
        }),
    }
}

/// The time of the last check, in atelier's state directory; checks are a
/// day apart, as for atelier's own.
const UPDATE_STAMP: &str = "last-oxinfer-check";

/// Whether to look for a newer oxinfer in the background: not when the user
/// turned updates off (`ATELIER_NO_UPDATE`), nor for a program they chose
/// themselves (`ATELIER_OXINFER_BIN`), nor before one is installed, nor
/// more than once a day.
fn update_wanted(turned_off: bool, own_binary: bool, installed: bool, due: bool) -> bool {
    !turned_off && !own_binary && installed && due
}

/// Check, at most once a day and in the background, for a newer oxinfer
/// than the one installed, and install it ([`update`]); the interface is
/// told when one was. Failures stay quiet, as for atelier's own update.
pub fn spawn_update_check() {
    let own = std::env::var_os(ENV_BIN).is_some_and(|v| !v.is_empty());
    let Ok(bin) = installed_path() else { return };
    let Some(stamp) = crate::update::state_file(UPDATE_STAMP) else {
        return;
    };
    let due = crate::update::due(&stamp, std::time::SystemTime::now());
    if !update_wanted(crate::update::turned_off(), own, bin.is_file(), due) {
        return;
    }
    // Recorded before, so that the sessions starting together don't all go.
    let _ = crate::update::record_check(&stamp, std::time::SystemTime::now());
    std::thread::spawn(move || {
        if let Ok(Updated::Installed { from, to }) = update(&api_base(), &bin, &mut |_| {}) {
            crate::update::set_notice(format!(
                "oxinfer {to} installed for the local model (a server running now keeps \
                 {from} until it stops)"
            ));
        }
    });
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
        let got = crate::update::hex(&purecrypto::hash::sha256(&archive));
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

/// oxinfer's report on the machine and the models of its table
/// (`--models --json`): the hardware, and for each model what it would take
/// and how fast it would run. It measures the CPU, so it takes seconds.
pub fn report(bin: &Path) -> Option<Value> {
    let out = std::process::Command::new(bin)
        .args(["--models", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// The report's entry for `model`.
fn table_entry<'a>(report: Option<&'a Value>, model: &Model) -> Option<&'a Value> {
    report?["models"]
        .as_array()?
        .iter()
        .find(|m| m["model"] == model.source())
}

/// The report's variant of `model`: the one its options ask for (`quant=q4`
/// is listed as `--quant q4`), else the one oxinfer recommends.
fn table_variant<'a>(report: Option<&'a Value>, model: &Model) -> Option<&'a Value> {
    let variants = table_entry(report, model)?["variants"].as_array()?;
    let quant = model
        .spec
        .split(',')
        .find_map(|o| o.strip_prefix("quant="))
        .map(|q| format!("--quant {q}"));
    variants
        .iter()
        .find(|v| quant.as_deref().is_some_and(|q| v["args"] == q))
        .or_else(|| variants.iter().find(|v| v["recommended"] == true))
}

/// What `model` would take on this machine, from oxinfer's `report`. A
/// model its table does not know is one of [`CANDIDATES`] (whose numbers
/// atelier has, and whose files `oxinfer models list` shows once they are
/// here) or the user's own choice — a local folder, a repository nobody
/// measured — which gives an empty plan: oxinfer fetches and reports as it
/// goes.
pub fn plan(bin: &Path, report: Option<&Value>, model: &Model) -> Plan {
    let cached = || {
        let out = std::process::Command::new(bin)
            .args(["models", "list", "--json"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        serde_json::from_slice::<Value>(&out.stdout).ok()
    };
    plan_from(report, model, cached)
}

fn plan_from(
    report: Option<&Value>,
    model: &Model,
    cached: impl FnOnce() -> Option<Value>,
) -> Plan {
    let hardware = report.map(|r| &r["hardware"]);
    let machine = Plan {
        disk_free_gb: hardware.and_then(|h| h["disk_free_gb"].as_f64()),
        memory_available_gb: hardware.and_then(|h| h["memory_available_gb"].as_f64()),
        ..Plan::default()
    };
    if let Some(entry) = table_entry(report, model) {
        let variant = table_variant(report, model);
        return Plan {
            download_gb: (entry["on_disk"] != true)
                .then(|| entry["download_gb"].as_f64())
                .flatten(),
            memory_gb: variant.and_then(|v| v["memory_gb"].as_f64()),
            runs_on: variant
                .and_then(|v| v["runs_on"].as_str())
                .map(str::to_string),
            tokens_per_s: variant.and_then(|v| v["decode_tok_s"].as_f64()),
            ..machine
        };
    }
    let Some(c) = model.candidate().filter(|c| !c.files.is_empty()) else {
        return Plan::default();
    };
    // Here when the cache shows every file; when it cannot be asked, it is
    // not known to be here, and asking first is the safe side.
    let listed = cached();
    let have: Vec<&str> = listed
        .as_ref()
        .and_then(|l| l["entries"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|e| e["specs"].as_array())
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let here = c.files.iter().all(|f| have.contains(f));
    Plan {
        download_gb: (!here).then_some(c.download_gb),
        memory_gb: Some(c.memory_gb),
        ..machine
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

/// Whether a server running `running` serves a session that wants `wanted`
/// (`None`: whatever atelier chose for the machine, which is one of
/// [`CANDIDATES`]).
fn serves(running: &str, wanted: Option<&Model>) -> bool {
    match wanted {
        Some(model) => running == model.spec,
        None => CANDIDATES.iter().any(|c| c.spec == running),
    }
}

/// The server already running `model` (`None`: one atelier chose for the
/// machine), if there is one: the quick way in, with nothing to install,
/// measure or start. An error when a server is running another model for
/// somebody else.
#[cfg(unixlike)]
pub fn join(model: Option<&Model>) -> Result<Option<Local>> {
    let dir = run_dir()?;
    let lease = lease(&dir)?;
    let Some(state) = read_state(&dir).filter(answers) else {
        return Ok(None);
    };
    if !serves(state["model"].as_str().unwrap_or_default(), model) {
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
pub fn join(_: Option<&Model>) -> Result<Option<Local>> {
    bail!("the local model is not supported on this platform")
}

/// A server running `model`, started if there is none, and this process's
/// lease on it. `say` gets a line when one has to be started.
#[cfg(unixlike)]
pub fn connect(bin: &Path, model: &Model, say: &mut dyn FnMut(&str)) -> Result<Local> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    if let Some(running) = join(Some(model))? {
        return Ok(running);
    }
    let dir = run_dir()?;
    let lease = lease(&dir)?;
    let usable = |state: &Value| state["model"] == model.spec.as_str() && answers(state);

    say(&format!(
        "starting {} (it is loaded on the first request)…",
        model.source()
    ));
    let exe = crate::app::helper_exe().context("locating the atelier executable")?;
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
        model: Model {
            spec: state["model"].as_str().unwrap_or_default().to_string(),
        },
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

/// `atelier local [status|download|update|stop]`: look at, prepare, update
/// or end the local model from a shell — `download` in particular, where
/// the progress of a long download reads better than inside a session.
pub fn run_command(args: &[String]) -> Result<()> {
    let user = crate::settings::Settings::load_user().unwrap_or_default();
    let mut say = |line: &str| eprintln!("{line}");
    match args.first().map(String::as_str) {
        None | Some("status") => {
            let installed = std::env::var_os(ENV_BIN)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| installed_path().ok())
                .filter(|p| p.is_file());
            let report = installed.as_deref().and_then(report);
            let (model, why) = resolve(&user, report.as_ref());
            println!("model: {}", model.spec);
            if let Some(why) = why {
                println!("  {why}");
            }
            match &installed {
                Some(bin) => {
                    let version = version_of(bin)
                        .map(|(a, b, c)| format!(" ({a}.{b}.{c})"))
                        .unwrap_or_default();
                    println!("oxinfer: {}{version}", bin.display());
                    for line in plan(bin, report.as_ref(), &model).lines(&model) {
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
            let report = report(&bin);
            let (model, why) = resolve(&user, report.as_ref());
            if let Some(why) = why {
                eprintln!("{why}");
            }
            let plan = plan(&bin, report.as_ref(), &model);
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
        Some("update") => {
            if std::env::var_os(ENV_BIN).is_some_and(|v| !v.is_empty()) {
                bail!("{ENV_BIN} names an oxinfer of your own: atelier leaves it alone");
            }
            let bin = installed_path()?;
            if !bin.is_file() {
                binary(&mut say)?;
                return Ok(());
            }
            let outcome = update(&api_base(), &bin, &mut say)?;
            if let Some(stamp) = crate::update::state_file(UPDATE_STAMP) {
                let _ = crate::update::record_check(&stamp, std::time::SystemTime::now());
            }
            match outcome {
                Updated::UpToDate => println!("oxinfer is up to date"),
                Updated::Installed { from, to } => {
                    println!("updated oxinfer {from} → {to}");
                    if run_dir()
                        .ok()
                        .and_then(|d| read_state(&d))
                        .is_some_and(|s| answers(&s))
                    {
                        println!(
                            "the server running now keeps {from} until its sessions are done; \
                             the next one started is {to}"
                        );
                    }
                }
            }
        }
        Some(other) => {
            bail!(
                "unknown local command '{other}' (usage: atelier local \
                 [status|download|update|stop])"
            )
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
    fn the_model_is_the_users_choice_then_the_machines() {
        let mut user = crate::settings::Settings::default();
        // (The environment variable is not set in the test environment.)
        assert!(std::env::var_os(ENV_MODEL).is_none());
        assert_eq!(Model::configured(&user), None);
        user.local.model = Some("  ".into());
        assert_eq!(Model::configured(&user), None);
        user.local.model = Some(" google/gemma-4-E4B-it,quant=q8 ".into());
        let model = Model::configured(&user).unwrap();
        assert_eq!(model.spec, "google/gemma-4-E4B-it,quant=q8");
        assert_eq!(model.source(), "google/gemma-4-E4B-it");
        // Named, it is run as named, with nothing to explain.
        let (model, why) = resolve(&user, None);
        assert_eq!(model.spec, "google/gemma-4-E4B-it,quant=q8");
        assert_eq!(why, None);
        assert_eq!(
            Model {
                spec: DEFAULT_MODEL.into()
            }
            .source(),
            "Qwen/Qwen3.8-27B"
        );
    }

    fn gpu(backend: &str, memory_gb: f64, unified: bool) -> Option<Gpu> {
        Some(Gpu {
            name: "Test GPU".into(),
            backend: backend.into(),
            memory_gb,
            unified,
        })
    }

    #[test]
    fn the_largest_model_that_fits_is_chosen() {
        let q8 = CANDIDATES[0].spec;
        let q4 = CANDIDATES[1].spec;
        let q3 = CANDIDATES[2].spec;
        assert_eq!(q4, DEFAULT_MODEL);
        /// A machine (its name, RAM and GPU), the model it gets, and a
        /// part of why.
        type Case = (
            &'static str,
            Option<f64>,
            Option<Gpu>,
            &'static str,
            &'static str,
        );
        let cases: &[Case] = &[
            // A workstation GPU holds the 8 bits.
            (
                "102 GB GPU",
                Some(270.0),
                gpu("cuda", 102.0, false),
                q8,
                "GPU (Test GPU) has 102 GB",
            ),
            // A 24 GB card the 4 bits, on the GPU still.
            (
                "24 GB GPU",
                Some(64.0),
                gpu("cuda", 24.0, false),
                q4,
                "GPU (Test GPU) has 24 GB",
            ),
            // A 16 GB card nothing: the CPU, where 8 bits would be half as fast,
            // and where GGUF weights would run too (not on an NVIDIA GPU).
            (
                "16 GB GPU",
                Some(134.0),
                gpu("cuda", 16.0, false),
                q4,
                "the GPU's is too small",
            ),
            (
                "vulkan",
                Some(64.0),
                gpu("vulkan", 15.0, false),
                q4,
                "on the CPU",
            ),
            // Apple silicon: the GPU's memory is the RAM, and both must hold it.
            ("64 GB Mac", Some(64.0), gpu("metal", 48.0, true), q8, "GPU"),
            ("36 GB Mac", Some(36.0), gpu("metal", 27.0, true), q4, "GPU"),
            ("32 GB Mac", Some(32.0), gpu("metal", 24.0, true), q4, "GPU"),
            // where the GGUF runs on the GPU too.
            ("28 GB Mac", Some(28.0), gpu("metal", 21.0, true), q3, "GPU"),
            (
                "24 GB Mac",
                Some(24.0),
                gpu("metal", 17.0, true),
                q3,
                "expect it to be slow",
            ),
            // No GPU: what the RAM holds with room to spare.
            (
                "32 GB",
                Some(32.0),
                None,
                q4,
                "about 19 GB of this machine's 32 GB, on the CPU",
            ),
            (
                "28 GB",
                Some(28.0),
                None,
                q3,
                "about 15 GB of this machine's 28 GB",
            ),
            // Too little for any: the smallest, and a warning.
            ("16 GB", Some(16.0), None, q3, "expect it to be slow"),
            (
                "8 GB GPU",
                Some(16.0),
                gpu("cuda", 8.0, false),
                q3,
                "expect it to be slow",
            ),
            // Nothing known: the default.
            ("unknown", None, None, q4, "not known"),
        ];
        for (name, memory_gb, gpu, want, says) in cases {
            let hw = Hardware {
                memory_gb: *memory_gb,
                gpu: gpu.clone(),
            };
            let choice = choose(&hw, None);
            assert_eq!(choice.model.spec, *want, "{name}: {}", choice.why);
            assert!(choice.why.contains(says), "{name}: {}", choice.why);
            // And always how to run another.
            assert!(choice.why.contains(ENV_MODEL), "{name}: {}", choice.why);
        }
    }

    /// A report as `oxinfer --models --json` writes it, cut to what is read.
    fn fake_report() -> Value {
        json!({
            "hardware": {
                "memory_gb": 64.0,
                "memory_available_gb": 50.0,
                "disk_free_gb": 500.0,
                "gpu": {
                    "backend": "cuda",
                    "name": "RTX Test",
                    "memory_gb": 40.0,
                    "unified_memory": false
                }
            },
            "models": [{
                "model": "Qwen/Qwen3.8-27B",
                "download_gb": 55.6,
                "on_disk": false,
                "variants": [
                    {"args": "", "memory_gb": 55.6, "runs_on": "cpu", "recommended": true},
                    {"args": "--quant q8", "memory_gb": 38.0, "runs_on": "cpu",
                     "decode_tok_s": 3.0},
                    {"args": "--quant q4", "memory_gb": 18.9, "runs_on": "gpu",
                     "decode_tok_s": 30.0}
                ]
            }]
        })
    }

    #[test]
    fn oxinfer_s_report_tells_the_machine_and_the_sizes() {
        let report = fake_report();
        let hw = Hardware::detect(Some(&report));
        assert_eq!(hw.memory_gb, Some(64.0));
        assert_eq!(hw.gpu, {
            let mut g = gpu("cuda", 40.0, false);
            g.as_mut().unwrap().name = "RTX Test".into();
            g
        });
        // By atelier's table the 8 bits would fit a 40 GB GPU; oxinfer's own
        // estimate says not, and wins.
        assert_eq!(choose(&hw, None).model.spec, CANDIDATES[0].spec);
        let choice = choose(&hw, Some(&report));
        assert_eq!(choice.model.spec, DEFAULT_MODEL, "{}", choice.why);
        assert!(choice.why.contains("GPU (RTX Test) has 40 GB"));

        // A report without a GPU leaves none; without the RAM, the system says.
        let bare = json!({"hardware": {"memory_gb": 12.0}, "models": []});
        assert_eq!(
            Hardware::detect(Some(&bare)),
            Hardware {
                memory_gb: Some(12.0),
                gpu: None
            }
        );
        assert_eq!(Hardware::detect(None).gpu, None);
        assert_eq!(
            meminfo_total_gb("MemTotal:       263208452 kB\nMemFree: 1 kB\n"),
            Some(263208452.0 * 1024.0 / 1e9)
        );
        assert_eq!(meminfo_total_gb("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn the_plan_comes_from_the_table_else_from_atelier_s_own() {
        let report = fake_report();
        let never = || -> Option<Value> { panic!("the cache need not be asked") };
        // In oxinfer's table: its numbers, for the variant asked for.
        let q8 = Model {
            spec: CANDIDATES[0].spec.into(),
        };
        let plan = plan_from(Some(&report), &q8, never);
        assert_eq!(plan.download_gb, Some(55.6));
        assert_eq!(plan.memory_gb, Some(38.0));
        assert_eq!(plan.runs_on.as_deref(), Some("cpu"));
        assert_eq!(plan.disk_free_gb, Some(500.0));

        // The GGUF is not in it: atelier's size, unless the cache has it — so
        // it is asked about before being downloaded, like the others.
        let q3 = Model {
            spec: CANDIDATES[2].spec.into(),
        };
        let plan = plan_from(Some(&report), &q3, || Some(json!({"entries": []})));
        assert_eq!(plan.download_gb, Some(14.1));
        assert_eq!(plan.memory_gb, Some(15.0));
        assert_eq!(plan.memory_available_gb, Some(50.0));
        assert!(plan.lines(&q3)[0].contains("a 14 GB download (500 GB free)"));
        let plan = plan_from(None, &q3, || None);
        assert_eq!(plan.download_gb, Some(14.1));
        let cached = json!({"entries": [{
            "repo": "unsloth/Qwen3.8-27B-GGUF",
            "specs": ["unsloth/Qwen3.8-27B-GGUF:Q3_K_XL", "unsloth/Qwen3.8-27B-GGUF:Q4_0"]
        }]});
        let plan = plan_from(Some(&report), &q3, || Some(cached));
        assert_eq!(plan.download_gb, None);
        assert_eq!(plan.memory_gb, Some(15.0));

        // Anything else oxinfer does not know: nothing to say.
        let other = Model {
            spec: "/models/mine".into(),
        };
        assert_eq!(plan_from(Some(&report), &other, never).memory_gb, None);
    }

    #[test]
    fn a_running_server_serves_who_wants_its_model() {
        let q4 = Model {
            spec: DEFAULT_MODEL.into(),
        };
        assert!(serves(DEFAULT_MODEL, Some(&q4)));
        assert!(!serves(CANDIDATES[0].spec, Some(&q4)));
        // Wanting what atelier chooses, any of its choices will do — not a
        // model somebody named.
        assert!(serves(CANDIDATES[0].spec, None));
        assert!(serves(CANDIDATES[2].spec, None));
        assert!(!serves("google/gemma-4-E4B-it", None));
    }

    fn release(tag: &str, suffixes: &[&str]) -> Value {
        json!({
            "tag_name": tag,
            "assets": suffixes.iter().map(|s| json!({
                "name": format!("oxinfer-{tag}-{s}"),
                "size": 1,
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn the_release_is_the_newest_with_an_archive() {
        let releases = json!([
            release("v0.0.15", &[]), // published, not built yet
            release("v0.0.14", &["linux-x86_64.tar.gz"]),
            release(
                "v0.0.13",
                &["linux-x86_64.tar.gz", "macos-universal.tar.gz"]
            ),
            release("nightly", &["macos-universal.tar.gz"]),
        ]);
        let tag = |suffix, than| pick_release(&releases, suffix, than).map(|(t, _)| t);
        assert_eq!(tag("linux-x86_64.tar.gz", None).as_deref(), Some("v0.0.14"));
        assert_eq!(
            tag("macos-universal.tar.gz", None).as_deref(),
            Some("v0.0.13")
        );
        // To update: only newer than what is installed, and only by version.
        assert_eq!(
            tag("linux-x86_64.tar.gz", Some((0, 0, 13))).as_deref(),
            Some("v0.0.14")
        );
        assert_eq!(tag("linux-x86_64.tar.gz", Some((0, 0, 14))), None);
        assert_eq!(tag("macos-universal.tar.gz", Some((0, 0, 13))), None);
        assert_eq!(tag("windows-x86_64.zip", None), None);
        let (_, asset) = pick_release(&releases, "linux-x86_64.tar.gz", None).unwrap();
        assert_eq!(asset["name"], "oxinfer-v0.0.14-linux-x86_64.tar.gz");

        assert_eq!(parse_version_line("oxinfer 0.0.14\n"), Some((0, 0, 14)));
        assert_eq!(parse_version_line("oxinfer 1.2.3 (abc)"), Some((1, 2, 3)));
        assert_eq!(parse_version_line("something 0.0.14"), None);
        assert_eq!(parse_version_line(""), None);
    }

    #[test]
    fn the_background_update_asks_leave() {
        // Off, a program of the user's own, nothing installed, checked today:
        // each is enough not to look.
        assert!(update_wanted(false, false, true, true));
        assert!(!update_wanted(true, false, true, true));
        assert!(!update_wanted(false, true, true, true));
        assert!(!update_wanted(false, false, false, true));
        assert!(!update_wanted(false, false, true, false));
    }

    /// Serves the files `make` gives for its base URL (path prefix → body)
    /// on localhost, to as many requests as come, counting them; hands back
    /// the base URL.
    fn serve(
        make: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files = make(&base);
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (status, body) = match files.iter().find(|(p, _)| path.starts_with(p)) {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None => ("404 Not Found", b"missing".to_vec()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        (base, hits)
    }

    /// A stand-in oxinfer: says its version, and that it can serve.
    fn fake_oxinfer(version: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo \"oxinfer {version}\"; \
             else echo 'usage: oxinfer serve [options]'; fi\n"
        )
        .into_bytes()
    }

    #[cfg(unixlike)]
    #[test]
    fn update_replaces_an_older_oxinfer_with_a_checked_newer_one() {
        use std::os::unix::fs::PermissionsExt;
        let Some(suffix) = archive_suffixes().first() else {
            return; // no release archive for this platform to stand in for
        };
        let dir = std::env::temp_dir().join(format!("atelier-oxinfer-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("oxinfer");
        std::fs::write(&bin, fake_oxinfer("0.0.14")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let new = fake_oxinfer("0.0.15");
        let archive = compcol::vec::compress_to_vec::<compcol::gzip::Gzip>(&tar(&[(
            "oxinfer-v0.0.15/oxinfer",
            b'0',
            &new,
        )]))
        .unwrap();
        let name = format!("oxinfer-v0.0.15-{suffix}");
        // A release server listing 0.0.15 (with `digest`) and the 0.0.14
        // installed, and serving the archive.
        let releases = |digest: String| {
            let (name, archive) = (name.clone(), archive.clone());
            move |base: &str| {
                let listing = json!([
                    {"tag_name": "v0.0.15", "assets": [{
                        "name": name,
                        "size": archive.len(),
                        "digest": format!("sha256:{digest}"),
                        "browser_download_url": format!("{base}/dl/{name}"),
                    }]},
                    release("v0.0.14", &[suffix]),
                ]);
                vec![
                    (
                        format!("/repos/{REPO}/releases"),
                        listing.to_string().into_bytes(),
                    ),
                    (format!("/dl/{name}"), archive),
                ]
            }
        };
        let digest = crate::update::hex(&purecrypto::hash::sha256(&archive));

        // A digest that does not match: refused, the old one left in place.
        let (bad, _) = serve(releases("0".repeat(64)));
        let err = update(&bad, &bin, &mut |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(version_of(&bin), Some((0, 0, 14)));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // The right one: installed, and nothing left beside it.
        let (good, _) = serve(releases(digest.clone()));
        let mut said = Vec::new();
        let outcome = update(&good, &bin, &mut |l| said.push(l.to_string())).unwrap();
        assert_eq!(
            outcome,
            Updated::Installed {
                from: "0.0.14".into(),
                to: "0.0.15".into()
            }
        );
        assert_eq!(version_of(&bin), Some((0, 0, 15)));
        assert!(
            said[0].starts_with("downloading oxinfer v0.0.15"),
            "{said:?}"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // Up to date now: only the listing is fetched.
        let (again, hits) = serve(releases(digest));
        assert_eq!(
            update(&again, &bin, &mut |_| {}).unwrap(),
            Updated::UpToDate
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
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
