//! Turns the session starts by itself, while it waits at the prompt.
//!
//! A session only works when its user prompts it. This module is the one
//! exception, and it is opt-in: an idle instance may be **woken** by another
//! instance — start a turn because a peer's question or message arrived,
//! instead of holding it for the user's next prompt.
//!
//! # The seam
//!
//! An interface that sits at a prompt calls [`Session::idle_tick`] now and
//! then (the TUI and the line REPL do, every fraction of a second; a
//! headless `--print` run never does, so it never starts a turn of its own).
//! It returns the prompt of a turn to start, or `None`, and the interface
//! runs that prompt exactly as if it had been typed — same mode, same
//! permission rules, same approval prompts. Nothing is approved because the
//! turn started itself: a woken turn that needs an approval waits at the
//! prompt for the user like any other.
//!
//! # Waking
//!
//! `[coordinator] wake` (or `ATELIER_COORDINATOR_WAKE`) is `never` (the
//! default), `questions` (a `peer_ask` wakes; plain messages wait) or
//! `always`. Whether a message wakes is decided once, when it arrives
//! ([`Idle::judge`], called from `Session::poll_peers`), and a wake that was
//! decided on starts at the first tick where the user is not in the middle
//! of typing a line. What keeps this from running away:
//!
//! - **Who can wake.** Only the user's own instances reach the coordinator
//!   (its socket is in a directory nobody else can enter).
//! - **A cap.** At most `wake_max` (default [`DEFAULT_WAKE_MAX`]) turns are
//!   started this way without the user typing anything; whatever they type
//!   next resets the count.
//! - **No ping-pong.** Every message carries a `wake_depth`: 0 from a turn
//!   the user started, and one more than the depth of the message that woke
//!   it from a woken turn. An instance that has already started a turn by
//!   itself since its user last typed does not wake for a message of depth
//!   [`MAX_DEPTH`] or more. So A (prompted) may wake B, and B's reply may
//!   wake A once; what A's woken turn sends back wakes nobody.
//! - **It is visible.** The turn is announced (`⏰ woken by <peer>: …`),
//!   with the count against the cap.

use std::io::{self, BufRead};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::{Session, Ui};
use crate::coord::{self, Wake};
use crate::settings::Settings;

/// Self-started turns allowed between two inputs of the user's, when
/// `[coordinator] wake_max` does not say.
pub const DEFAULT_WAKE_MAX: u32 = 5;

/// The `wake_depth` from which a message no longer wakes an instance that
/// has already woken since its user last typed.
const MAX_DEPTH: u32 = 2;

/// Overrides `[coordinator] wake`.
const ENV_WAKE: &str = "ATELIER_COORDINATOR_WAKE";

/// The prompt of a woken turn. The message that caused it leads the prompt,
/// as the messages received while idle always do.
const WAKE_PROMPT: &str = "\
[woken] Nobody typed this prompt: this instance was idle when the above \
arrived from another atelier instance, and it is set to wake for that. Deal \
with what was sent — answer a question with peer_answer — doing no more than \
it calls for, then stop. Your user may not be watching.";

/// The session's standing with regard to starting turns by itself.
pub(super) struct Idle {
    policy: Wake,
    wake_max: u32,
    /// Turns started by a wake since the user last typed anything.
    woken: u32,
    /// A wake decided on, waiting for the prompt to be free.
    due: Option<Due>,
}

/// The message a wake is for.
struct Due {
    from: String,
    body: String,
    /// The id, if it is a question: a wake for a question nobody waits for
    /// any more is not worth a turn.
    question: Option<String>,
    wake_depth: u32,
}

impl Idle {
    /// As `[coordinator]` and the environment say. A value of the variable
    /// that is not a policy means `never`: the careful reading of a typo.
    pub(super) fn configured(settings: &Settings) -> Self {
        let policy = match std::env::var(ENV_WAKE) {
            Ok(v) if !v.is_empty() => Wake::parse(&v).unwrap_or(Wake::Never),
            _ => settings.coordinator.wake.unwrap_or_default(),
        };
        Self::new(policy, settings.coordinator.wake_max)
    }

    fn new(policy: Wake, wake_max: Option<u32>) -> Self {
        Self {
            policy,
            wake_max: wake_max.unwrap_or(DEFAULT_WAKE_MAX),
            woken: 0,
            due: None,
        }
    }

    /// Whether `m`, arriving now, is to start a turn.
    fn wakes(&self, m: &coord::Message) -> bool {
        let wanted = match self.policy {
            Wake::Never => false,
            Wake::Questions => m.question.is_some(),
            Wake::Always => true,
        };
        let chained = m.wake_depth >= MAX_DEPTH && self.woken > 0;
        wanted && self.woken < self.wake_max && !chained
    }

    /// Look at the messages that just arrived while idle: the first that
    /// qualifies is what the next self-started turn will be for. Later ones
    /// ride along (they lead the same prompt); the deepest `wake_depth`
    /// among them is the one the turn continues from.
    pub(super) fn judge(&mut self, fresh: &[coord::Message]) {
        for m in fresh {
            if !self.wakes(m) {
                continue;
            }
            match &mut self.due {
                Some(due) => due.wake_depth = due.wake_depth.max(m.wake_depth),
                None => {
                    self.due = Some(Due {
                        from: m.from.clone(),
                        body: m.body.clone(),
                        question: m.question.clone(),
                        wake_depth: m.wake_depth,
                    });
                }
            }
        }
    }
}

impl Session {
    /// Called by the interface while it waits at the prompt: the prompt of
    /// a turn to start now, if the session has reason to start one by
    /// itself (see the module docs). `typing` says the user has text on the
    /// input line; nothing starts under their fingers.
    pub fn idle_tick(&mut self, typing: bool, ui: &mut dyn Ui) -> Option<String> {
        if typing {
            return None;
        }
        self.wake(ui)
    }

    /// Start the wake that is due, if one is.
    fn wake(&mut self, ui: &mut dyn Ui) -> Option<String> {
        let coord = self.coord.clone()?;
        let due = self.idle.due.take()?;
        // The asker gave up while the user was typing.
        if due
            .question
            .as_deref()
            .is_some_and(|id| !coord.question_open(id))
        {
            return None;
        }
        self.idle.woken += 1;
        // What this turn sends is one step further from a user's prompt.
        coord.set_wake_depth(due.wake_depth + 1);
        let body: String = due.body.chars().take(200).collect();
        ui.info(&format!(
            "⏰ woken by {} ({} of {} without you): {body}",
            due.from, self.idle.woken, self.idle.wake_max
        ));
        Some(WAKE_PROMPT.to_string())
    }

    /// The user typed something (a prompt, a command): what the session
    /// does from here is on their word again.
    pub(super) fn user_typed(&mut self) {
        self.idle.woken = 0;
        if let Some(coord) = &self.coord {
            coord.set_wake_depth(0);
        }
    }

    /// A turn is over, however it ended.
    pub(super) fn turn_ended(&mut self, ui: &mut dyn Ui) {
        self.close_peer_questions(ui);
        // Whatever arrived before or during the turn was given to it.
        self.idle.due = None;
    }
}

/// Standard input, a line at a time, for an interface that must also keep
/// an eye on the clock while it waits (the line REPL, between prompts).
///
/// A blocking read cannot be abandoned, so once [`wait_line`] has been
/// called a thread owns stdin for the rest of the process and every read
/// goes through it — including the ones an approval prompt makes
/// ([`read_line`]), which would otherwise race it for the user's answer.
static LINES: OnceLock<Mutex<Receiver<String>>> = OnceLock::new();

/// What became of waiting for a line.
pub(super) enum Line {
    Typed(String),
    /// Nothing yet.
    Pending,
    /// End of input.
    Closed,
}

fn lines() -> &'static Mutex<Receiver<String>> {
    LINES.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Mutex::new(rx)
    })
}

/// The next line of standard input, waiting at most `wait` for it.
pub(super) fn wait_line(wait: Duration) -> Line {
    let rx = lines().lock().unwrap_or_else(|p| p.into_inner());
    match rx.recv_timeout(wait) {
        Ok(line) => Line::Typed(line),
        Err(RecvTimeoutError::Timeout) => Line::Pending,
        Err(RecvTimeoutError::Disconnected) => Line::Closed,
    }
}

/// The next line of standard input, however long it takes; `None` at the
/// end of input. Reads stdin directly unless [`wait_line`] has taken it
/// over.
pub(super) fn read_line() -> Option<String> {
    match LINES.get() {
        Some(rx) => rx.lock().unwrap_or_else(|p| p.into_inner()).recv().ok(),
        None => {
            let mut line = String::new();
            match io::stdin().read_line(&mut line) {
                Ok(n) if n > 0 => Some(line),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unixlike)]
    use crate::agent::tests::{Peers, Quiet, scratch_session, wait_for};

    fn message(question: bool, wake_depth: u32) -> coord::Message {
        coord::Message {
            from: "peer".into(),
            body: "hello".into(),
            question: question.then(|| "q1".to_string()),
            wake_depth,
            ..coord::Message::default()
        }
    }

    #[test]
    fn the_policy_decides_what_wakes() {
        // (policy, a question wakes, a message wakes)
        for (policy, question, plain) in [
            (Wake::Never, false, false),
            (Wake::Questions, true, false),
            (Wake::Always, true, true),
        ] {
            let idle = Idle::new(policy, None);
            assert_eq!(idle.wakes(&message(true, 0)), question, "{policy:?}");
            assert_eq!(idle.wakes(&message(false, 0)), plain, "{policy:?}");
        }
    }

    #[test]
    fn the_cap_counts_self_started_turns() {
        let mut idle = Idle::new(Wake::Always, Some(2));
        idle.woken = 1;
        assert!(idle.wakes(&message(false, 0)));
        idle.woken = 2;
        assert!(!idle.wakes(&message(false, 0)));
        assert!(!idle.wakes(&message(true, 0)));
        // The default.
        let mut idle = Idle::new(Wake::Always, None);
        idle.woken = DEFAULT_WAKE_MAX - 1;
        assert!(idle.wakes(&message(false, 0)));
        idle.woken = DEFAULT_WAKE_MAX;
        assert!(!idle.wakes(&message(false, 0)));
    }

    #[test]
    fn a_deep_message_does_not_wake_an_instance_that_already_woke() {
        let mut idle = Idle::new(Wake::Always, None);
        // Fresh from its user: even the end of a chain may wake it, once.
        assert!(idle.wakes(&message(false, MAX_DEPTH)));
        idle.woken = 1;
        assert!(idle.wakes(&message(false, MAX_DEPTH - 1)));
        assert!(!idle.wakes(&message(false, MAX_DEPTH)));
        assert!(!idle.wakes(&message(true, MAX_DEPTH + 3)));
    }

    /// A session set to `policy`, joined to `peers` as `name`.
    #[cfg(unixlike)]
    fn instance(
        peers: &Peers,
        name: &str,
        policy: Wake,
        wake_max: Option<u32>,
    ) -> (Session, std::sync::Arc<coord::Client>, std::path::PathBuf) {
        let (mut session, root) = scratch_session(&format!("idle-{name}"));
        let client = peers.join(name);
        session.attach_coordinator(std::sync::Arc::clone(&client));
        session.idle = Idle::new(policy, wake_max);
        (session, client, root)
    }

    /// Wait until `session` has been shown a message whose line contains
    /// `what`, the way an interface polls at the prompt.
    #[cfg(unixlike)]
    fn shown(session: &mut Session, ui: &mut Quiet, what: &str) {
        wait_for(what, || {
            session.poll_peers(ui);
            ui.infos
                .iter()
                .any(|l| l.starts_with('✉') && l.contains(what))
        });
    }

    /// Through a real coordinator: what arrives while idle starts a turn or
    /// not, as the policy says, and the turn is announced.
    #[cfg(unixlike)]
    #[test]
    fn an_idle_session_wakes_as_its_policy_says() {
        let peers = Peers::start("wake-policy");
        let other = peers.join("other");
        let long = std::time::Duration::from_secs(30);
        for (name, policy, on_question, on_message) in [
            ("never", Wake::Never, false, false),
            ("questions", Wake::Questions, true, false),
            ("always", Wake::Always, true, true),
        ] {
            let (mut session, me, root) = instance(&peers, name, policy, None);
            let mut ui = Quiet::default();

            assert_eq!(other.send(name, "the build is red").unwrap(), 1);
            shown(&mut session, &mut ui, "the build is red");
            let started = session.idle_tick(false, &mut ui);
            assert_eq!(started.is_some(), on_message, "{name}: a message");
            if on_message {
                assert_eq!(
                    ui.infos.last().unwrap(),
                    "⏰ woken by other (1 of 5 without you): the build is red"
                );
                assert!(started.unwrap().starts_with("[woken]"));
            }
            // Decided once: nothing starts later for the same message.
            assert!(session.idle_tick(false, &mut ui).is_none());
            session.turn_ended(&mut ui);
            session.user_typed();

            let asker = {
                let (other, to) = (std::sync::Arc::clone(&other), name.to_string());
                std::thread::spawn(move || other.ask(&to, "which port?", long))
            };
            shown(&mut session, &mut ui, "which port?");
            let started = session.idle_tick(false, &mut ui);
            assert_eq!(started.is_some(), on_question, "{name}: a question");
            // The question leads the turn's prompt, to be answered.
            let lead = session.peer_lead(&me);
            assert!(lead.contains("<peer-question id=\"q1\""), "{lead}");
            me.answer_question("q1", "8080").unwrap();
            asker.join().unwrap().unwrap();
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[cfg(unixlike)]
    #[test]
    fn nothing_starts_while_the_user_is_typing() {
        let peers = Peers::start("wake-typing");
        let other = peers.join("other");
        let (mut session, _me, root) = instance(&peers, "me", Wake::Always, None);
        let mut ui = Quiet::default();
        other.send("me", "ping").unwrap();
        shown(&mut session, &mut ui, "ping");
        for _ in 0..3 {
            assert!(session.idle_tick(true, &mut ui).is_none());
        }
        assert!(!ui.infos.iter().any(|l| l.starts_with('⏰')));
        // The line is empty again: now it starts.
        assert!(session.idle_tick(false, &mut ui).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unixlike)]
    #[test]
    fn the_cap_holds_until_the_user_types() {
        let peers = Peers::start("wake-cap");
        let other = peers.join("other");
        let (mut session, me, root) = instance(&peers, "me", Wake::Always, Some(2));
        let mut ui = Quiet::default();
        // A message arrives; if it starts a turn, the turn runs and ends.
        let arrives = |session: &mut Session, ui: &mut Quiet, body: &str| {
            other.send("me", body).unwrap();
            shown(session, ui, body);
            let started = session.idle_tick(false, ui).is_some();
            session.peer_lead(&me);
            session.turn_ended(ui);
            started
        };
        assert!(arrives(&mut session, &mut ui, "one"));
        assert!(arrives(&mut session, &mut ui, "two"));
        assert_eq!(
            ui.infos.iter().rfind(|l| l.starts_with('⏰')).unwrap(),
            "⏰ woken by other (2 of 2 without you): two"
        );
        assert!(!arrives(&mut session, &mut ui, "three"));
        assert!(!arrives(&mut session, &mut ui, "four"));
        session.user_typed();
        assert!(arrives(&mut session, &mut ui, "five"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two instances that both wake for anything: a prompt at one makes at
    /// most one self-started turn at each, and then it stops.
    #[cfg(unixlike)]
    #[test]
    fn two_instances_do_not_wake_each_other_forever() {
        let peers = Peers::start("wake-loop");
        let (mut a, a_link, a_root) = instance(&peers, "a", Wake::Always, None);
        let (mut b, b_link, b_root) = instance(&peers, "b", Wake::Always, None);
        let (mut a_ui, mut b_ui) = (Quiet::default(), Quiet::default());
        // One exchange: `from` sends `body`; does it start a turn at `to`?
        // If it does, that turn runs (its messages are sent from within it)
        // and ends.
        let wakes = |from: &coord::Client,
                     to: &mut Session,
                     to_link: &coord::Client,
                     ui: &mut Quiet,
                     body: &str| {
            from.send(&to_link.name(), body).unwrap();
            shown(to, ui, body);
            let started = to.idle_tick(false, ui).is_some();
            to.peer_lead(to_link);
            started
        };

        // a's user prompts it, and its turn messages b: b wakes.
        a.user_typed();
        assert!(wakes(&a_link, &mut b, &b_link, &mut b_ui, "from a's user"));
        // b's woken turn answers a: a wakes, once.
        assert!(wakes(&b_link, &mut a, &a_link, &mut a_ui, "b's reply"));
        b.turn_ended(&mut b_ui);
        // a's woken turn writes back: b, which already woke, stays idle —
        // and so does a for whatever a woken b might have sent.
        assert!(!wakes(&a_link, &mut b, &b_link, &mut b_ui, "a again"));
        a.turn_ended(&mut a_ui);
        assert!(!wakes(&a_link, &mut b, &b_link, &mut b_ui, "and again"));

        // b's user comes back: b may wake again, once more, and what it then
        // sends is too deep for a.
        b.user_typed();
        assert!(wakes(&a_link, &mut b, &b_link, &mut b_ui, "after b typed"));
        assert!(!wakes(
            &b_link,
            &mut a,
            &a_link,
            &mut a_ui,
            "b's second reply"
        ));
        b.turn_ended(&mut b_ui);
        // And a prompt at a starts the count from nothing.
        a.user_typed();
        b.user_typed();
        assert!(wakes(&a_link, &mut b, &b_link, &mut b_ui, "a's user again"));
        for root in [a_root, b_root] {
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn the_policy_is_read_from_settings() {
        let settings: Settings =
            tomlproc::serde::from_str("[coordinator]\nwake = \"questions\"\nwake_max = 3\n")
                .unwrap();
        assert_eq!(settings.coordinator.wake, Some(Wake::Questions));
        assert_eq!(settings.coordinator.wake_max, Some(3));
        // Left out when unset, so rewriting the file does not add the table.
        let text = tomlproc::serde::to_string_pretty(&Settings::default()).unwrap();
        assert!(!text.contains("coordinator"), "{text}");
        assert_eq!(Wake::parse("always"), Some(Wake::Always));
        assert_eq!(Wake::parse("sometimes"), None);
    }

    #[test]
    fn several_arrivals_make_one_wake_at_the_deepest_depth() {
        let mut idle = Idle::new(Wake::Questions, None);
        idle.judge(&[message(false, 0)]);
        assert!(idle.due.is_none());
        let mut second = message(true, 1);
        second.from = "other".into();
        idle.judge(&[message(false, 5), message(true, 0), second]);
        let due = idle.due.as_ref().unwrap();
        assert_eq!((due.from.as_str(), due.wake_depth), ("peer", 1));
    }
}
