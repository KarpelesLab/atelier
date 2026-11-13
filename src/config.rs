//! Runtime configuration: which backend, which endpoints, which model for
//! which job.
//!
//! # Providers
//!
//! The environment (`ATELIER_BASE_URL`, `ATELIER_API_KEY`) describes one
//! endpoint, as it always has; it is the provider named `env`, and the
//! default one. `[[provider]]` tables in `atelier.toml` — the user-global
//! file, then the project's, a project entry replacing a global one of the
//! same name — add more (see [`Provider`]). `[defaults] provider` makes
//! another one the default.
//!
//! # Addressing a model
//!
//! A model is written `provider/model#variant`; both the prefix and the
//! suffix are optional. The spelling has to coexist with model ids that
//! contain those characters themselves (`meta/llama-3`, as an aggregator
//! names them), so the prefix is taken as a provider only when it names a
//! configured one, and the suffix as a variant only when it names a known
//! one. Anything else is part of the model id and goes to the default
//! provider — which is what a setup that predates providers relies on.
//!
//! # Variants
//!
//! A variant is a set of request-body fields (`reasoning_effort`,
//! `temperature`, …) from a `[variant.<name>]` table; `low`, `medium` and
//! `high` exist without being configured and set `reasoning_effort`. They
//! travel as [`Target::options`] and are merged into the request body by the
//! provider code — never into the messages, so the prompt a server caches is
//! the same whatever the variant.
//!
//! # Roles
//!
//! `[models]` assigns a model per [`Role`]. The main model is also set by
//! `ATELIER_MODEL` (which wins), `[defaults] model`, and `/model` during a
//! session; the others fall back towards it, so a file with no `[models]`
//! table sends every request where it always went.
//!
//! None of this applies to the Claude backend, where the `claude` CLI picks
//! and calls its own models.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::provider::endpoint::{ENV_PROVIDER, Provider, Target};
use crate::settings::{ModelRoles, Settings, VariantConfig};

/// Default OpenAI-compatible endpoint (our test server).
pub const DEFAULT_BASE_URL: &str = "http://192.168.0.50:11400/v1";
/// Default model — thinking- and vision-capable.
pub const DEFAULT_MODEL: &str = "qwen3.8-unc:q4";
/// Default Claude Code executable for the Claude backend.
pub const DEFAULT_CLAUDE_BIN: &str = "claude";

/// The variants that exist without a `[variant.*]` table: reasoning effort.
const BUILTIN_VARIANTS: [&str; 3] = ["low", "medium", "high"];

/// Where turns are sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    /// An OpenAI-compatible chat-completions endpoint, driven by atelier's own
    /// agent loop.
    OpenAi,
    /// The user's installed Claude Code CLI, driven as a subprocess (see
    /// `src/claude`).
    Claude {
        /// The `claude` executable.
        bin: String,
        /// `--model` to pass, if the user chose one.
        model: Option<String>,
        /// Keep claude's built-in tools that overlap atelier's (by default
        /// they're switched off in favor of atelier's).
        keep_builtins: bool,
    },
}

/// What a model request is for. Each role can have its own model in
/// `[models]`; an unset one falls back as noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The conversation.
    Main,
    /// Cheap upkeep work (summaries; titles and memory upkeep to come).
    /// Falls back to main.
    Small,
    /// Subagents started by the `task` tool. Falls back to main.
    Subagent,
    /// The background reviewer. Falls back to `[review] model`, then main.
    Review,
    /// Compaction's hand-off summary. Falls back to small, then main.
    Summary,
}

impl Role {
    pub const ALL: [Role; 5] = [
        Role::Main,
        Role::Small,
        Role::Subagent,
        Role::Review,
        Role::Summary,
    ];

    /// The role's key in `[models]`.
    pub fn name(self) -> &'static str {
        match self {
            Role::Main => "main",
            Role::Small => "small",
            Role::Subagent => "subagent",
            Role::Review => "review",
            Role::Summary => "summary",
        }
    }
}

/// A model address taken apart (see the module docs for the rules).
#[derive(Debug, PartialEq)]
pub struct ModelRef<'a> {
    /// The provider named in the address, if one was.
    pub provider: Option<&'a str>,
    pub model: &'a str,
    pub variant: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The main model as addressed (`provider/model#variant`, or any part
    /// of that), or the display label of the model serving the session
    /// (Claude backend).
    pub model: String,
    pub backend: Backend,
    /// `ATELIER_MODEL` chose the model: `atelier.toml` doesn't get to.
    model_from_env: bool,
    /// The configured endpoints; `env` is always among them.
    providers: Vec<Arc<Provider>>,
    /// Index in `providers` of the one a bare model name is on.
    default_provider: usize,
    /// `[models]`, with `[review] model` folded into `review`. `main` is
    /// not kept here: it becomes [`Config::model`].
    roles: ModelRoles,
    /// Request options by variant name.
    variants: BTreeMap<String, Map<String, Value>>,
}

impl Config {
    /// Build config from the environment, falling back to defaults.
    ///
    /// - `ATELIER_BACKEND`    — `claude` for the Claude backend (also selected
    ///   by `claude`, e.g. from the `--claude` flag); anything else or unset
    ///   means OpenAI-compatible
    /// - `ATELIER_BASE_URL`   — endpoint base (default [`DEFAULT_BASE_URL`])
    /// - `ATELIER_MODEL`      — model (default [`DEFAULT_MODEL`]); with the
    ///   Claude backend, passed as `--model` only when set
    /// - `ATELIER_API_KEY`    — bearer token (optional)
    /// - `ATELIER_CLAUDE_BIN` — Claude Code executable (default
    ///   [`DEFAULT_CLAUDE_BIN`])
    /// - `ATELIER_CLAUDE_BUILTINS` — `keep` to leave claude's own file/shell/
    ///   fetch tools enabled next to atelier's (default: switched off)
    ///
    /// The result knows the `env` provider only; [`Config::apply_settings`]
    /// adds what `atelier.toml` configures.
    pub fn from_env(claude: bool) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let base_url = env("ATELIER_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let api_key = env("ATELIER_API_KEY");
        let model_env = env("ATELIER_MODEL");
        let claude = claude || env("ATELIER_BACKEND").as_deref() == Some("claude");
        let (backend, model) = if claude {
            let bin = env("ATELIER_CLAUDE_BIN").unwrap_or_else(|| DEFAULT_CLAUDE_BIN.to_string());
            let label = model_env.clone().unwrap_or_else(|| "claude".to_string());
            (
                Backend::Claude {
                    bin,
                    model: model_env,
                    keep_builtins: matches!(
                        env("ATELIER_CLAUDE_BUILTINS").as_deref(),
                        Some("keep" | "on" | "1" | "yes")
                    ),
                },
                label,
            )
        } else {
            (
                Backend::OpenAi,
                model_env.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            )
        };
        Self {
            backend,
            // Set at all, as before providers: an empty value still keeps
            // `[defaults] model` out, leaving the built-in default.
            model_from_env: std::env::var_os("ATELIER_MODEL").is_some(),
            ..Self::single(Provider::from_env(base_url, api_key), model)
        }
    }

    /// A configuration with one provider and nothing else set.
    pub(crate) fn single(provider: Provider, model: String) -> Self {
        let variants = BUILTIN_VARIANTS
            .iter()
            .map(|&effort| {
                let mut options = Map::new();
                options.insert("reasoning_effort".into(), json!(effort));
                (effort.to_string(), options)
            })
            .collect();
        Self {
            model,
            backend: Backend::OpenAi,
            model_from_env: false,
            providers: vec![Arc::new(provider)],
            default_provider: 0,
            roles: ModelRoles::default(),
            variants,
        }
    }

    /// Lay the model tables of `atelier.toml` over the environment: the
    /// project's file and the user-global one, the project winning wherever
    /// both name the same thing. Returns what was wrong with them, for the
    /// caller to print; a faulty entry is skipped, the rest still applies.
    pub fn apply_settings(&mut self, project: &Settings, user: &Settings) -> Vec<String> {
        // claude picks and calls its own models (see the module docs).
        if matches!(self.backend, Backend::Claude { .. }) {
            return Vec::new();
        }
        let mut warnings = Vec::new();

        for table in user.provider.iter().chain(&project.provider) {
            let provider = match Provider::from_config(table) {
                Ok(p) if p.name == ENV_PROVIDER => {
                    warnings.push(format!(
                        "provider name '{ENV_PROVIDER}' is taken by the endpoint the environment \
                         describes; pick another"
                    ));
                    continue;
                }
                Ok(p) => Arc::new(p),
                Err(e) => {
                    warnings.push(format!("{e:#}"));
                    continue;
                }
            };
            match self.providers.iter_mut().find(|p| p.name == provider.name) {
                Some(slot) => *slot = provider,
                None => self.providers.push(provider),
            }
        }

        let default = (project.defaults.provider.as_deref()).or(user.defaults.provider.as_deref());
        if let Some(name) = default {
            match self.providers.iter().position(|p| p.name == name) {
                Some(i) => self.default_provider = i,
                None => warnings.push(format!(
                    "[defaults] provider = \"{name}\" names no configured provider; \
                     using '{ENV_PROVIDER}'"
                )),
            }
        }

        for (name, table) in user.variant.iter().chain(&project.variant) {
            if name.is_empty() || name.contains(['/', '#']) {
                warnings.push(format!("variant name {name:?} can't contain '/' or '#'"));
                continue;
            }
            self.variants.insert(name.clone(), variant_options(table));
        }

        let pick = |role: fn(&ModelRoles) -> &Option<String>| {
            role(&project.models)
                .clone()
                .or_else(|| role(&user.models).clone())
        };
        self.roles = ModelRoles {
            main: None,
            small: pick(|m| &m.small),
            subagent: pick(|m| &m.subagent),
            review: pick(|m| &m.review).or_else(|| project.review.model.clone()),
            summary: pick(|m| &m.summary),
        };
        // ATELIER_MODEL wins; then the role, then the older spelling of it.
        if !self.model_from_env
            && let Some(main) = pick(|m| &m.main)
                .or_else(|| project.defaults.model.clone())
                .or_else(|| user.defaults.model.clone())
        {
            self.model = main;
        }

        warnings
    }

    /// Take a model address apart (see the module docs).
    pub fn parse<'a>(&self, spec: &'a str) -> ModelRef<'a> {
        let (rest, variant) = match spec.rsplit_once('#') {
            Some((rest, v)) if !rest.is_empty() && self.variants.contains_key(v) => (rest, Some(v)),
            _ => (spec, None),
        };
        let (provider, model) = match rest.split_once('/') {
            Some((p, m)) if !m.is_empty() && self.providers.iter().any(|x| x.name == p) => {
                (Some(p), m)
            }
            _ => (None, rest),
        };
        ModelRef {
            provider,
            model,
            variant,
        }
    }

    /// Where a request for the model addressed as `spec` goes.
    pub fn resolve(&self, spec: &str) -> Target {
        let parsed = self.parse(spec);
        let provider = parsed
            .provider
            .and_then(|name| self.providers.iter().find(|p| p.name == name))
            .unwrap_or(&self.providers[self.default_provider]);
        Target {
            provider: Arc::clone(provider),
            model: parsed.model.to_string(),
            options: parsed
                .variant
                .and_then(|v| self.variants.get(v))
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// The model configured for `role` itself (or for the role it falls
    /// back to, short of main), as addressed.
    fn own_model(&self, role: Role) -> Option<&str> {
        let r = &self.roles;
        match role {
            Role::Main => None,
            Role::Small => r.small.as_deref(),
            Role::Subagent => r.subagent.as_deref(),
            Role::Review => r.review.as_deref(),
            Role::Summary => r.summary.as_deref().or(r.small.as_deref()),
        }
    }

    /// The model serving `role`, as addressed.
    pub fn role_model(&self, role: Role) -> &str {
        self.own_model(role).unwrap_or(&self.model)
    }

    /// Whether `role` was given a model of its own, rather than following
    /// the main one. (A session signed in with ChatGPT sends the main
    /// model's requests there, and only those.)
    pub fn has_own_model(&self, role: Role) -> bool {
        self.own_model(role).is_some()
    }

    /// Where a request made for `role` goes.
    pub fn target(&self, role: Role) -> Target {
        self.resolve(self.role_model(role))
    }

    /// `spec` addressed with another variant, or with none.
    pub fn with_variant(&self, spec: &str, variant: Option<&str>) -> String {
        let base = match self.parse(spec).variant {
            Some(v) => &spec[..spec.len() - v.len() - 1],
            None => spec,
        };
        match variant {
            Some(v) => format!("{base}#{v}"),
            None => base.to_string(),
        }
    }

    /// `spec` as a model id and its variant's request options, ignoring
    /// providers: for the ChatGPT backend, which is its own endpoint.
    pub fn model_and_options<'a>(&self, spec: &'a str) -> (&'a str, Map<String, Value>) {
        match self.parse(spec).variant {
            Some(v) => (
                &spec[..spec.len() - v.len() - 1],
                self.variants.get(v).cloned().unwrap_or_default(),
            ),
            None => (spec, Map::new()),
        }
    }

    /// A `#suffix` on `spec` that names no variant — so is being sent as
    /// part of the model id, which after a typo is not what was meant.
    pub fn stray_variant<'a>(&self, spec: &'a str) -> Option<&'a str> {
        let (_, suffix) = spec.rsplit_once('#')?;
        (!self.variants.contains_key(suffix)).then_some(suffix)
    }

    /// The known variant names, built-in ones included.
    pub fn variant_names(&self) -> Vec<&str> {
        self.variants.keys().map(String::as_str).collect()
    }

    /// The configured endpoints, `env` first.
    pub fn providers(&self) -> &[Arc<Provider>] {
        &self.providers
    }

    /// `/providers`: each endpoint with where its key comes from (never the
    /// key), then which model serves which role.
    pub fn describe(&self) -> Vec<String> {
        let mut out = vec!["providers:".to_string()];
        let width = self.providers.iter().map(|p| p.name.len()).max();
        let width = width.unwrap_or(0);
        for (i, p) in self.providers.iter().enumerate() {
            let default = if i == self.default_provider {
                "  (default)"
            } else {
                ""
            };
            out.push(format!(
                "  {:<width$}  {}  key: {}{default}",
                p.name,
                p.base_url,
                p.key_description(),
            ));
        }
        out.push("models:".to_string());
        for role in Role::ALL {
            let target = self.target(role);
            let options = if target.options.is_empty() {
                String::new()
            } else {
                format!("  {}", Value::Object(target.options))
            };
            out.push(format!(
                "  {:<8}  {}/{}{options}",
                role.name(),
                target.provider.name,
                target.model,
            ));
        }
        out
    }
}

/// The request-body fields a `[variant.<name>]` table stands for. `extra`
/// is applied last, so it can express anything the named fields can.
fn variant_options(table: &VariantConfig) -> Map<String, Value> {
    let mut options = Map::new();
    let named = [
        (
            "reasoning_effort",
            table.reasoning_effort.as_ref().map(|v| json!(v)),
        ),
        ("temperature", table.temperature.map(|v| json!(v))),
        ("top_p", table.top_p.map(|v| json!(v))),
        ("max_tokens", table.max_tokens.map(|v| json!(v))),
    ];
    for (key, value) in named {
        if let Some(value) = value {
            options.insert(key.to_string(), value);
        }
    }
    for (key, value) in &table.extra {
        options.insert(key.clone(), value.clone());
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(toml: &str) -> Settings {
        tomlproc::serde::from_str(toml).expect("valid settings")
    }

    /// A config for the `env` endpoint with `project` (and `user`) applied.
    fn config(project: &str, user: &str) -> (Config, Vec<String>) {
        let env = Provider::from_env("http://env.test/v1".into(), None);
        let mut cfg = Config::single(env, "base-model".into());
        let warnings = cfg.apply_settings(&settings(project), &settings(user));
        (cfg, warnings)
    }

    const TWO_PROVIDERS: &str = r#"
[[provider]]
name = "local"
base_url = "http://local.test/v1"

[[provider]]
name = "second"
base_url = "http://second.test/v1"
api_key_env = "SECOND_KEY"
"#;

    /// With no model tables, every role is the one model on the one
    /// endpoint, and nothing is added to the request: the setup that
    /// predates providers.
    #[test]
    fn an_env_only_setup_resolves_as_before() {
        let (cfg, warnings) = config("", "");
        assert!(warnings.is_empty(), "{warnings:?}");
        for role in Role::ALL {
            let t = cfg.target(role);
            assert_eq!(t.provider.name, "env");
            assert_eq!(t.provider.base_url, "http://env.test/v1");
            assert_eq!(t.model, "base-model");
            assert!(t.options.is_empty());
            assert!(!cfg.has_own_model(role));
        }
        // Ids with a slash or a hash of their own stay whole.
        for id in ["meta/llama-3", "org/name:tag", "weird#id", "a/b#c"] {
            let t = cfg.resolve(id);
            assert_eq!((t.provider.name.as_str(), t.model.as_str()), ("env", id));
        }
    }

    #[test]
    fn addresses_split_only_on_known_providers_and_variants() {
        let (cfg, warnings) = config(TWO_PROVIDERS, "");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            cfg.parse("second/qwen3-coder:30b#high"),
            ModelRef {
                provider: Some("second"),
                model: "qwen3-coder:30b",
                variant: Some("high"),
            }
        );
        let t = cfg.resolve("second/org/model-b");
        assert_eq!(t.provider.base_url, "http://second.test/v1");
        assert_eq!(t.model, "org/model-b");
        // An unknown prefix is part of the id, on the default provider.
        let t = cfg.resolve("nowhere/m#medium");
        assert_eq!(t.provider.name, "env");
        assert_eq!(t.model, "nowhere/m");
        assert_eq!(t.options["reasoning_effort"], "medium");
        assert_eq!(cfg.stray_variant("m#hihg"), Some("hihg"));
        assert_eq!(cfg.stray_variant("m#high"), None);
        assert_eq!(cfg.stray_variant("m"), None);
    }

    #[test]
    fn project_providers_replace_global_ones_by_name() {
        let user = r#"
[defaults]
provider = "local"

[[provider]]
name = "local"
base_url = "http://global.test/v1"

[[provider]]
name = "mine"
base_url = "http://mine.test/v1"
"#;
        let (cfg, warnings) = config(TWO_PROVIDERS, user);
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<_> = cfg.providers().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["env", "local", "mine", "second"]);
        // The default came from the global file; its URL from the project's.
        let t = cfg.resolve("m");
        assert_eq!(t.provider.base_url, "http://local.test/v1");
        assert_eq!(cfg.resolve("env/m").provider.base_url, "http://env.test/v1");
    }

    #[test]
    fn faulty_entries_are_reported_and_skipped() {
        let (cfg, warnings) = config(
            r#"
[defaults]
provider = "nope"

[[provider]]
name = "env"
base_url = "http://shadow.test/v1"

[[provider]]
name = "ok"
base_url = "http://ok.test/v1"
"#,
            "",
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(cfg.resolve("m").provider.base_url, "http://env.test/v1");
        assert_eq!(cfg.resolve("ok/m").provider.base_url, "http://ok.test/v1");
    }

    #[test]
    fn roles_fall_back_towards_the_main_model() {
        let (mut cfg, _) = config(
            &format!(
                "{TWO_PROVIDERS}\n[models]\nmain = \"local/big#high\"\nsmall = \"second/tiny\"\n\
                 [review]\nmodel = \"local/critic\"\n"
            ),
            "[models]\nsubagent = \"local/worker\"\nsmall = \"ignored\"\n",
        );
        let at = |cfg: &Config, role| {
            let t = cfg.target(role);
            format!("{}/{}", t.provider.name, t.model)
        };
        assert_eq!(cfg.model, "local/big#high");
        assert_eq!(at(&cfg, Role::Main), "local/big");
        assert_eq!(cfg.target(Role::Main).options["reasoning_effort"], "high");
        assert_eq!(at(&cfg, Role::Small), "second/tiny");
        // summary -> small; subagent from the global file; review from the
        // older `[review] model`.
        assert_eq!(at(&cfg, Role::Summary), "second/tiny");
        assert!(cfg.target(Role::Summary).options.is_empty());
        assert_eq!(at(&cfg, Role::Subagent), "local/worker");
        assert_eq!(at(&cfg, Role::Review), "local/critic");
        assert!(cfg.has_own_model(Role::Summary) && !cfg.has_own_model(Role::Main));

        // `/model` moves the main model; roles with their own stay put.
        cfg.model = "second/other".into();
        assert_eq!(at(&cfg, Role::Main), "second/other");
        assert_eq!(at(&cfg, Role::Subagent), "local/worker");

        // Unset, they follow it: subagent and review -> main,
        // summary -> small -> main.
        let (cfg, _) = config("[models]\nreview = \"r\"\nsummary = \"s\"\n", "");
        assert_eq!(at(&cfg, Role::Subagent), "env/base-model");
        assert_eq!(at(&cfg, Role::Small), "env/base-model");
        assert_eq!(at(&cfg, Role::Summary), "env/s");
        assert_eq!(at(&cfg, Role::Review), "env/r");
    }

    #[test]
    fn variants_carry_request_options() {
        let (cfg, warnings) = config(
            r#"
[variant.creative]
temperature = 1.1
top_p = 0.9
max_tokens = 4096
extra = { top_k = 40, temperature = 1.2 }

[variant.high]
reasoning_effort = "xhigh"
"#,
            "[variant.terse]\nmax_tokens = 200\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let options = cfg.resolve("m#creative").options;
        assert_eq!(options["top_p"], 0.9);
        assert_eq!(options["max_tokens"], 4096);
        assert_eq!(options["top_k"], 40);
        // `extra` has the last word.
        assert_eq!(options["temperature"], 1.2);
        // A built-in can be redefined; the others stay.
        assert_eq!(cfg.resolve("m#high").options["reasoning_effort"], "xhigh");
        assert_eq!(cfg.resolve("m#low").options["reasoning_effort"], "low");
        assert_eq!(cfg.resolve("m#terse").options["max_tokens"], 200);
        assert_eq!(
            cfg.variant_names(),
            ["creative", "high", "low", "medium", "terse"]
        );

        assert_eq!(cfg.with_variant("p/m#low", Some("high")), "p/m#high");
        assert_eq!(cfg.with_variant("p/m", Some("terse")), "p/m#terse");
        assert_eq!(cfg.with_variant("p/m#creative", None), "p/m");
        assert_eq!(cfg.with_variant("odd#id", None), "odd#id");
    }

    #[test]
    fn describe_lists_providers_and_roles_without_keys() {
        let env = Provider::from_env("http://env.test/v1".into(), Some("sk-secret".into()));
        let mut cfg = Config::single(env, "m#low".into());
        cfg.apply_settings(&settings(TWO_PROVIDERS), &Settings::default());
        let text = cfg.describe().join("\n");
        assert!(text.contains("env     http://env.test/v1  key: ATELIER_API_KEY  (default)"));
        assert!(text.contains("second  http://second.test/v1  key: $SECOND_KEY"));
        assert!(text.contains("subagent  env/m  {\"reasoning_effort\":\"low\"}"));
        assert!(!text.contains("sk-secret"), "{text}");
    }
}
