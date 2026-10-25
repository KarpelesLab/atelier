//! Self-update from GitHub releases.
//!
//! Official builds — made by the release workflow, which sets
//! `ATELIER_RELEASE_TARGET` at compile time to the asset they come from —
//! check the repository's latest release at most once a day, in the
//! background, at startup. A newer release's binary for this target is
//! downloaded, checked against the release's `SHA256SUMS`, test-run
//! (`--version`), and swapped in for the running executable with an atomic
//! rename; it takes effect on the next launch, and the interface says so
//! ([`take_notice`]). `atelier update` does the same on demand.
//!
//! Builds from source never update themselves (there's no asset to update
//! from, and replacing a dev build with a release would surprise). Set
//! `ATELIER_NO_UPDATE=1` to turn the background check off.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Where releases are published.
const REPO: &str = "KarpelesLab/atelier";
/// GitHub's API, unless `ATELIER_UPDATE_API` points elsewhere (a mirror, or
/// a test server).
const DEFAULT_API: &str = "https://api.github.com";
/// This build's release target (e.g. `x86_64-unknown-linux-musl`), set by
/// the release workflow; `None` for builds from source.
const TARGET: Option<&str> = option_env!("ATELIER_RELEASE_TARGET");
/// The version this binary is.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Minimum time between background checks.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Checksums file published with each release.
const SUMS_ASSET: &str = "SHA256SUMS";

/// A message for the user from the background check, picked up by the
/// interface.
static NOTICE: Mutex<Option<String>> = Mutex::new(None);

/// `atelier --version` text.
pub fn version_line() -> String {
    match TARGET {
        Some(t) => format!("atelier {VERSION} ({t})"),
        None => format!("atelier {VERSION} (built from source)"),
    }
}

/// The background check's message, if it has one (once).
pub fn take_notice() -> Option<String> {
    NOTICE.lock().ok()?.take()
}

/// Start the daily background check, if this build updates itself and the
/// user hasn't turned it off. Never blocks; failures stay quiet (a network
/// hiccup isn't worth interrupting anyone for).
pub fn spawn_background_check() {
    let Some(target) = TARGET else {
        return;
    };
    if std::env::var_os("ATELIER_NO_UPDATE").is_some_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    std::thread::spawn(move || {
        let stamp = stamp_path();
        if let Some(p) = &stamp
            && !due(p, SystemTime::now())
        {
            return;
        }
        let outcome = check_and_install(target);
        if let Some(p) = &stamp {
            let _ = record_check(p, SystemTime::now());
        }
        let notice = match outcome {
            Ok(Outcome::Installed { to, .. }) => Some(format!(
                "atelier {to} installed — restart atelier to use it"
            )),
            Ok(Outcome::NotWritable { version, path }) => Some(format!(
                "atelier {version} is available, but {} isn't writable; run `atelier update` \
                 with permission to replace it",
                path.display()
            )),
            Ok(Outcome::UpToDate { .. }) | Err(_) => None,
        };
        if let (Some(n), Ok(mut slot)) = (notice, NOTICE.lock()) {
            *slot = Some(n);
        }
    });
}

/// `atelier update`: check now and install if there's something newer.
pub fn run_command() -> Result<()> {
    let Some(target) = TARGET else {
        bail!(
            "this atelier was built from source ({VERSION}), so it doesn't update itself; \
             update the checkout and rebuild instead"
        );
    };
    match check_and_install(target)? {
        Outcome::UpToDate { latest } => {
            println!("atelier {VERSION} is up to date (latest release: {latest})");
        }
        Outcome::Installed { from, to, path } => {
            println!("updated atelier {from} → {to} ({})", path.display());
        }
        Outcome::NotWritable { version, path } => bail!(
            "atelier {version} is available, but {} isn't writable (try again with \
             permission to replace it)",
            path.display()
        ),
    }
    if let Some(p) = stamp_path() {
        let _ = record_check(&p, SystemTime::now());
    }
    Ok(())
}

enum Outcome {
    UpToDate {
        latest: String,
    },
    Installed {
        from: &'static str,
        to: String,
        path: PathBuf,
    },
    NotWritable {
        version: String,
        path: PathBuf,
    },
}

fn check_and_install(target: &str) -> Result<Outcome> {
    let api = std::env::var("ATELIER_UPDATE_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API.to_string());
    let release = get(&format!(
        "{}/repos/{REPO}/releases/latest",
        api.trim_end_matches('/')
    ))
    .context("checking the latest release")?;
    let release: Value = serde_json::from_slice(&release).context("parsing the release")?;
    let tag = release["tag_name"]
        .as_str()
        .context("release has no tag")?
        .to_string();
    let latest =
        parse_version(&tag).with_context(|| format!("unrecognized release tag {tag:?}"))?;
    let current = parse_version(VERSION).context("unrecognized own version")?;
    if latest <= current {
        return Ok(Outcome::UpToDate { latest: tag });
    }

    let asset_name = format!("atelier-{target}");
    let url_of = |name: &str| {
        release["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["name"] == name)
            .and_then(|a| a["browser_download_url"].as_str())
            .map(str::to_owned)
            .with_context(|| format!("release {tag} has no {name}"))
    };
    let sums = String::from_utf8(get(&url_of(SUMS_ASSET)?)?).context("reading checksums")?;
    let expected = checksum_for(&sums, &asset_name)
        .with_context(|| format!("{SUMS_ASSET} of {tag} doesn't list {asset_name}"))?;
    let binary = get(&url_of(&asset_name)?).context("downloading the new binary")?;
    let actual = hex(&purecrypto::hash::sha256(&binary));
    if actual != expected {
        bail!("checksum mismatch for {asset_name} of {tag}; not installing it");
    }

    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locating the running executable")?;
    let version = tag.trim_start_matches('v').to_string();
    match install(&exe, &binary)? {
        true => Ok(Outcome::Installed {
            from: VERSION,
            to: version,
            path: exe,
        }),
        false => Ok(Outcome::NotWritable { version, path: exe }),
    }
}

/// Put `binary` in place of `exe`: write it next to it, check it runs, then
/// rename over (atomic, and harmless to the running process, which keeps
/// its open copy). `Ok(false)` when the directory isn't writable.
fn install(exe: &Path, binary: &[u8]) -> Result<bool> {
    let dir = exe.parent().context("executable has no directory")?;
    let name = exe.file_name().context("executable has no name")?;
    let tmp = dir.join(format!(
        ".{}.update-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    match std::fs::write(&tmp, binary) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("writing {}", tmp.display())),
    }
    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        // It must at least start on this machine before it replaces us.
        let out = std::process::Command::new(&tmp)
            .arg("--version")
            .env("ATELIER_NO_UPDATE", "1")
            .output()
            .context("running the new binary")?;
        if !out.status.success() || !String::from_utf8_lossy(&out.stdout).starts_with("atelier ") {
            bail!("the downloaded binary doesn't run here");
        }
        std::fs::rename(&tmp, exe).with_context(|| format!("replacing {}", exe.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map(|()| true)
}

/// GET a URL (following GitHub's redirects to its asset storage).
fn get(url: &str) -> Result<Vec<u8>> {
    let resp = rsurl::Request::new("GET", url)
        .with_context(|| format!("building request for {url}"))?
        .header("user-agent", &format!("atelier/{VERSION}"))
        .header(
            "accept",
            "application/vnd.github+json, application/octet-stream",
        )
        .follow_redirects(true)
        .connect_timeout(Duration::from_secs(15))
        .max_time(Duration::from_secs(300))
        .send()
        .with_context(|| format!("fetching {url}"))?;
    if !(200..300).contains(&resp.status) {
        bail!("{url}: HTTP {}", resp.status);
    }
    Ok(resp.body)
}

/// `v1.2.3` / `1.2.3` → (1, 2, 3). Pre-releases (`-rc1` …) aren't offered.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.strip_prefix('v').unwrap_or(s).split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let v = (next()?, next()?, next()?);
    parts.next().is_none().then_some(v)
}

/// The expected hex digest of `name` in `sha256sum` output.
fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        // `sha256sum` marks binary mode with a leading `*`.
        let file = file.trim_start().trim_start_matches('*');
        (file == name).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the time of the last check is kept: `$XDG_STATE_HOME/atelier/`
/// (default `~/.local/state/atelier/`). `None` when there's no home.
fn stamp_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("atelier").join("last-update-check"))
}

/// Whether a check is due: the stamp is missing, unreadable, older than
/// [`CHECK_INTERVAL`], or in the future (a clock that jumped back).
fn due(stamp: &Path, now: SystemTime) -> bool {
    let Some(last) = std::fs::read_to_string(stamp)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    else {
        return true;
    };
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    last > now || now - last >= CHECK_INTERVAL.as_secs()
}

fn record_check(stamp: &Path, now: SystemTime) -> Result<()> {
    if let Some(dir) = stamp.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    std::fs::write(stamp, secs.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
        assert_eq!(parse_version("v1.2.3-rc1"), None);
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert!(parse_version(VERSION).is_some());
    }

    #[test]
    fn checksums() {
        let sums = "AB12  atelier-x86_64-unknown-linux-musl\n\
                    cd34 *atelier-aarch64-unknown-linux-musl\n";
        assert_eq!(
            checksum_for(sums, "atelier-x86_64-unknown-linux-musl").as_deref(),
            Some("ab12")
        );
        assert_eq!(
            checksum_for(sums, "atelier-aarch64-unknown-linux-musl").as_deref(),
            Some("cd34")
        );
        assert_eq!(checksum_for(sums, "atelier-other"), None);
        assert_eq!(hex(&purecrypto::hash::sha256(b"abc"))[..8], *"ba7816bf");
    }

    #[test]
    fn check_throttle() {
        let dir = std::env::temp_dir().join(format!("atelier-update-{}", std::process::id()));
        let stamp = dir.join("last-update-check");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(due(&stamp, t0)); // never checked
        record_check(&stamp, t0).unwrap();
        assert!(!due(&stamp, t0 + Duration::from_secs(3600)));
        assert!(due(&stamp, t0 + CHECK_INTERVAL));
        assert!(due(&stamp, t0 - Duration::from_secs(60))); // clock went back
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_swaps_in_a_working_binary_only() {
        let dir = std::env::temp_dir().join(format!("atelier-install-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("atelier");
        std::fs::write(&exe, "old").unwrap();

        // Doesn't run: refused, old binary left alone, no temp file left.
        assert!(install(&exe, b"#!/bin/sh\nexit 1\n").is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        // Runs and says it's atelier: installed.
        let new = b"#!/bin/sh\necho atelier 9.9.9\n";
        assert!(install(&exe, new).unwrap());
        assert_eq!(std::fs::read(&exe).unwrap(), new);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
