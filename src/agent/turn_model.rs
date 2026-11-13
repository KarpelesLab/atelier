//! A model for one turn: `model:` in a command file's front matter.
//!
//! A command file may say which model its prompt is for — a cheap one for a
//! chore, a stronger one for a review (`model: provider/model#variant`, as
//! `/model` takes it). That choice belongs to the command, not the session:
//! it holds for the turn the command starts and the session's own model is
//! back afterwards, however the turn ended.
//!
//! So the switch is a pair. [`Session::enter_turn_model`] makes it and
//! returns the [`TurnModel`] to hand back to [`Session::leave_turn_model`];
//! [`Session::send_expansion`] is the two around a `send`, for an interface
//! with nothing to do in between. The TUI calls them itself, to redraw its
//! status strip — which names the session's current model, so it shows the
//! temporary one while the turn runs. The restore is not left to `send`:
//! `send` returns an error for a failed or interrupted turn, and the model
//! must come back on that path as on any other.
//!
//! With the Claude backend a model change means relaunching claude
//! (`Session::set_model`), twice for one turn. That is not worth a command's
//! preference, so there the key is reported and ignored.

use anyhow::Result;

use super::{Expansion, Session, Ui};

/// What [`Session::enter_turn_model`] changed, for
/// [`Session::leave_turn_model`] to undo: the model the session was on, or
/// nothing when it was left alone.
#[must_use = "hand it to `leave_turn_model` when the turn is over"]
pub struct TurnModel(Option<String>);

impl Session {
    /// Switch to `model` — what a command asks for — for the turn about to
    /// start. `None`, or the model already in use, changes nothing.
    pub fn enter_turn_model(&mut self, model: Option<&str>, ui: &mut dyn Ui) -> TurnModel {
        let Some(model) = model.filter(|m| *m != self.main_model()) else {
            return TurnModel(None);
        };
        if self.is_claude() {
            ui.info(&format!(
                "model: {model} ignored — a per-command model is not supported with the \
                 Claude backend (it would relaunch claude for one turn)"
            ));
            return TurnModel(None);
        }
        let previous = self.main_model().to_string();
        self.set_model(model);
        ui.info(&format!("model: {model} for this turn"));
        TurnModel(Some(previous))
    }

    /// Put back the model [`enter_turn_model`](Self::enter_turn_model)
    /// replaced, now that its turn is over.
    pub fn leave_turn_model(&mut self, entered: TurnModel) {
        if let Some(previous) = entered.0 {
            self.set_model(&previous);
        }
    }

    /// Send the prompt a command expanded into, on the command's model if it
    /// names one. The session's model is restored whether the turn finished,
    /// failed or was interrupted.
    pub fn send_expansion(&mut self, expansion: &Expansion, ui: &mut dyn Ui) -> Result<()> {
        let entered = self.enter_turn_model(expansion.model.as_deref(), ui);
        let result = self.send(&expansion.prompt, ui);
        self.leave_turn_model(entered);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Approval;
    use crate::config::{Backend, Config};
    use crate::settings::Settings;
    use crate::tools::ToolRegistry;

    /// Keeps what a command printed.
    #[derive(Default)]
    struct Lines(Vec<String>);

    impl Ui for Lines {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, _: &str) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &str, _: bool) {}
        fn info(&mut self, text: &str) {
            self.0.push(text.to_string());
        }
        fn ask_approval(&mut self, _: &str, _: &str) -> Approval {
            Approval::Deny
        }
        fn notice(&mut self, _: &str) {}
    }

    /// A session on `cfg` that talks to nothing.
    fn session(cfg: Config) -> Session {
        let root = std::env::temp_dir();
        Session::new(
            cfg,
            root.clone(),
            ToolRegistry::new(),
            Vec::new(),
            Settings::default(),
            crate::trust::testing::open(&root),
        )
    }

    fn unreachable(model: &str) -> Config {
        let provider = crate::provider::Provider::from_env("http://127.0.0.1:9/v1".into(), None);
        Config::single(provider, model.into())
    }

    #[test]
    fn the_commands_model_holds_until_the_turn_is_left() {
        let mut session = session(unreachable("base"));
        let mut ui = Lines::default();

        let entered = session.enter_turn_model(Some("other#high"), &mut ui);
        assert_eq!(session.main_model(), "other#high");
        assert_eq!(session.config().model, "other#high", "what the strip shows");
        assert_eq!(ui.0, ["model: other#high for this turn"]);
        session.leave_turn_model(entered);
        assert_eq!(session.main_model(), "base");

        // No model, or the one in use: nothing to do, nothing said.
        for same in [None, Some("base")] {
            let entered = session.enter_turn_model(same, &mut ui);
            assert!(entered.0.is_none());
            session.leave_turn_model(entered);
        }
        assert_eq!(session.main_model(), "base");
        assert_eq!(ui.0.len(), 1);
    }

    #[test]
    fn a_failed_turn_still_gives_the_model_back() {
        // Nothing listens on the port: the request fails, and so the turn.
        let mut session = session(unreachable("base"));
        let mut ui = Lines::default();
        let expansion = Expansion {
            prompt: "hello".into(),
            model: Some("other".into()),
        };
        assert!(session.send_expansion(&expansion, &mut ui).is_err());
        assert_eq!(session.main_model(), "base");
    }

    #[test]
    fn the_claude_backend_keeps_its_model_and_says_so() {
        let mut cfg = unreachable("base");
        cfg.backend = Backend::Claude {
            bin: "claude".into(),
            model: None,
            keep_builtins: false,
        };
        let mut session = session(cfg);
        let mut ui = Lines::default();

        let entered = session.enter_turn_model(Some("opus"), &mut ui);
        assert!(entered.0.is_none());
        assert_eq!(session.main_model(), "base");
        assert!(!session.claude_restart, "claude is not relaunched");
        assert_eq!(ui.0.len(), 1);
        assert!(ui.0[0].starts_with("model: opus ignored"), "{:?}", ui.0);
        assert!(ui.0[0].contains("not supported with the Claude backend"));
    }
}
