//! `/loop`: a prompt the session sends itself at an interval.
//!
//! `/loop 5m check the deploy and say if anything changed` runs the prompt
//! now, and again every five minutes, for as long as the session lives —
//! "keep an eye on this" without the user having to come back and ask.
//! `/loop` lists the loops, `/loop stop [n|all]` ends them.
//!
//! A firing is a turn like any other, started through the same seam as a
//! wake ([`super::idle`]): the interface's idle tick is handed the prompt
//! and runs it under the session's mode and permission rules. That seam is
//! also what makes the timing simple:
//!
//! - A loop only fires **while the session is idle**. One that comes due
//!   during a turn (the user's, or another loop's) fires when the prompt is
//!   free again — and once, however many intervals went by: the next firing
//!   is counted from the one that just happened, so missed ones do not pile
//!   up.
//! - Nothing starts while the user is typing a line.
//!
//! A loop is a standing order, so it is bounded: it stops after
//! `[loops] max_runs` firings ([`DEFAULT_MAX_RUNS`]), when two of its turns
//! in a row end in an error (an endpoint that is down should not be retried
//! every interval forever), on `/loop stop`, and — all loops at once — on
//! Ctrl-C at an empty prompt. Each firing is announced (`⏰ loop 1 (run 3 of
//! 50): …`).
//!
//! Loops belong to the session and are not saved: they end with the
//! process. A schedule that should outlive it is a job for cron and
//! `atelier --print`.
//!
//! Time is passed in rather than read ([`Loops::due`] takes `now`), so the
//! tests run on a clock of their own.

use std::time::{Duration, Instant};

/// Firings a loop gets when `[loops] max_runs` does not say.
pub const DEFAULT_MAX_RUNS: u32 = 50;

/// The shortest interval: a loop is for watching something, not for
/// spinning.
const MIN_INTERVAL: Duration = Duration::from_secs(10);

/// A turn that failed this many times in a row ends its loop.
const MAX_FAILURES: u32 = 2;

const USAGE: &str = "usage: /loop <interval> <prompt>   (interval: 30s, 5m, 1h; at least 10s)\n       \
                     /loop                       list the loops\n       \
                     /loop stop [n|all]          stop one, or all";

struct Loop {
    id: u32,
    /// The interval as the user wrote it, and as a duration.
    spec: String,
    every: Duration,
    prompt: String,
    /// When it fires next.
    next: Instant,
    runs: u32,
    /// Turns of its own that failed, in a row.
    failures: u32,
}

/// A session's loops.
pub(super) struct Loops {
    all: Vec<Loop>,
    /// Loops created so far; numbers them.
    created: u32,
    max_runs: u32,
    /// The loop whose turn is running.
    firing: Option<u32>,
}

/// What a firing amounts to: the line that announces it, and the prompt.
pub(super) struct Firing {
    pub(super) notice: String,
    pub(super) prompt: String,
}

impl Loops {
    pub(super) fn new(max_runs: Option<u32>) -> Self {
        Self {
            all: Vec::new(),
            created: 0,
            max_runs: max_runs.unwrap_or(DEFAULT_MAX_RUNS).max(1),
            firing: None,
        }
    }

    /// `/loop <rest>`: what to tell the user.
    pub(super) fn command(&mut self, rest: &str, now: Instant) -> Vec<String> {
        let (first, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let tail = tail.trim();
        match first {
            "" => self.list(now),
            "stop" => vec![self.stop(tail)],
            _ => match (interval(first), tail) {
                (Ok(_), "") => USAGE.lines().map(String::from).collect(),
                (Ok(every), prompt) => {
                    self.created += 1;
                    self.all.push(Loop {
                        id: self.created,
                        spec: first.to_string(),
                        every,
                        prompt: prompt.to_string(),
                        // The first run is the next thing the session does.
                        next: now,
                        runs: 0,
                        failures: 0,
                    });
                    vec![format!(
                        "loop {}: every {first}, up to {} runs — /loop stop {} ends it",
                        self.created, self.max_runs, self.created
                    )]
                }
                (Err(why), _) => {
                    let mut lines = vec![why];
                    lines.extend(USAGE.lines().map(String::from));
                    lines
                }
            },
        }
    }

    fn list(&self, now: Instant) -> Vec<String> {
        if self.all.is_empty() {
            return vec!["no loops — /loop <interval> <prompt> starts one".to_string()];
        }
        self.all
            .iter()
            .map(|l| {
                format!(
                    "loop {}: every {}, {} of {} runs, next in {}: {}",
                    l.id,
                    l.spec,
                    l.runs,
                    self.max_runs,
                    brief(l.next.saturating_duration_since(now)),
                    l.prompt
                )
            })
            .collect()
    }

    /// `/loop stop [n|all]`.
    fn stop(&mut self, which: &str) -> String {
        match which {
            "" | "all" => match self.stop_all() {
                0 => "no loops to stop".to_string(),
                n => format!("stopped {n} loop(s)"),
            },
            n => match n.parse::<u32>() {
                Ok(id) if self.all.iter().any(|l| l.id == id) => {
                    self.all.retain(|l| l.id != id);
                    format!("stopped loop {id}")
                }
                Ok(id) => format!("no loop {id} — /loop lists them"),
                Err(_) => "usage: /loop stop [n|all]".to_string(),
            },
        }
    }

    /// End every loop; returns how many there were.
    pub(super) fn stop_all(&mut self) -> usize {
        let n = self.all.len();
        self.all.clear();
        n
    }

    /// The loop to run now, if one is due: the one that has waited longest.
    /// Called only while the session is idle, which is what holds a loop
    /// back during a turn.
    pub(super) fn due(&mut self, now: Instant) -> Option<Firing> {
        let max = self.max_runs;
        let l = self
            .all
            .iter_mut()
            .filter(|l| l.next <= now)
            .min_by_key(|l| l.next)?;
        l.runs += 1;
        // From now, not from when it was due: intervals that went by while
        // the session was busy are not made up for.
        l.next = now + l.every;
        let last = l.runs >= max;
        let firing = Firing {
            notice: format!(
                "⏰ loop {} (run {} of {max}{}): {}",
                l.id,
                l.runs,
                if last { ", its last" } else { "" },
                l.prompt
            ),
            prompt: l.prompt.clone(),
        };
        let id = l.id;
        self.firing = Some(id);
        if last {
            self.all.retain(|l| l.id != id);
        }
        Some(firing)
    }

    /// A turn is over; `ok` unless it ended in an error. If it was a loop's
    /// and the loop is stopped for failing, the line that says so.
    pub(super) fn turn_ended(&mut self, ok: bool) -> Option<String> {
        let id = self.firing.take()?;
        let l = self.all.iter_mut().find(|l| l.id == id)?;
        if ok {
            l.failures = 0;
            return None;
        }
        l.failures += 1;
        if l.failures < MAX_FAILURES {
            return None;
        }
        self.all.retain(|l| l.id != id);
        Some(format!(
            "⏰ loop {id} stopped: its turn failed {MAX_FAILURES} times in a row"
        ))
    }
}

/// An interval as typed: a whole number of seconds (`30s`), minutes (`5m`)
/// or hours (`1h`).
fn interval(spec: &str) -> Result<Duration, String> {
    let unit = match spec.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 3600,
        _ => return Err(format!("'{spec}' is not an interval (30s, 5m, 1h)")),
    };
    let every = spec[..spec.len() - 1]
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("'{spec}' is not an interval (30s, 5m, 1h)"))?;
    if every < MIN_INTERVAL {
        return Err(format!(
            "an interval of {spec} is too short: the least is {}s",
            MIN_INTERVAL.as_secs()
        ));
    }
    Ok(every)
}

/// A wait, to the precision a glance needs: `45s`, `12m`, `3h`.
fn brief(d: Duration) -> String {
    match d.as_secs() {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s.div_ceil(60)),
        s => format!("{}h", s.div_ceil(3600)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// The prompt of the firing due at `now`, if any.
    fn fired(loops: &mut Loops, now: Instant) -> Option<String> {
        let firing = loops.due(now)?;
        loops.turn_ended(true);
        Some(firing.prompt)
    }

    #[test]
    fn intervals_are_seconds_minutes_or_hours_and_not_too_short() {
        assert_eq!(interval("30s"), Ok(secs(30)));
        assert_eq!(interval("5m"), Ok(secs(300)));
        assert_eq!(interval("1h"), Ok(secs(3600)));
        assert_eq!(interval("10s"), Ok(secs(10)));
        assert!(interval("9s").unwrap_err().contains("too short"));
        assert!(interval("0m").unwrap_err().contains("too short"));
        for bad in ["", "5", "m", "5x", "-5m", "1.5h", "5 m"] {
            assert!(
                interval(bad).unwrap_err().contains("not an interval"),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_loop_runs_at_once_and_then_each_interval() {
        let t0 = Instant::now();
        let mut loops = Loops::new(None);
        let said = loops.command("30s check the build", t0);
        assert_eq!(
            said,
            ["loop 1: every 30s, up to 50 runs — /loop stop 1 ends it"]
        );

        let first = loops.due(t0).expect("due at once");
        assert_eq!(first.notice, "⏰ loop 1 (run 1 of 50): check the build");
        assert_eq!(first.prompt, "check the build");
        loops.turn_ended(true);
        // Not again before the interval is up.
        assert_eq!(fired(&mut loops, t0 + secs(1)), None);
        assert_eq!(fired(&mut loops, t0 + secs(29)), None);
        assert!(fired(&mut loops, t0 + secs(30)).is_some());
        assert_eq!(fired(&mut loops, t0 + secs(59)), None);
        assert!(fired(&mut loops, t0 + secs(60)).is_some());
    }

    /// Nobody asks while a turn runs. When the session is idle again, a
    /// loop that came due meanwhile fires once — not once per interval
    /// missed — and its next firing is an interval from then.
    #[test]
    fn a_loop_waits_for_idle_and_missed_firings_do_not_pile_up() {
        let t0 = Instant::now();
        let mut loops = Loops::new(None);
        loops.command("30s tick", t0);
        assert!(fired(&mut loops, t0).is_some());
        // A turn of the user's runs from t0+20 to t0+200: no ticks.
        let idle_again = t0 + secs(200);
        assert!(fired(&mut loops, idle_again).is_some());
        assert_eq!(fired(&mut loops, idle_again), None);
        assert_eq!(fired(&mut loops, idle_again + secs(29)), None);
        assert!(fired(&mut loops, idle_again + secs(30)).is_some());
    }

    #[test]
    fn of_two_due_loops_the_one_that_waited_longest_goes_first() {
        let t0 = Instant::now();
        let mut loops = Loops::new(None);
        loops.command("1m slow", t0);
        loops.command("10s fast", t0 + secs(1));
        assert_eq!(fired(&mut loops, t0 + secs(5)).as_deref(), Some("slow"));
        assert_eq!(fired(&mut loops, t0 + secs(5)).as_deref(), Some("fast"));
        assert_eq!(fired(&mut loops, t0 + secs(5)), None);
        // One firing per look, so the turns run one after the other.
        let later = t0 + secs(120);
        assert_eq!(fired(&mut loops, later).as_deref(), Some("fast"));
        assert_eq!(fired(&mut loops, later).as_deref(), Some("slow"));
    }

    #[test]
    fn loops_are_listed_and_stopped() {
        let t0 = Instant::now();
        let mut loops = Loops::new(Some(7));
        assert_eq!(
            loops.command("", t0),
            ["no loops — /loop <interval> <prompt> starts one"]
        );
        loops.command("5m watch the deploy", t0);
        loops.command("1h  summarize   the log", t0);
        fired(&mut loops, t0);
        assert_eq!(
            loops.command("", t0 + secs(60)),
            [
                "loop 1: every 5m, 1 of 7 runs, next in 4m: watch the deploy",
                "loop 2: every 1h, 0 of 7 runs, next in 0s: summarize   the log",
            ]
        );
        assert_eq!(
            loops.command("stop 3", t0),
            ["no loop 3 — /loop lists them"]
        );
        assert_eq!(loops.command("stop x", t0), ["usage: /loop stop [n|all]"]);
        assert_eq!(loops.command("stop 1", t0), ["stopped loop 1"]);
        assert_eq!(
            fired(&mut loops, t0 + secs(600)).as_deref(),
            Some("summarize   the log")
        );
        // Numbers are not reused.
        assert!(loops.command("10s again", t0)[0].starts_with("loop 3:"));
        assert_eq!(loops.command("stop", t0), ["stopped 2 loop(s)"]);
        assert_eq!(loops.command("stop all", t0), ["no loops to stop"]);
        assert_eq!(fired(&mut loops, t0 + secs(7200)), None);

        // What is not a loop says how to write one.
        assert!(loops.command("5m", t0)[0].starts_with("usage: /loop"));
        let bad = loops.command("soon do it", t0);
        assert!(bad[0].contains("'soon' is not an interval") && bad[1].starts_with("usage:"));
        assert!(loops.command("2s do it", t0)[0].contains("too short"));
        assert_eq!(loops.stop_all(), 0);
    }

    #[test]
    fn a_loop_ends_at_its_cap() {
        let t0 = Instant::now();
        let mut loops = Loops::new(Some(3));
        loops.command("10s tick", t0);
        let notices: Vec<String> = (0..5)
            .filter_map(|i| loops.due(t0 + secs(10 * i)))
            .map(|f| f.notice)
            .collect();
        assert_eq!(
            notices,
            [
                "⏰ loop 1 (run 1 of 3): tick",
                "⏰ loop 1 (run 2 of 3): tick",
                "⏰ loop 1 (run 3 of 3, its last): tick",
            ]
        );
        assert_eq!(
            loops.command("", t0)[0],
            "no loops — /loop <interval> <prompt> starts one"
        );
        // Its last turn failing is nothing to report: it is already gone.
        assert_eq!(loops.turn_ended(false), None);
    }

    #[test]
    fn two_failed_turns_in_a_row_end_a_loop() {
        let t0 = Instant::now();
        let mut loops = Loops::new(None);
        loops.command("10s tick", t0);
        let mut run = |n: u64, ok: bool| {
            loops.due(t0 + secs(10 * n)).expect("due");
            loops.turn_ended(ok)
        };
        // A failure, then a success: the count starts over.
        assert_eq!(run(0, false), None);
        assert_eq!(run(1, true), None);
        assert_eq!(run(2, false), None);
        assert_eq!(
            run(3, false).as_deref(),
            Some("⏰ loop 1 stopped: its turn failed 2 times in a row")
        );
        assert!(loops.due(t0 + secs(1000)).is_none());

        // A turn that was not a loop's changes nothing.
        loops.command("10s tock", t0);
        assert_eq!(loops.turn_ended(false), None);
        assert_eq!(loops.turn_ended(false), None);
        assert!(loops.due(t0).is_some());
    }
}
