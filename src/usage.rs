//! What the model requests cost: a log of every request, and the sums over it.
//!
//! # The log
//!
//! Each completed model request — the conversation's, the reviewer's, a
//! compaction summary — appends one JSON line to
//! `$XDG_STATE_HOME/atelier/usage/YYYY-MM.jsonl` (else under
//! `~/.local/state`): when, which provider and model, the project directory,
//! the token counts the endpoint reported (zeros when it reported none), and
//! how long the request took. It is written by the provider code
//! ([`record`]), so nothing that makes a request can forget to. Writing is
//! best-effort: a log that can't be written never fails a turn.
//!
//! One file per month keeps `atelier stats` from reading years of history to
//! answer for the last week, and makes old usage something a user can delete
//! by file. Lines are appended whole with `O_APPEND`, so several atelier
//! processes share a file without a lock.
//!
//! # The sums
//!
//! `atelier stats` ([`run_command`]) totals the log per day (UTC), per model
//! and per project; `/usage` ([`session_report`]) does the same for the
//! requests of this process, kept in memory as they are recorded. Cost is
//! computed at reading time from the `[[price]]` tables in force, so
//! changing a price re-prices history, and a model without a price simply
//! has no cost.
//!
//! The Claude backend makes no requests of its own — `claude` does, and
//! bills them itself — so it writes nothing here; `/usage` shows the token
//! counts claude reports.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::provider::Usage;
use crate::settings::{PriceConfig, Settings};

/// How far back `atelier stats` looks unless told.
const DEFAULT_DAYS: u64 = 30;

const SECS_PER_DAY: u64 = 86_400;

const STATS_USAGE: &str = "usage: atelier stats [--days N] [--project] [--json]";

/// One model request, as logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// When the request finished, in seconds since the Unix epoch.
    pub ts: u64,
    pub provider: String,
    /// The model id as sent to the provider.
    pub model: String,
    /// The directory atelier was run in.
    #[serde(default)]
    pub project: String,
    /// Prompt tokens, cached ones included.
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// The part of `prompt_tokens` the provider served from its cache.
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub duration_ms: u64,
}

/// The requests this process has made, for `/usage`.
static SESSION: Mutex<Vec<Record>> = Mutex::new(Vec::new());

/// Log a finished request and count it towards this session's totals.
pub fn record(provider: &str, model: &str, usage: Option<&Usage>, took: Duration) {
    let usage = usage.cloned().unwrap_or_default();
    let record = Record {
        ts: now(),
        provider: provider.to_string(),
        model: model.to_string(),
        project: std::env::current_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
        prompt_tokens: u64::from(usage.prompt_tokens),
        completion_tokens: u64::from(usage.completion_tokens),
        cached_tokens: u64::from(usage.cached_tokens),
        duration_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
    };
    if let Some(dir) = log_dir() {
        // Best effort, as the module docs say.
        let _ = append(&dir, &record);
    }
    SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(record);
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `$XDG_STATE_HOME/atelier/usage`, else `~/.local/state/atelier/usage`.
fn log_dir() -> Option<PathBuf> {
    let abs = |k: &str| {
        std::env::var_os(k)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let state = abs("XDG_STATE_HOME").or_else(|| abs("HOME").map(|h| h.join(".local/state")))?;
    Some(state.join("atelier").join("usage"))
}

/// Append `record` to its month's file under `dir`.
fn append(dir: &Path, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let (year, month, _) = civil(record.ts / SECS_PER_DAY);
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{year:04}-{month:02}.jsonl")))?;
    // One write per line, so concurrent appenders can't interleave.
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// The records under `dir` from `since` (seconds) on, oldest file first.
/// Lines that don't parse — a torn write, a future format — are skipped.
fn read(dir: &Path, since: u64) -> Vec<Record> {
    let (year, month, _) = civil(since / SECS_PER_DAY);
    let first = format!("{year:04}-{month:02}.jsonl");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        // `YYYY-MM.jsonl` names sort by date.
        .filter(|name| name.ends_with(".jsonl") && *name >= first)
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|name| std::fs::read_to_string(dir.join(name)).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str::<Record>(l).ok())
                .collect::<Vec<_>>()
        })
        .filter(|r| r.ts >= since)
        .collect()
}

/// The civil date (year, month, day) of a day counted from the Unix epoch,
/// UTC. (Howard Hinnant's `civil_from_days`, for dates from 1970 on.)
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// The price of `provider/model`: the first `[[price]]` naming it in full,
/// else the first naming the bare model.
fn price_of<'a>(prices: &'a [PriceConfig], provider: &str, model: &str) -> Option<&'a PriceConfig> {
    let full = format!("{provider}/{model}");
    prices
        .iter()
        .find(|p| p.model == full)
        .or_else(|| prices.iter().find(|p| p.model == model))
}

/// What a request cost at `price` (per million tokens; cached prompt tokens
/// at their own rate when there is one).
fn cost_of(record: &Record, price: &PriceConfig) -> f64 {
    let cached = record.cached_tokens.min(record.prompt_tokens);
    let fresh = record.prompt_tokens - cached;
    (fresh as f64 * price.input
        + cached as f64 * price.cached.unwrap_or(price.input)
        + record.completion_tokens as f64 * price.output)
        / 1e6
}

/// Sums over a group of requests.
#[derive(Debug, Default, Clone, PartialEq)]
struct Totals {
    requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    duration_ms: u64,
    /// The cost of the requests whose model has a price, and how many those
    /// are: with `priced < requests` the cost is a lower bound.
    cost: f64,
    priced: u64,
}

impl Totals {
    fn add(&mut self, record: &Record, price: Option<&PriceConfig>) {
        self.requests += 1;
        self.prompt_tokens += record.prompt_tokens;
        self.completion_tokens += record.completion_tokens;
        self.cached_tokens += record.cached_tokens;
        self.duration_ms += record.duration_ms;
        if let Some(price) = price {
            self.cost += cost_of(record, price);
            self.priced += 1;
        }
    }

    /// `cost` is null when no request of the group had a price.
    fn to_json(&self) -> Value {
        json!({
            "requests": self.requests,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "cached_tokens": self.cached_tokens,
            "duration_ms": self.duration_ms,
            "cost": (self.priced > 0).then_some(self.cost),
            "priced_requests": self.priced,
        })
    }

    /// The cost column: blank without a price, marked when only part of the
    /// group has one.
    fn cost_text(&self) -> String {
        match self.priced {
            0 => "-".to_string(),
            n if n < self.requests => format!("{:.4}+", self.cost),
            _ => format!("{:.4}", self.cost),
        }
    }
}

/// The log summed three ways.
#[derive(Debug, Default)]
struct Stats {
    total: Totals,
    by_day: BTreeMap<String, Totals>,
    by_model: BTreeMap<String, Totals>,
    by_project: BTreeMap<String, Totals>,
}

fn summarize(records: &[Record], prices: &[PriceConfig]) -> Stats {
    let mut stats = Stats::default();
    for r in records {
        let price = price_of(prices, &r.provider, &r.model);
        let (y, m, d) = civil(r.ts / SECS_PER_DAY);
        stats.total.add(r, price);
        let groups = [
            (&mut stats.by_day, format!("{y:04}-{m:02}-{d:02}")),
            (&mut stats.by_model, format!("{}/{}", r.provider, r.model)),
            (&mut stats.by_project, r.project.clone()),
        ];
        for (group, key) in groups {
            group.entry(key).or_default().add(r, price);
        }
    }
    stats
}

impl Stats {
    fn to_json(&self, days: u64) -> Value {
        let rows = |group: &BTreeMap<String, Totals>, key: &str| -> Value {
            group
                .iter()
                .map(|(name, totals)| {
                    let mut row = totals.to_json();
                    row[key] = json!(name);
                    row
                })
                .collect()
        };
        json!({
            "days": days,
            "total": self.total.to_json(),
            "by_day": rows(&self.by_day, "day"),
            "by_model": rows(&self.by_model, "model"),
            "by_project": rows(&self.by_project, "project"),
        })
    }

    fn to_text(&self, days: u64) -> Vec<String> {
        if self.total.requests == 0 {
            return vec![format!(
                "no model requests logged in the last {days} day(s)"
            )];
        }
        let mut out = Vec::new();
        for (title, group) in [
            ("day", &self.by_day),
            ("model", &self.by_model),
            ("project", &self.by_project),
        ] {
            out.extend(table(title, group));
            out.push(String::new());
        }
        out.extend(table(
            "total",
            &BTreeMap::from([(format!("last {days} day(s)"), self.total.clone())]),
        ));
        if self.total.priced < self.total.requests {
            out.push(String::new());
            out.push(
                "cost: '-' no [[price]] for the model; '+' some requests in the row have none"
                    .to_string(),
            );
        }
        out
    }
}

/// One aligned table: a header naming the grouping, then a row per group.
fn table(title: &str, group: &BTreeMap<String, Totals>) -> Vec<String> {
    let width = group.keys().map(|k| k.chars().count()).max().unwrap_or(0);
    let width = width.max(title.len());
    let row = |name: &str, cells: [String; 5]| {
        let [requests, prompt, cached, completion, cost] = cells;
        format!(
            "{name:<width$}  {requests:>8}  {prompt:>12}  {cached:>12}  {completion:>12}  {cost:>10}"
        )
    };
    let header = ["requests", "prompt", "cached", "completion", "cost"].map(String::from);
    let mut out = vec![row(title, header)];
    for (name, t) in group {
        out.push(row(
            name,
            [
                t.requests.to_string(),
                t.prompt_tokens.to_string(),
                t.cached_tokens.to_string(),
                t.completion_tokens.to_string(),
                t.cost_text(),
            ],
        ));
    }
    out
}

/// `/usage`: this session's requests per model, with cost where a price is
/// known. With the Claude backend, the counts claude reported (`context` at
/// its latest request, `output` over the session).
pub fn session_report(
    prices: &[PriceConfig],
    claude: bool,
    context: u32,
    output: u64,
) -> Vec<String> {
    if claude {
        return vec![
            "Claude backend: claude makes and bills the model requests; it reports".to_string(),
            format!("  context at the latest request: {context} tokens"),
            format!("  output this session:           {output} tokens"),
        ];
    }
    let stats = session_stats(prices);
    if stats.total.requests == 0 {
        return vec!["no model requests made yet this session".to_string()];
    }
    let mut out = table("model", &stats.by_model);
    if stats.by_model.len() > 1 {
        let total = BTreeMap::from([("total".to_string(), stats.total.clone())]);
        // Its rows only: the header is already there.
        out.extend(table("model", &total).split_off(1));
    }
    out.push(format!(
        "time waiting on the model: {:.1}s",
        stats.total.duration_ms as f64 / 1000.0
    ));
    out
}

/// This process's model requests, summed.
fn session_stats(prices: &[PriceConfig]) -> Stats {
    let session = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    summarize(&session, prices)
}

/// What `/usage` reports ([`session_report`]), as data: the session's
/// requests per model and in total, with cost where a price is known, next
/// to the token counts the session keeps (`context` at its latest request,
/// `output` over the session). With the Claude backend there are only
/// those: claude makes the requests, and `by_model` is empty.
pub fn session_data(prices: &[PriceConfig], claude: bool, context: u32, output: u64) -> Value {
    let stats = match claude {
        true => Stats::default(),
        false => session_stats(prices),
    };
    let by_model: Vec<Value> = stats
        .by_model
        .iter()
        .map(|(model, totals)| {
            let mut row = totals.to_json();
            row["model"] = json!(model);
            row
        })
        .collect();
    json!({
        "context_tokens": context,
        "output_tokens": output,
        "by_model": by_model,
        "total": stats.total.to_json(),
    })
}

/// What `atelier stats` was asked.
#[derive(Debug, PartialEq)]
struct StatsOptions {
    days: u64,
    project_only: bool,
    json: bool,
}

fn parse_args(args: &[String]) -> Result<StatsOptions> {
    let mut options = StatsOptions {
        days: DEFAULT_DAYS,
        project_only: false,
        json: false,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--days" => {
                let value = args.next().context("--days needs a number")?;
                options.days =
                    value.parse().ok().filter(|&n| n > 0).with_context(|| {
                        format!("--days needs a positive number, not '{value}'")
                    })?;
            }
            "--project" => options.project_only = true,
            "--json" => options.json = true,
            other => bail!("unknown argument '{other}'"),
        }
    }
    Ok(options)
}

/// `atelier stats [--days N] [--project] [--json]`: totals of the usage log
/// over the last N days (today included; default 30) — per day, per model
/// and per project, or for the current directory's project only.
pub fn run_command(args: &[String]) -> Result<()> {
    let options = parse_args(args).map_err(|e| anyhow::anyhow!("{e}\n{STATS_USAGE}"))?;
    let root = std::env::current_dir()?;
    // Prices are the one thing needed from the settings; a file that can't
    // be read only costs the cost column.
    let project = Settings::load(&root).unwrap_or_default();
    let user = Settings::load_user().unwrap_or_default();
    let prices = crate::config::merged_prices(&project, &user);

    // From the start (UTC) of the day `days - 1` days ago.
    let today = now() / SECS_PER_DAY;
    let since = today.saturating_sub(options.days - 1) * SECS_PER_DAY;
    let mut records = log_dir().map(|dir| read(&dir, since)).unwrap_or_default();
    if options.project_only {
        let here = root.to_string_lossy();
        records.retain(|r| r.project == here);
    }
    let stats = summarize(&records, &prices);
    if options.json {
        println!("{}", stats.to_json(options.days));
    } else {
        for line in stats.to_text(options.days) {
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ts: u64, provider: &str, model: &str, project: &str, tokens: [u64; 3]) -> Record {
        Record {
            ts,
            provider: provider.into(),
            model: model.into(),
            project: project.into(),
            prompt_tokens: tokens[0],
            completion_tokens: tokens[1],
            cached_tokens: tokens[2],
            duration_ms: 10,
        }
    }

    fn price(model: &str, input: f64, output: f64, cached: Option<f64>) -> PriceConfig {
        PriceConfig {
            model: model.into(),
            input,
            output,
            cached,
        }
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(59), (1970, 3, 1));
        // 2000-02-29 and the day after; 2026-11-13.
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(11_017), (2000, 3, 1));
        assert_eq!(civil(20_770), (2026, 11, 13));
        assert_eq!(civil(1_799_999_999 / SECS_PER_DAY), (2027, 1, 15));
    }

    #[test]
    fn cost_uses_the_cached_rate_for_cached_prompt_tokens() {
        let r = record(0, "p", "m", "", [1_000_000, 100_000, 400_000]);
        // 600k fresh at 2, 400k cached at 0.5, 100k out at 10.
        let c = cost_of(&r, &price("m", 2.0, 10.0, Some(0.5)));
        assert!((c - 2.4).abs() < 1e-9, "{c}");
        // No cached rate: all prompt tokens at the input price.
        let c = cost_of(&r, &price("m", 2.0, 10.0, None));
        assert!((c - 3.0).abs() < 1e-9, "{c}");
    }

    #[test]
    fn a_full_name_price_wins_over_a_bare_one() {
        let prices = [price("m", 1.0, 1.0, None), price("p/m", 2.0, 2.0, None)];
        assert_eq!(price_of(&prices, "p", "m").unwrap().input, 2.0);
        assert_eq!(price_of(&prices, "q", "m").unwrap().input, 1.0);
        assert!(price_of(&prices, "p", "other").is_none());
    }

    /// Records land in their month's file, and reading back honors the
    /// cutoff and skips what isn't a record.
    #[test]
    fn the_log_round_trips_by_month() {
        let dir = std::env::temp_dir().join(format!(
            "atelier-usage-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let day = SECS_PER_DAY;
        // 2026-10-31, 2026-11-01, 2026-11-13.
        let (oct, nov1, nov13) = (20_757 * day, 20_758 * day + 5, 20_770 * day);
        for (ts, model) in [(oct, "a"), (nov1, "b"), (nov13, "c")] {
            append(&dir, &record(ts, "p", model, "/x", [1, 2, 0])).unwrap();
        }
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["2026-10.jsonl", "2026-11.jsonl"]);
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("2026-11.jsonl"))
            .unwrap()
            .write_all(b"not json\n{\"half\":\n")
            .unwrap();

        let models =
            |since| -> Vec<String> { read(&dir, since).into_iter().map(|r| r.model).collect() };
        assert_eq!(models(0), ["a", "b", "c"]);
        assert_eq!(models(nov1), ["b", "c"]);
        assert_eq!(models(nov1 + 1), ["c"]);
        assert!(models(nov13 + 1).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_sum_per_day_model_and_project() {
        let day = SECS_PER_DAY;
        let records = [
            record(20_769 * day, "env", "m", "/a", [100, 10, 0]),
            record(20_770 * day, "env", "m", "/a", [200, 20, 50]),
            record(20_770 * day + 9, "second", "n", "/b", [1_000, 500, 0]),
        ];
        let prices = [price("env/m", 1.0, 2.0, None)];
        let stats = summarize(&records, &prices);
        assert_eq!(stats.total.requests, 3);
        assert_eq!(stats.total.prompt_tokens, 1_300);
        assert_eq!(stats.total.completion_tokens, 530);
        assert_eq!(stats.total.cached_tokens, 50);
        assert_eq!(stats.total.priced, 2);
        assert_eq!(stats.by_day["2026-11-12"].requests, 1);
        assert_eq!(stats.by_day["2026-11-13"].requests, 2);
        assert_eq!(stats.by_model["env/m"].prompt_tokens, 300);
        assert_eq!(stats.by_project["/b"].completion_tokens, 500);

        let json = stats.to_json(7);
        assert_eq!(json["days"], 7);
        assert_eq!(json["total"]["requests"], 3);
        assert_eq!(json["by_model"][0]["model"], "env/m");
        let cost = json["by_model"][0]["cost"].as_f64().unwrap();
        assert!((cost - (300.0 + 60.0) / 1e6).abs() < 1e-12, "{cost}");
        // No price: no cost, rather than a zero that reads as free.
        assert!(json["by_model"][1]["cost"].is_null());
        assert_eq!(json["by_project"][1]["project"], "/b");

        let text = stats.to_text(7).join("\n");
        assert!(text.contains("second/n"), "{text}");
        assert!(text.contains("0.0004+"), "{text}");
        assert_eq!(
            summarize(&[], &prices).to_text(7),
            ["no model requests logged in the last 7 day(s)"]
        );
    }

    #[test]
    fn stats_arguments() {
        let parse =
            |args: &[&str]| parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            parse(&[]).unwrap(),
            StatsOptions {
                days: 30,
                project_only: false,
                json: false
            }
        );
        assert_eq!(
            parse(&["--json", "--days", "7", "--project"]).unwrap(),
            StatsOptions {
                days: 7,
                project_only: true,
                json: true
            }
        );
        assert!(parse(&["--days"]).is_err());
        assert!(parse(&["--days", "0"]).is_err());
        assert!(parse(&["--weeks"]).is_err());
    }

    #[test]
    fn the_claude_backend_reports_what_claude_counted() {
        let lines = session_report(&[], true, 1234, 56);
        assert!(
            lines[1].contains("1234") && lines[2].contains("56"),
            "{lines:?}"
        );
    }
}
