//! The compaction threshold: how many tokens a request may come to before
//! the conversation is compacted (see [`compact`](super::compact)).
//!
//! What the user set wins, as it is: `ATELIER_CONTEXT_LIMIT`, `[defaults]
//! context_limit`, `/config`. Otherwise the threshold follows the model's
//! context window, where atelier knows it — [`WINDOW_PERCENT`] of it, the
//! rest being room for the answer and for what the next step adds — and is
//! [`FALLBACK`] where it doesn't. Either way it is at least
//! [`FLOOR_FACTOR`] times what every request carries before the
//! conversation starts (the system message and the tool list — some 7,500
//! tokens with the built-in tools alone, more with every MCP server): a
//! threshold near that fixed part would have each turn compacted as soon as
//! it ended.
//!
//! No table of models says how large a window is. atelier learns it from,
//! the most telling first:
//!
//! 1. **a refusal.** An endpoint that refuses a request as too long has
//!    said that the window is smaller than that request, and often says how
//!    large it is ("maximum context length is 8192 tokens"). The figure it
//!    gave — else the size of the request it refused — is the window from
//!    then on.
//! 2. **the provider's table**: `[[provider]] context_window`.
//! 3. **the endpoint**: the model's entry in `GET /models`, where it gives
//!    a size (see [`provider::advertised_window`]). Asked once per model,
//!    and only when it matters: the first time a request passes the
//!    threshold that stands without it.
//!
//! What was learned is about one model: it is forgotten when the main model
//! changes.

use super::Session;
use super::compact::{history_tokens, thousands};
use crate::config::Role;
use crate::provider;

/// The threshold while nothing is known about the model's window. Three
/// times what the built-in tools and the system prompt cost, and inside the
/// 32k window that is the least a model fit for this work is served with.
pub(crate) const FALLBACK: u32 = 24_000;

/// The share of a known context window the threshold is set at. It is
/// compared with the last request's prompt and answer together; the next
/// request adds to that the results of the tools just called and its own
/// answer, which is what the rest is for.
const WINDOW_PERCENT: u64 = 80;

/// The threshold is at least this many times the fixed part of a request.
const FLOOR_FACTOR: usize = 2;

/// How the size of the window came to be known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Source {
    /// The endpoint refused a longer request.
    Refusal,
    /// `[[provider]] context_window`.
    Provider,
    /// The endpoint's `GET /models`.
    Endpoint,
}

/// What a session has learned about its main model's context window.
#[derive(Debug, Default)]
pub(super) struct Learned {
    /// The model this is about (see [`Session::window_model`]).
    model: String,
    /// The window a refusal stated or implied.
    refused: Option<u32>,
    /// The window `GET /models` gave, once [`asked`](Self::asked).
    advertised: Option<u32>,
    asked: bool,
}

/// The threshold given what was set (`explicit`), the model's context
/// `window` if known, and the estimated size of the `fixed` part of a
/// request (see the module docs).
pub(super) fn threshold(explicit: Option<u32>, window: Option<u32>, fixed: usize) -> u32 {
    if let Some(set) = explicit {
        return set;
    }
    let floor = u32::try_from(fixed.saturating_mul(FLOOR_FACTOR)).unwrap_or(u32::MAX);
    match window {
        // A window too small for the floor is too small for the work; the
        // threshold is then the window itself, which the endpoint enforces.
        Some(window) => {
            let share = u64::from(window) * WINDOW_PERCENT / 100;
            (share as u32).max(floor).min(window)
        }
        None => FALLBACK.max(floor),
    }
}

impl Session {
    /// The main model as the window is known by: where it is served, and
    /// its id there. A variant is the same model with other options.
    fn window_model(&self) -> String {
        if self.on_chatgpt() {
            let (model, _) = self.cfg.model_and_options(self.main_model());
            return format!("chatgpt/{model}");
        }
        let target = self.cfg.target(Role::Main);
        format!("{}/{}", target.provider.name, target.model)
    }

    /// Whether the conversation's requests go to the ChatGPT backend.
    fn on_chatgpt(&self) -> bool {
        self.chatgpt.is_some() && !self.cfg.has_own_model(Role::Main)
    }

    /// What was learned, if it is about the model in use.
    fn learned(&self) -> Option<&Learned> {
        Some(&self.learned).filter(|l| l.model == self.window_model())
    }

    /// What was learned, for adding to: started over if the model changed.
    fn learned_mut(&mut self) -> &mut Learned {
        let model = self.window_model();
        if self.learned.model != model {
            self.learned = Learned {
                model,
                ..Learned::default()
            };
        }
        &mut self.learned
    }

    /// The main model's context window in tokens, and how it is known.
    pub(super) fn window(&self) -> Option<(u32, Source)> {
        let learned = self.learned();
        if let Some(n) = learned.and_then(|l| l.refused) {
            return Some((n, Source::Refusal));
        }
        let configured = match self.on_chatgpt() {
            true => None,
            false => self.cfg.target(Role::Main).provider.context_window,
        };
        if let Some(n) = configured {
            return Some((n, Source::Provider));
        }
        let advertised = learned.and_then(|l| l.advertised)?;
        Some((advertised, Source::Endpoint))
    }

    /// The compaction threshold in force (see the module docs).
    pub(super) fn compact_threshold(&self) -> u32 {
        match self.context_limit {
            Some(set) => set,
            None => threshold(None, self.window().map(|(n, _)| n), self.fixed_tokens()),
        }
    }

    /// The threshold that holds when nothing sets one.
    pub fn default_context_limit(&self) -> u32 {
        threshold(None, self.window().map(|(n, _)| n), self.fixed_tokens())
    }

    /// Ask the endpoint for the model's window, if that could change the
    /// threshold and was not done yet. Returns whether it gave one.
    pub(super) fn ask_window(&mut self) -> bool {
        if self.context_limit.is_some() || self.window().is_some() || self.on_chatgpt() {
            return false;
        }
        if self.learned().is_some_and(|l| l.asked) {
            return false;
        }
        let advertised = provider::advertised_window(&self.cfg.target(Role::Main));
        let learned = self.learned_mut();
        learned.asked = true;
        learned.advertised = advertised;
        advertised.is_some()
    }

    /// The endpoint refused the request the conversation now makes as too
    /// long (`err`): the window is what it says it is, else no larger than
    /// that request.
    pub(super) fn note_refusal(&mut self, err: &anyhow::Error) {
        let request = self.fixed_tokens() + history_tokens(&self.history);
        let bound = provider::stated_window(err)
            .unwrap_or_else(|| u32::try_from(request).unwrap_or(u32::MAX));
        let learned = self.learned_mut();
        learned.refused = Some(learned.refused.map_or(bound, |known| known.min(bound)));
    }

    /// One line for `/context` on where the threshold comes from.
    pub(super) fn threshold_note(&self) -> String {
        let limit = thousands(self.compact_threshold() as usize);
        if self.context_limit.is_some() {
            return format!("compaction threshold: {limit}, as set (context_limit)");
        }
        let Some((window, source)) = self.window() else {
            let floor = self.fixed_tokens() * FLOOR_FACTOR;
            return match floor > FALLBACK as usize {
                true => format!(
                    "compaction threshold: {limit} — twice the system message and tool list; \
                     the model's context window is not known (set context_limit, or \
                     context_window on its provider)"
                ),
                false => format!(
                    "compaction threshold: {limit} — the default while the model's context \
                     window is not known (set context_limit, or context_window on its provider)"
                ),
            };
        };
        let how = match source {
            Source::Refusal => "learned when the endpoint refused a longer request",
            Source::Provider => "the provider's context_window",
            Source::Endpoint => "as the endpoint lists it",
        };
        format!(
            "compaction threshold: {limit} — from the model's context window of {} tokens ({how})",
            thousands(window as usize)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{HttpError, Message, Provider};
    use crate::settings::{ProviderConfig, Settings};

    /// What is set is the threshold, whatever else is known — also where
    /// it is smaller than the fixed part of a request.
    #[test]
    fn what_is_set_wins() {
        assert_eq!(threshold(Some(50), Some(200_000), 7_500), 50);
        assert_eq!(threshold(Some(500_000), Some(8_192), 7_500), 500_000);
        assert_eq!(threshold(Some(8_000), None, 7_500), 8_000);
    }

    /// A known window gives its share; an unknown one the fallback.
    #[test]
    fn the_threshold_follows_the_window_where_it_is_known() {
        assert_eq!(threshold(None, Some(32_768), 7_500), 26_214);
        assert_eq!(threshold(None, Some(131_072), 7_500), 104_857);
        assert_eq!(threshold(None, Some(1_000_000), 7_500), 800_000);
        assert_eq!(threshold(None, Some(u32::MAX), 7_500), 3_435_973_836);
        assert_eq!(threshold(None, None, 7_500), FALLBACK);
        assert_eq!(threshold(None, None, 0), FALLBACK);
    }

    /// The threshold stays clear of what every request carries: twice the
    /// fixed part at least — but never past a window that is known.
    #[test]
    fn the_floor_is_twice_the_fixed_part_within_the_window() {
        // A tool list that outgrew the fallback (MCP servers).
        assert_eq!(threshold(None, None, 20_000), 40_000);
        assert_eq!(threshold(None, None, 12_000), FALLBACK);
        // A small window: its share is under the floor, which then holds…
        assert_eq!(threshold(None, Some(16_384), 7_500), 15_000);
        // …up to the window itself.
        assert_eq!(threshold(None, Some(8_192), 7_500), 8_192);
        assert_eq!(threshold(None, Some(4_096), 7_500), 4_096);
        for (window, fixed) in [
            (None, 3_000),
            (Some(32_768), 7_500),
            (Some(200_000), 60_000),
        ] {
            let limit = threshold(None, window, fixed) as usize;
            assert!(limit >= fixed * FLOOR_FACTOR, "{window:?} {fixed}");
        }
    }

    /// The default build's own prompt — the system message and the built-in
    /// tools — is what the fallback has to leave room after: it must not
    /// grow into it unnoticed.
    #[test]
    fn the_fallback_is_clear_of_the_builtin_tool_list() {
        let root = std::env::temp_dir().join("atl-limit-builtin");
        let mut tools = crate::tools::builtin_registry();
        tools.register(Box::new(crate::tools::task::TaskTool::new(
            root.clone(),
            crate::agents::global_dir(),
        )));
        let session = session(&root, None, tools);
        let fixed = session.fixed_tokens();
        assert!(fixed > 5_000, "the tool list is the bulk of it: {fixed}");
        assert!(
            fixed * 3 <= FALLBACK as usize,
            "the fixed part ({fixed} tokens) is more than a third of the fallback"
        );
        assert_eq!(session.compact_threshold(), FALLBACK);
    }

    fn session(
        root: &std::path::Path,
        context_window: Option<u32>,
        tools: crate::tools::ToolRegistry,
    ) -> Session {
        let provider = Provider::from_config(&ProviderConfig {
            name: "here".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key_env: None,
            api_key_command: None,
            headers: Default::default(),
            timeout_secs: None,
            context_window,
        })
        .unwrap();
        let cfg = crate::config::Config::single(provider, "m".into());
        let trust = crate::trust::testing::open(root);
        let mut session = Session::new(
            cfg,
            root.to_path_buf(),
            tools,
            Vec::new(),
            Settings::default(),
            trust,
        );
        // Whatever the environment of the test run says.
        session.context_limit = None;
        session
    }

    fn bare(context_window: Option<u32>) -> Session {
        let root = std::env::temp_dir().join("atl-limit-unused");
        session(&root, context_window, crate::tools::ToolRegistry::new())
    }

    fn refusal(message: &str) -> anyhow::Error {
        let body = serde_json::json!({"error": {"message": message}}).to_string();
        HttpError::read(400, body.as_bytes()).into()
    }

    /// The provider's `context_window` sets the threshold; a set limit
    /// still wins over it.
    #[test]
    fn a_providers_context_window_sets_the_threshold() {
        let mut s = bare(Some(100_000));
        assert_eq!(s.window(), Some((100_000, Source::Provider)));
        assert_eq!(s.compact_threshold(), 80_000);
        assert!(
            s.threshold_note().contains("80,000"),
            "{}",
            s.threshold_note()
        );
        s.set_context_limit(5_000);
        assert_eq!(s.compact_threshold(), 5_000);
        assert_eq!(s.default_context_limit(), 80_000);
    }

    /// A refusal that gives the window's size is believed over everything
    /// but a set limit; one that gives none bounds the window by the
    /// request that was refused. Neither outlives a change of model.
    #[test]
    fn a_refusal_teaches_the_window() {
        let mut s = bare(Some(100_000));
        s.note_refusal(&refusal(
            "This model's maximum context length is 16384 tokens.",
        ));
        assert_eq!(s.window(), Some((16_384, Source::Refusal)));
        assert_eq!(s.compact_threshold(), 13_107);
        // A later, larger figure does not raise it again.
        s.note_refusal(&refusal("maximum context length is 65536 tokens"));
        assert_eq!(s.window(), Some((16_384, Source::Refusal)));

        s.set_model("other");
        assert_eq!(s.window(), Some((100_000, Source::Provider)));

        let mut s = bare(None);
        assert_eq!(s.window(), None);
        s.history.push(Message::user("x".repeat(40_000)));
        let request = s.fixed_tokens() + history_tokens(&s.history);
        s.note_refusal(&refusal("the prompt is too long"));
        assert_eq!(s.window(), Some((request as u32, Source::Refusal)));
        assert!(s.compact_threshold() < request as u32);
    }

    /// The endpoint is asked for the window once, and not at all when the
    /// answer could change nothing. (Nothing listens at the test's
    /// endpoint: the answer is "unknown", and the fallback stands.)
    #[test]
    fn the_endpoint_is_asked_once_and_only_when_it_matters() {
        let mut s = bare(Some(100_000));
        assert!(!s.ask_window());
        assert!(!s.learned.asked, "the provider's table already says");

        let mut s = bare(None);
        s.set_context_limit(9_000);
        assert!(!s.ask_window());
        assert!(!s.learned.asked, "a set limit wins whatever the answer");

        let mut s = bare(None);
        assert!(!s.ask_window());
        assert!(s.learned.asked);
        assert_eq!(s.compact_threshold(), FALLBACK);
        assert!(s.threshold_note().contains("not known"));
    }
}
