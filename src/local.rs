//! A model on this machine: atelier runs [oxinfer] for the user who has no
//! endpoint, key or subscription to point it at.
//!
//! oxinfer is not linked in. It is a separate program, taken from its GitHub
//! releases the first time it is needed and run as `oxinfer serve`, an
//! OpenAI-compatible server on a loopback port — so the rest of atelier
//! talks to it like to any other provider (it becomes the provider named
//! `local`).
//!
//! Four things happen on the way to the first answer:
//!
//! 1. **The program** ([`binary`]): the newest release that has an archive
//!    for this platform is downloaded, checked (its size, and its SHA-256
//!    where GitHub lists one), unpacked and test-run, into
//!    `$XDG_DATA_HOME/atelier/oxinfer/`. That is the one time atelier
//!    installs it: from then on oxinfer updates itself (`oxinfer --update`),
//!    when atelier asks it to ([`make_current`]). `ATELIER_OXINFER_BIN`
//!    names a program to use instead.
//! 2. **The server** ([`server`]): one per user, run by the coordinator
//!    (see [`crate::coord::infer`]) for as long as an atelier uses it, and
//!    started with no model at all.
//! 3. **The weights** ([`Server::download`]) of the model the user names,
//!    else of the one chosen for the machine ([`choose`]): tens of
//!    gigabytes, so never fetched without the user saying yes ([`Plan`]
//!    carries what to tell them). The server does the fetching, into
//!    oxinfer's cache.
//! 4. **The model** ([`Server::load`]): each atelier has the server load
//!    its own, under a name made from what it is ([`Model::id`]) — so two
//!    sessions that want the same model share it, and two that want
//!    different ones are both served, oxinfer unloading the one used least
//!    recently when its memory budget is short.
//!
//! Without the coordinator (`ATELIER_NO_COORDINATOR`) there is nobody to
//! run the server, and the local model is not available.
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
/// The oldest oxinfer that will do: the first whose server starts with no
/// model and takes them through its management API.
pub const MIN_VERSION: (u64, u64, u64) = (0, 0, 16);
/// Names a model to run instead of the one chosen for the machine, as
/// oxinfer writes them: `SOURCE[,KEY=VALUE...]`.
pub const ENV_MODEL: &str = "ATELIER_LOCAL_MODEL";
/// Names an oxinfer executable to use instead of a downloaded one.
pub const ENV_BIN: &str = "ATELIER_OXINFER_BIN";
/// `1` agrees to the weights download where nobody can be asked.
pub const ENV_ACCEPT: &str = "ATELIER_LOCAL_ACCEPT";
/// How many seconds the server is kept with nobody using it (a minute when
/// unset): read by the coordinator when it starts.
pub const ENV_IDLE: &str = "ATELIER_LOCAL_IDLE";

/// The release archives are tens of megabytes.
const DOWNLOAD_TIME: Duration = Duration::from_secs(900);
/// How long oxinfer gets to look for a newer release of itself when the
/// one installed would do.
const UPDATE_TIME: Duration = Duration::from_secs(90);

/// The model to run: the source oxinfer loads, and its options.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// `SOURCE[,KEY=VALUE...]`, as `oxinfer serve --model` takes it.
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

    /// The options, without the source: `KEY=VALUE[,KEY=VALUE...]`.
    pub fn options(&self) -> &str {
        self.spec.split_once(',').map_or("", |(_, options)| options)
    }

    /// The name the server knows this model by: what it is, in the letters
    /// a model name may have (`provider/model#effort` gives `/` and `#` a
    /// meaning), and a few digits of the whole specification's hash, so
    /// that two specifications never share a name. The same model asked for
    /// by two ateliers has the same name, and is loaded once.
    pub fn id(&self) -> String {
        let source = self.source().trim_end_matches('/');
        let last = source.rsplit('/').next().unwrap_or(source);
        let readable: String = std::iter::once(last)
            .chain(
                self.options()
                    .split(',')
                    .filter(|o| !o.is_empty())
                    .map(|o| {
                        // `quant=q4` reads as `q4`, a file as its name; the rest
                        // is in the hash.
                        let value = o.split_once('=').map_or(o, |(_, value)| value);
                        value.rsplit('/').next().unwrap_or(value)
                    }),
            )
            .map(|part| {
                let part: String = part
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                            c
                        } else {
                            '-'
                        }
                    })
                    .take(40)
                    .collect();
                part
            })
            .collect::<Vec<_>>()
            .join("-");
        let hash = crate::update::hex(&purecrypto::hash::sha256(self.spec.as_bytes()));
        let readable: String = readable.chars().take(56).collect();
        format!("{}-{}", readable.trim_matches('-'), &hash[..6])
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

/// A number of gigabytes as it is said: whole from ten on, to a tenth
/// below (a small model's download is not "0 GB").
pub fn gigabytes(gb: f64) -> String {
    if gb >= 9.95 {
        format!("{gb:.0}")
    } else {
        format!("{gb:.1}")
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
                "{} is not on this machine yet: a {} GB download{free}",
                model.source(),
                gigabytes(gb)
            ));
            if self.disk_free_gb.is_some_and(|free| free < gb) {
                out.push("  that is more than the disk has free".to_string());
            }
        }
        if let Some(need) = self.memory_gb {
            // Where oxinfer says it would run: a device, or how the
            // weights are held when it names none.
            let place = match self.runs_on.as_deref() {
                Some("memory") => ", held in memory".to_string(),
                Some("streamed") => ", streamed from storage".to_string(),
                Some(on) => format!(", on the {on}"),
                None => String::new(),
            };
            let speed = match self.tokens_per_s {
                Some(t) if self.runs_on.is_some() => format!(" at about {t:.0} tokens/s"),
                _ => String::new(),
            };
            out.push(format!(
                "it takes about {} GB of memory{place}{speed}",
                gigabytes(need)
            ));
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

/// The model a session is served by, for as long as it keeps this.
pub struct Local {
    /// The model, and the name the server knows it by.
    pub model: Model,
    pub id: String,
    /// `http://127.0.0.1:<port>/v1`.
    pub base_url: String,
    pub key: String,
    /// What tells the coordinator the server has a user.
    #[cfg(unixlike)]
    _lease: crate::coord::infer::Lease,
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

/// Where the coordinator is, and with it the server's record and log.
#[cfg(unixlike)]
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
    if install(&api, &bin, say)?.is_none() {
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
/// an archive named for `suffix`: its tag and that asset. A release is
/// published before its archives are built, so the newest may have none yet.
fn pick_release(releases: &Value, suffix: &str) -> Option<(String, Value)> {
    releases.as_array()?.iter().find_map(|r| {
        let tag = r["tag_name"].as_str()?;
        let name = format!("oxinfer-{tag}-{suffix}");
        let asset = r["assets"]
            .as_array()?
            .iter()
            .find(|a| a["name"] == name.as_str())?;
        Some((tag.to_string(), asset.clone()))
    })
}

/// Download, check and unpack as `bin` the newest release of `api` that has
/// an archive for this platform. `Ok(None)` when there is none;
/// `Ok(Some(tag))` once installed.
fn install(api: &str, bin: &Path, say: &mut dyn FnMut(&str)) -> Result<Option<String>> {
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
        let Some((tag, asset)) = pick_release(&releases, suffix) else {
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

fn version_text(version: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

/// Whether the installed oxinfer may be brought up to date: not when the
/// user turned updates off (`ATELIER_NO_UPDATE`), nor a program they chose
/// themselves (`ATELIER_OXINFER_BIN`).
pub fn update_allowed() -> bool {
    let own = std::env::var_os(ENV_BIN).is_some_and(|v| !v.is_empty());
    !own && !crate::update::turned_off()
}

/// The time oxinfer was last asked to update itself, in atelier's state
/// directory; askings are a day apart, as atelier's own check is.
const UPDATE_STAMP: &str = "last-oxinfer-check";

/// Have the oxinfer at `bin` update itself (`oxinfer --update`: it fetches
/// its latest release, writes it beside itself and renames it into place),
/// giving it `time`. Whether it ran to a good end — up to date counts.
///
/// The rename leaves a server that runs the old file running it (the
/// kernel holds on to a file a process runs): nothing in use is disturbed,
/// and the next server started is the new one.
fn self_update(bin: &Path, time: Duration, quiet: bool) -> bool {
    use std::process::Stdio;
    let out = || {
        if quiet {
            Stdio::null()
        } else {
            Stdio::inherit()
        }
    };
    let Ok(mut child) = std::process::Command::new(bin)
        .arg("--update")
        .stdin(Stdio::null())
        .stdout(out())
        .stderr(out())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + time;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Make the oxinfer at `bin` one that can serve — [`MIN_VERSION`] or
/// newer — and say which version it is. Called before a server is started,
/// which is when a new version takes effect.
///
/// With `update` (see [`update_allowed`]) oxinfer is asked to update
/// itself: once a day at most while it would do as it is, and at once when
/// it is too old (or does not say what it is, as the oldest ones don't).
/// One too old that cannot update itself is installed afresh, the way the
/// first one was. Without `update`, one too old is an error that says so.
pub fn make_current(bin: &Path, update: bool, say: &mut dyn FnMut(&str)) -> Result<String> {
    let stamp = crate::update::state_file(UPDATE_STAMP);
    make_current_with(bin, update, stamp.as_deref(), &api_base(), say)
}

fn make_current_with(
    bin: &Path,
    update: bool,
    stamp: Option<&Path>,
    api: &str,
    say: &mut dyn FnMut(&str),
) -> Result<String> {
    let too_old = |v: Option<(u64, u64, u64)>| v.is_none_or(|v| v < MIN_VERSION);
    let before = version_of(bin);
    let mut version = before;
    if update {
        let now = std::time::SystemTime::now();
        let due = stamp.is_some_and(|s| crate::update::due(s, now));
        if too_old(version) || due {
            // Recorded before, so that it is not asked again at once should
            // it take long or fail.
            if let Some(stamp) = stamp {
                let _ = crate::update::record_check(stamp, now);
            }
            if too_old(version) {
                say("this oxinfer is too old for atelier: updating it…");
                self_update(bin, DOWNLOAD_TIME, true);
            } else {
                self_update(bin, UPDATE_TIME, true);
            }
            version = version_of(bin);
            if too_old(version) {
                // Its own update did not do it (it has none, or it failed).
                install(api, bin, say).context("installing a newer oxinfer")?;
                version = version_of(bin);
            }
            if version != before
                && let Some(now) = version
            {
                say(&format!("oxinfer updated itself to {}", version_text(now)));
            }
        }
    }
    match version {
        Some(version) if version >= MIN_VERSION => Ok(version_text(version)),
        _ => bail!(
            "{} is oxinfer {}, and atelier needs {} or newer{}",
            bin.display(),
            version.map_or_else(|| "of a version it does not say".to_string(), version_text),
            version_text(MIN_VERSION),
            if update {
                ""
            } else {
                ": atelier leaves alone a program named with ATELIER_OXINFER_BIN, and any \
                 when ATELIER_NO_UPDATE is set — `oxinfer --update` brings it up to date"
            }
        ),
    }
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

/// Whether an oxinfer server answers on the loopback `port` to `key`.
#[cfg(unixlike)]
pub(crate) fn answers(port: u16, key: &str) -> bool {
    http_get(
        &format!("http://127.0.0.1:{port}/v1/models"),
        Some(key),
        Duration::from_secs(5),
    )
    .is_ok()
}

/// The server's HTTP interface: oxinfer's management API (`/api/web/…`),
/// which lists, fetches and loads models. It is on loopback and takes the
/// key, which only the user's own ateliers are given.
#[derive(Debug, Clone)]
pub struct Api {
    port: u16,
    key: String,
}

impl Api {
    /// One request; the answer's JSON, or the error oxinfer gave.
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("http://127.0.0.1:{}{path}", self.port);
        let mut req = rsurl::Request::new(method, &url)
            .with_context(|| format!("building request for {url}"))?
            .header("authorization", &format!("Bearer {}", self.key))
            .header("accept", "application/json")
            .connect_timeout(Duration::from_secs(5))
            // The plan of a download lists a repository over the network.
            .max_time(Duration::from_secs(120));
        if let Some(body) = body {
            req = req
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let resp = req
            .send()
            .with_context(|| format!("asking the local model's server ({path})"))?;
        let answer: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        if !(200..300).contains(&resp.status) {
            match answer["error"]["message"].as_str() {
                Some(message) => bail!("{message}"),
                None => bail!("the local model's server: HTTP {} ({path})", resp.status),
            }
        }
        Ok(answer)
    }

    /// The models the server lists, as it describes them (`id`, `source`,
    /// `state`, …).
    pub fn models(&self) -> Result<Vec<Value>> {
        let listing = self.call("GET", "/api/web/models", None)?;
        Ok(listing["data"].as_array().cloned().unwrap_or_default())
    }

    fn model(&self, id: &str) -> Option<Value> {
        self.models().ok()?.into_iter().find(|m| m["id"] == id)
    }

    /// oxinfer's report on the machine and the models of its table — what
    /// `oxinfer --models --json` prints. The first answer comes before the
    /// CPU has been timed, without speeds: it is asked again for a moment.
    pub fn report(&self) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let report = self.call("GET", "/api/web/catalog", None).ok()?;
            if report["calibrated"] != false || Instant::now() >= deadline {
                return Some(report);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

/// The server, and — for a session — this process's lease on it.
pub struct Server {
    pub api: Api,
    /// The version of the oxinfer that runs.
    pub version: String,
    #[cfg(unixlike)]
    lease: crate::coord::infer::Lease,
}

/// The server, started by the coordinator if none is running, with this
/// process counted among its users from here on. `say` gets a line when it
/// has to be started.
#[cfg(unixlike)]
pub fn server(bin: &Path, say: &mut dyn FnMut(&str)) -> Result<Server> {
    use crate::coord::infer;
    if crate::coord::disabled() {
        bail!(
            "the local model's server is run by the coordinator, which {} switches off: unset \
             it to use the local model",
            crate::coord::ENV_DISABLE
        );
    }
    let dir = run_dir()?;
    let exe = crate::app::helper_exe().ok();
    let ask = infer::Ask {
        bin: std::path::absolute(bin).unwrap_or_else(|_| bin.to_path_buf()),
        update: update_allowed(),
    };
    // The coordinator sees to it before it starts a server; said here,
    // where the user is, since it is what the wait is for.
    if ask.update && running_server().is_none() && version_of(bin).is_none_or(|v| v < MIN_VERSION) {
        say("the installed oxinfer is too old for atelier: it is updated first…");
    }
    let (lease, endpoint) = infer::acquire(&dir, exe.as_deref(), &ask, say)
        .context("the local model's server did not start")?;
    Ok(Server {
        api: Api {
            port: endpoint.port,
            key: endpoint.key,
        },
        version: endpoint.version,
        lease,
    })
}

#[cfg(not(unixlike))]
pub fn server(_: &Path, _: &mut dyn FnMut(&str)) -> Result<Server> {
    bail!("the local model is not supported on this platform")
}

/// The server as the coordinator has it, without becoming a user of it and
/// without starting either: its interface, its version, and how many
/// ateliers use it. `None` when none is running.
#[cfg(unixlike)]
fn running_server() -> Option<(Api, String, u64)> {
    if crate::coord::disabled() {
        return None;
    }
    let status = crate::coord::infer::status(&run_dir().ok()?)?;
    (status["state"] == "up").then_some(())?;
    let api = Api {
        port: u16::try_from(status["port"].as_u64()?).ok()?,
        key: status["key"].as_str()?.to_string(),
    };
    let version = status["version"].as_str().unwrap_or("?").to_string();
    Some((api, version, status["leases"].as_u64().unwrap_or(0)))
}

#[cfg(not(unixlike))]
fn running_server() -> Option<(Api, String, u64)> {
    None
}

/// Whether a model the server lists is one to use as it is: any state but
/// a load that failed.
fn usable(listed: &Value) -> bool {
    listed["state"] != "error"
}

impl Server {
    /// The model the server already has that serves a session wanting
    /// `wanted` (`None`: whatever atelier chose for the machine — any of
    /// [`CANDIDATES`] will do, so that a session joins the one another
    /// started whatever the choice would be today).
    pub fn has(&self, wanted: Option<&Model>) -> Option<Model> {
        let listed = self.api.models().ok()?;
        let has = |model: &Model| {
            let id = model.id();
            listed.iter().any(|m| m["id"] == id.as_str() && usable(m))
        };
        match wanted {
            Some(model) => has(model).then(|| model.clone()),
            None => CANDIDATES
                .iter()
                .map(|c| Model {
                    spec: c.spec.to_string(),
                })
                .find(has),
        }
    }

    /// What `model` would take: the memory and the speed from oxinfer's
    /// `report`, and what is left to download from the server, which looks
    /// at the repository and at its cache. When the repository cannot be
    /// asked (no network), a model that is here is used, and one whose size
    /// is known is asked about with that; anything else is an error — what
    /// cannot be sized is not fetched.
    pub fn plan(&self, report: Option<&Value>, model: &Model) -> Result<Plan> {
        let mut plan = plan_from(report, model, || None);
        let asked = self.api.call(
            "POST",
            "/api/web/plan",
            Some(json!({ "model": model.source() })),
        );
        match asked {
            Ok(answer) => {
                let missing = answer["missing"].as_f64().unwrap_or(0.0);
                plan.download_gb = (missing > 0.0).then_some(missing / 1e9);
                if let Some(free) = answer["free"].as_f64() {
                    plan.disk_free_gb = Some(free / 1e9);
                }
                Ok(plan)
            }
            Err(_) if self.cached(report, model) => {
                plan.download_gb = None;
                Ok(plan)
            }
            Err(_) if plan.download_gb.is_some() => Ok(plan),
            Err(e) => Err(e.context(format!(
                "finding out what {} takes to download",
                model.source()
            ))),
        }
    }

    /// Whether the model's files are on this machine, as far as can be told
    /// without the network: oxinfer's table says so, or its cache lists
    /// what loads the model.
    fn cached(&self, report: Option<&Value>, model: &Model) -> bool {
        if table_entry(report, model).is_some_and(|e| e["on_disk"] == true) {
            return true;
        }
        let Ok(cache) = self.api.call("GET", "/api/web/cache", None) else {
            return false;
        };
        let have: Vec<&str> = cache["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["usable"] != false)
            .filter_map(|e| e["specs"].as_array())
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        match model.candidate().map(|c| c.files).filter(|f| !f.is_empty()) {
            Some(files) => files.iter().all(|f| have.contains(f)),
            None => have.contains(&model.source()),
        }
    }

    /// Fetch the model's weights: a download the server runs (so it goes on
    /// if this session ends, and another session asking for the same model
    /// joins it), followed here, with a line of its progress now and then.
    pub fn download(&self, model: &Model, say: &mut dyn FnMut(&str)) -> Result<()> {
        let job = self.api.call(
            "POST",
            "/api/web/downloads",
            Some(json!({ "model": model.source() })),
        )?;
        let id = job["id"]
            .as_str()
            .context("the server did not start the download")?
            .to_string();
        let mut told = Instant::now();
        let mut first = true;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let job = self
                .api
                .call("GET", &format!("/api/web/downloads/{id}"), None)
                .context("following the download")?;
            match job["status"].as_str() {
                Some("done") => return Ok(()),
                Some("error") | Some("cancelled") => bail!(
                    "the download failed: {}",
                    job["error"]
                        .as_str()
                        .or(job["status"].as_str())
                        .unwrap_or("?")
                ),
                _ => {}
            }
            let due = if first { 3 } else { 20 };
            if told.elapsed() >= Duration::from_secs(due)
                && let Some(line) = progress(&job)
            {
                say(&line);
                told = Instant::now();
                first = false;
            }
        }
    }

    /// Have the server load `model` under its name, and make this session
    /// one it serves. The load goes on in the server; the first request
    /// waits for it. A model that cannot be loaded at all — no such
    /// repository, files that are no model — fails here, with why.
    pub fn load(self, model: &Model, say: &mut dyn FnMut(&str)) -> Result<Local> {
        let id = model.id();
        let state = |listed: &Value| listed["state"].as_str().unwrap_or_default().to_string();
        let listed = self.api.model(&id);
        if !listed.as_ref().is_some_and(usable) {
            say(&format!(
                "loading {} (the first request waits for it)…",
                model.source()
            ));
            self.api.call(
                "POST",
                "/api/web/models/load",
                Some(json!({
                    "model": model.source(),
                    "id": id,
                    "options": model.options(),
                })),
            )?;
        }
        if !listed.is_some_and(|m| matches!(state(&m).as_str(), "ready" | "busy")) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while let Some(now) = self.api.model(&id) {
                match state(&now).as_str() {
                    "error" => bail!(
                        "oxinfer could not load {}: {}",
                        model.spec,
                        now["load"]["error"]
                            .as_str()
                            .unwrap_or("it does not say why")
                    ),
                    "ready" | "busy" => break,
                    _ if Instant::now() >= deadline => break,
                    _ => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        }
        Ok(Local {
            model: model.clone(),
            id,
            base_url: format!("http://127.0.0.1:{}/v1", self.api.port),
            key: self.api.key,
            #[cfg(unixlike)]
            _lease: self.lease,
        })
    }
}

/// A download job of the server's, in a line.
fn progress(job: &Value) -> Option<String> {
    let done = job["done"].as_f64()?;
    let total = job["total"].as_f64().filter(|t| *t > 0.0)?;
    let mut line = format!(
        "downloaded {} of {} GB ({:.0}%)",
        gigabytes(done / 1e9),
        gigabytes(total / 1e9),
        done / total * 100.0
    );
    if let Some(speed) = job["speed"].as_f64().filter(|s| *s > 0.0) {
        line.push_str(&format!(", {:.0} MB/s", speed / 1e6));
    }
    if let Some(eta) = job["eta"].as_f64() {
        line.push_str(&match eta {
            s if s >= 5400.0 => format!(", about {:.0} h left", s / 3600.0),
            s if s >= 90.0 => format!(", about {:.0} min left", s / 60.0),
            s => format!(", about {s:.0} s left"),
        });
    }
    Some(line)
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
            let version = installed.as_deref().and_then(version_of);
            let server = running_server();
            // The server's word where there is one; else the program's, if
            // it is one that can say.
            let report = match &server {
                Some((api, ..)) => api.report(),
                None => installed
                    .as_deref()
                    .filter(|_| version.is_some_and(|v| v >= MIN_VERSION))
                    .and_then(report),
            };
            let (model, why) = resolve(&user, report.as_ref());
            println!("model: {}", model.spec);
            if let Some(why) = why {
                println!("  {why}");
            }
            match &installed {
                Some(bin) => {
                    match version {
                        Some(v) if v >= MIN_VERSION => {
                            println!("oxinfer: {} ({})", bin.display(), version_text(v));
                        }
                        Some(v) => println!(
                            "oxinfer: {} ({}: too old, it is updated when the server is next \
                             started)",
                            bin.display(),
                            version_text(v)
                        ),
                        None => println!(
                            "oxinfer: {} (too old to say its version: it is updated when the \
                             server is next started)",
                            bin.display()
                        ),
                    }
                    for line in plan(bin, report.as_ref(), &model).lines(&model) {
                        println!("{line}");
                    }
                }
                None => println!("oxinfer: not installed yet (`atelier local download`)"),
            }
            match &server {
                Some((api, version, users)) => {
                    println!(
                        "server: oxinfer {version} on 127.0.0.1:{}, used by {}",
                        api.port,
                        match users {
                            1 => "1 atelier".to_string(),
                            n => format!("{n} ateliers"),
                        }
                    );
                    for listed in api.models().unwrap_or_default() {
                        println!(
                            "  {} ({}): {}",
                            listed["source"].as_str().unwrap_or("?"),
                            listed["id"].as_str().unwrap_or("?"),
                            listed["state"].as_str().unwrap_or("?")
                        );
                    }
                }
                None => println!("server: not running"),
            }
        }
        Some("download") => {
            let bin = binary(&mut say)?;
            make_current(&bin, update_allowed(), &mut say)?;
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
        Some("stop") => {
            #[cfg(unixlike)]
            let stopped = !crate::coord::disabled()
                && run_dir().is_ok_and(|dir| crate::coord::infer::stop(&dir));
            #[cfg(not(unixlike))]
            let stopped = false;
            println!("{}", if stopped { "stopped" } else { "not running" });
        }
        Some("update") => {
            if std::env::var_os(ENV_BIN).is_some_and(|v| !v.is_empty()) {
                bail!("{ENV_BIN} names an oxinfer of your own: atelier leaves it alone");
            }
            let bin = installed_path()?;
            if !bin.is_file() {
                binary(&mut say)?;
                return Ok(());
            }
            // oxinfer updates itself, and says what it did.
            let before = version_of(&bin);
            let done = self_update(&bin, DOWNLOAD_TIME, false);
            if let Some(stamp) = crate::update::state_file(UPDATE_STAMP) {
                let _ = crate::update::record_check(&stamp, std::time::SystemTime::now());
            }
            let mut after = version_of(&bin);
            if after.is_none_or(|v| v < MIN_VERSION) {
                // One too old to update itself: installed afresh.
                install(&api_base(), &bin, &mut say)?;
                after = version_of(&bin);
            } else if !done {
                bail!("oxinfer could not update itself");
            }
            if after != before
                && let Some((_, running, _)) = running_server()
            {
                println!(
                    "the server running now keeps oxinfer {running} until its sessions are \
                     done; the next one started is the new version"
                );
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
    fn a_model_s_name_on_the_server_tells_it_from_any_other() {
        let model = |spec: &str| Model { spec: spec.into() };
        let q4 = model(DEFAULT_MODEL);
        assert_eq!(q4.source(), "Qwen/Qwen3.8-27B");
        assert_eq!(q4.options(), "quant=q4");
        let id = q4.id();
        // Readable, and of the letters a model name may have: no `/` (it
        // follows `local/`), no `#`, `,`, `=` or `:`.
        assert!(id.starts_with("Qwen3.8-27B-q4-"), "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "{id}"
        );
        // The same model, the same name; another precision, another.
        assert_eq!(id, model(DEFAULT_MODEL).id());
        assert_ne!(id, model(CANDIDATES[0].spec).id());
        // Two that read alike once cleaned are told apart by the hash.
        assert_ne!(model("a/b,x=1:2").id(), model("a/b,x=1/2").id());
        assert_ne!(model("org/m").id(), model("other/m").id());
        // No options, a folder, a GGUF with its image reader: all name
        // something.
        assert!(model("org/m").id().starts_with("m-"));
        assert_eq!(model("org/m").options(), "");
        assert!(model("/models/mine/").id().starts_with("mine-"));
        let gguf = model(CANDIDATES[2].spec).id();
        assert!(gguf.starts_with("Qwen3.8-27B-GGUF-UD-Q3_K_XL-"), "{gguf}");
        assert!(gguf.len() <= 64, "{gguf}");
        let ids: std::collections::BTreeSet<String> =
            CANDIDATES.iter().map(|c| model(c.spec).id()).collect();
        assert_eq!(ids.len(), CANDIDATES.len());
    }

    #[test]
    fn a_download_s_progress_reads_in_a_line() {
        let job = json!({
            "status": "downloading", "done": 12.3e9, "total": 55.6e9,
            "speed": 45.2e6, "eta": 958.0
        });
        assert_eq!(
            progress(&job).unwrap(),
            "downloaded 12 of 56 GB (22%), 45 MB/s, about 16 min left"
        );
        let small = json!({"done": 0.2e9, "total": 0.8e9, "speed": 0.0, "eta": null});
        assert_eq!(progress(&small).unwrap(), "downloaded 0.2 of 0.8 GB (25%)");
        // Still planning: nothing to say yet.
        assert_eq!(progress(&json!({"done": 0, "total": null})), None);
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
        let tag = |suffix| pick_release(&releases, suffix).map(|(t, _)| t);
        assert_eq!(tag("linux-x86_64.tar.gz").as_deref(), Some("v0.0.14"));
        assert_eq!(tag("macos-universal.tar.gz").as_deref(), Some("v0.0.13"));
        assert_eq!(tag("windows-x86_64.zip"), None);
        let (_, asset) = pick_release(&releases, "linux-x86_64.tar.gz").unwrap();
        assert_eq!(asset["name"], "oxinfer-v0.0.14-linux-x86_64.tar.gz");

        assert_eq!(parse_version_line("oxinfer 0.0.14\n"), Some((0, 0, 14)));
        assert_eq!(parse_version_line("oxinfer 1.2.3 (abc)"), Some((1, 2, 3)));
        assert_eq!(parse_version_line("something 0.0.14"), None);
        assert_eq!(parse_version_line(""), None);
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

    /// A stand-in oxinfer: says its version (or, like the oldest ones, that
    /// it does not know the option), says that it can serve, and on
    /// `--update` leaves a mark and does `on_update`.
    fn fake_oxinfer(version: Option<&str>, on_update: &str) -> Vec<u8> {
        let says = match version {
            Some(v) => format!("echo \"oxinfer {v}\""),
            None => "echo 'unknown argument --version' >&2; exit 1".to_string(),
        };
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) {says};;\n--update) echo x >> \"$0.asked\"; \
             {on_update};;\n*) echo 'usage: oxinfer serve [options]';;\nesac\n"
        )
        .into_bytes()
    }

    #[cfg(unixlike)]
    fn put(path: &Path, program: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        // Written beside and renamed in, as an update does: a file being
        // run cannot be written to.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, program).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    #[cfg(unixlike)]
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("atelier-oxinfer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// oxinfer keeps itself up to date: atelier asks it to, once a day and
    /// before a server is started, and at once when it is too old to serve.
    #[cfg(unixlike)]
    #[test]
    fn oxinfer_is_asked_to_update_itself_daily_and_when_too_old() {
        let dir = scratch("self");
        let bin = dir.join("oxinfer");
        let stamp = dir.join("stamp");
        let asked =
            || std::fs::read_to_string(dir.join("oxinfer.asked")).map_or(0, |s| s.lines().count());
        let nowhere = "http://127.0.0.1:9";
        let mut said = Vec::new();
        let run = |bin: &Path, update: bool, said: &mut Vec<String>| {
            make_current_with(bin, update, Some(&stamp), nowhere, &mut |l| {
                said.push(l.to_string())
            })
        };

        // One that will do, never asked yet: asked now, and not again today.
        put(&bin, &fake_oxinfer(Some("0.0.16"), "exit 0"));
        assert_eq!(run(&bin, true, &mut said).unwrap(), "0.0.16");
        assert_eq!(asked(), 1);
        assert_eq!(run(&bin, true, &mut said).unwrap(), "0.0.16");
        assert_eq!(asked(), 1);
        assert!(said.is_empty(), "{said:?}");
        // Not at all when atelier is to leave it alone.
        std::fs::remove_file(&stamp).unwrap();
        assert_eq!(run(&bin, false, &mut said).unwrap(), "0.0.16");
        assert_eq!(asked(), 1);
        // An update that fails (no network) leaves one that will do in use.
        put(&bin, &fake_oxinfer(Some("0.0.16"), "exit 1"));
        assert_eq!(run(&bin, true, &mut said).unwrap(), "0.0.16");
        assert_eq!(asked(), 2);

        // Too old, checked today or not: it updates itself (a new file
        // renamed over the old one), and that is said.
        put(
            &dir.join("oxinfer.next"),
            &fake_oxinfer(Some("0.0.17"), "exit 0"),
        );
        put(&bin, &fake_oxinfer(Some("0.0.10"), "mv \"$0.next\" \"$0\""));
        assert_eq!(run(&bin, true, &mut said).unwrap(), "0.0.17");
        assert_eq!(asked(), 3);
        assert!(said[0].contains("too old"), "{said:?}");
        assert!(said[1].contains("updated itself to 0.0.17"), "{said:?}");

        // Too old and not atelier's to update: an error that says what is
        // needed. So is one too old to say its version.
        put(&bin, &fake_oxinfer(Some("0.0.15"), "exit 0"));
        let err = run(&bin, false, &mut said).unwrap_err().to_string();
        assert!(err.contains("is oxinfer 0.0.15"), "{err}");
        assert!(err.contains("needs 0.0.16 or newer"), "{err}");
        assert!(err.contains("oxinfer --update"), "{err}");
        put(&bin, &fake_oxinfer(None, "exit 0"));
        let err = run(&bin, false, &mut said).unwrap_err().to_string();
        assert!(err.contains("a version it does not say"), "{err}");
        assert_eq!(asked(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One too old that cannot update itself is installed afresh, the way
    /// the first one was: the release's archive, checked.
    #[cfg(unixlike)]
    #[test]
    fn an_oxinfer_that_cannot_update_itself_is_installed_afresh() {
        let Some(suffix) = archive_suffixes().first() else {
            return; // no release archive for this platform to stand in for
        };
        let dir = scratch("afresh");
        let bin = dir.join("oxinfer");
        // It has no `--update` that works, and does not say its version.
        put(&bin, &fake_oxinfer(None, "exit 1"));

        let new = fake_oxinfer(Some("0.0.16"), "exit 0");
        let archive = compcol::vec::compress_to_vec::<compcol::gzip::Gzip>(&tar(&[(
            "oxinfer-v0.0.16/oxinfer",
            b'0',
            &new,
        )]))
        .unwrap();
        let name = format!("oxinfer-v0.0.16-{suffix}");
        // A release server listing 0.0.16 (with `digest`) and serving the
        // archive.
        let releases = |digest: String| {
            let (name, archive) = (name.clone(), archive.clone());
            move |base: &str| {
                let listing = json!([
                    {"tag_name": "v0.0.16", "assets": [{
                        "name": name,
                        "size": archive.len(),
                        "digest": format!("sha256:{digest}"),
                        "browser_download_url": format!("{base}/dl/{name}"),
                    }]},
                    release("v0.0.15", &[suffix]),
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
        let files = || {
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        // A digest that does not match: refused, the old one left in place.
        let (bad, _) = serve(releases("0".repeat(64)));
        let err = make_current_with(&bin, true, None, &bad, &mut |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(version_of(&bin), None);
        assert_eq!(files(), ["oxinfer", "oxinfer.asked"]);

        // The right one: installed, and nothing left beside it.
        let (good, _) = serve(releases(digest));
        let mut said = Vec::new();
        let version =
            make_current_with(&bin, true, None, &good, &mut |l| said.push(l.to_string())).unwrap();
        assert_eq!(version, "0.0.16");
        assert_eq!(version_of(&bin), Some((0, 0, 16)));
        assert!(said[0].contains("too old"), "{said:?}");
        assert!(
            said[1].starts_with("downloading oxinfer v0.0.16"),
            "{said:?}"
        );
        assert_eq!(files(), ["oxinfer", "oxinfer.asked"]);
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
        // A small one is not "0 GB", and "memory" is no device.
        let plan = Plan {
            download_gb: Some(0.43),
            memory_gb: Some(0.43),
            runs_on: Some("memory".into()),
            ..Plan::default()
        };
        assert_eq!(
            plan.lines(&model),
            [
                "Qwen/Qwen3.8-27B is not on this machine yet: a 0.4 GB download",
                "it takes about 0.4 GB of memory, held in memory"
            ]
        );
    }
}
