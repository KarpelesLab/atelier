//! Tests for the GitHub Action (`action.yml`, `.github/action/*.sh`; see
//! `docs/github-action.md`), run without GitHub.
//!
//! The scripts are driven the way a runner drives them — inputs as
//! `INPUT_*` variables, the event as a JSON file, `GITHUB_OUTPUT` and
//! `GITHUB_STEP_SUMMARY` as files — with two stubs first on `PATH`:
//!
//! - `gh` records every API call (method, path, request body, the token it
//!   was given) in `calls.jsonl` and answers from canned response files;
//! - `atelier` records its arguments, stdin and environment, prints a fixture
//!   and exits with a chosen status.
//!
//! `git` is the real one, on a throwaway repository. Nothing here touches the
//! network. The tests need `bash`, `jq` and `git` (the action's own
//! requirements, present on every GitHub runner) and skip when one is
//! missing.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use serde_json::{Value, json};

const REPO: &str = "acme/widgets";
const TOKEN: &str = "ghs_action_test_token";
const MARKER: &str = "<!-- atelier-review -->";
const SUPERSEDED: &str = "<!-- atelier-review:superseded -->";
const REPLY: &str = "<!-- atelier-reply -->";

const GH_STUB: &str = r#"#!/usr/bin/env bash
set -euo pipefail
d=$GH_STUB_DIR
method=GET path='' input=''
while [ $# -gt 0 ]; do
  case $1 in
    api | --paginate) ;;
    --method) method=$2; shift ;;
    --input) input=$2; shift ;;
    *) path=$1 ;;
  esac
  shift
done
body=null
if [ -n "$input" ]; then body=$(cat "$input"); fi
jq -cn --arg method "$method" --arg path "$path" --arg token "${GH_TOKEN:-}" \
  --argjson body "$body" '{method: $method, path: $path, token: $token, body: $body}' \
  >>"$d/calls.jsonl"
# With `reject-inline`, behave like GitHub given a comment on a line outside
# the diff: refuse the whole review.
if [ -e "$d/reject-inline" ] && [ "$method" = POST ] &&
  [ "$(jq '.comments | length' <<<"$body")" != 0 ]; then
  echo "HTTP 422: Unprocessable Entity" >&2
  exit 1
fi
if [ -e "$d/fail-all" ]; then
  echo "HTTP 403: Resource not accessible by integration" >&2
  exit 1
fi
key=$(printf '%s %s' "$method" "${path%%\?*}" | tr -c 'A-Za-z0-9' '_')
if [ -f "$d/responses/$key" ]; then
  cat "$d/responses/$key"
elif [ "$method" = GET ]; then
  echo '[]'
else
  echo '{}'
fi
"#;

const ATELIER_STUB: &str = r#"#!/usr/bin/env bash
d=$ATELIER_STUB_DIR
printf '%s\0' "$@" >"$d/atelier-args"
cat >"$d/atelier-stdin"
env >"$d/atelier-env"
git rev-parse HEAD >"$d/atelier-head" 2>/dev/null || true
# What a model (or a file it prints) could write to the job log.
echo "::error::written by the model" >&2
cat "$d/atelier-stdout" 2>/dev/null || true
exit "$(cat "$d/atelier-exit" 2>/dev/null || echo 0)"
"#;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What a script printed and how it ended.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn all(&self) -> String {
        format!(
            "exit {}\n--- stdout\n{}\n--- stderr\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

/// A fake runner: a temp directory with the stubs, a git repository and the
/// files a runner provides.
struct Runner {
    dir: PathBuf,
    env: HashMap<String, String>,
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Runner {
    /// `None` (after saying so) when the tools the action needs are missing.
    fn new(name: &str) -> Option<Runner> {
        for tool in ["bash", "jq", "git"] {
            if !have(tool) {
                eprintln!("skipping: `{tool}` is not installed");
                return None;
            }
        }
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "atelier-action-{name}-{}-{nanos}",
            std::process::id()
        ));
        for sub in ["bin", "stub/responses", "repo", "home", "tmp", "tools"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let runner = Runner {
            dir,
            env: HashMap::new(),
        };
        runner.stub("gh", GH_STUB);
        runner.stub("atelier", ATELIER_STUB);
        Some(runner)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn repo(&self) -> PathBuf {
        self.path("repo")
    }

    /// A file no test creates: if it exists afterwards, text from the event
    /// was executed.
    fn canary(&self) -> PathBuf {
        self.path("canary")
    }

    /// Text that does something in every shell context it could wrongly be
    /// expanded in: command substitution, backticks, a quote that ends a
    /// string, a line break that ends a command.
    fn hostile(&self) -> String {
        let c = self.canary();
        let c = c.display();
        format!("$(touch {c}) `touch {c}` \"; touch {c}; ' ; touch {c}\ntouch {c}\n$(touch {c})")
    }

    fn stub(&self, name: &str, source: &str) {
        let path = self.path("bin").join(name);
        fs::write(&path, source).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn set(&mut self, key: &str, value: &str) -> &mut Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }

    fn event(&mut self, name: &str, payload: &Value) -> &mut Self {
        fs::write(self.path("event.json"), payload.to_string()).unwrap();
        self.set("GITHUB_EVENT_NAME", name)
    }

    /// What the `gh` stub answers to `METHOD path`.
    fn respond(&self, method: &str, path: &str, body: &Value) {
        let key: String = format!("{method} {path}")
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        fs::write(self.path("stub/responses").join(key), body.to_string()).unwrap();
    }

    /// What the `atelier` stub prints and exits with.
    fn atelier_says(&self, stdout: &Value, code: i32) {
        fs::write(self.path("stub/atelier-stdout"), stdout.to_string()).unwrap();
        fs::write(self.path("stub/atelier-exit"), code.to_string()).unwrap();
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.env_clear()
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .current_dir(self.repo());
        cmd
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Run one of the action's scripts as a runner step would.
    fn script(&self, name: &str, args: &[&str]) -> Run {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".github/action")
            .join(name);
        let mut cmd = self.command("bash");
        cmd.arg(script)
            .args(args)
            .env("RUNNER_TEMP", self.path("tmp"))
            .env("RUNNER_TOOL_CACHE", self.path("tools"))
            .env("GITHUB_OUTPUT", self.path("output"))
            .env("GITHUB_STEP_SUMMARY", self.path("summary"))
            .env("GITHUB_PATH", self.path("github-path"))
            .env("GITHUB_REPOSITORY", REPO)
            .env("GITHUB_EVENT_PATH", self.path("event.json"))
            .env("GH_STUB_DIR", self.path("stub"))
            .env("ATELIER_STUB_DIR", self.path("stub"))
            .env("ATELIER_BIN", self.path("bin/atelier"))
            .env("INPUT_GITHUB_TOKEN", TOKEN)
            .envs(&self.env)
            .stdin(Stdio::null());
        let out = cmd.output().unwrap();
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn run(&self) -> Run {
        self.script("run.sh", &[])
    }

    /// Every call the `gh` stub received, in order.
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.path("stub/calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("a recorded call"))
            .collect()
    }

    fn calls_to(&self, method: &str, path: &str) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|c| c["method"] == method && c["path"].as_str().unwrap().starts_with(path))
            .collect()
    }

    fn read(&self, rel: &str) -> Option<String> {
        fs::read(self.path(rel))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// The arguments atelier was run with, or `None` if it never ran.
    fn atelier_args(&self) -> Option<Vec<String>> {
        self.read("stub/atelier-args").map(|s| {
            s.split('\0')
                .filter(|a| !a.is_empty())
                .map(String::from)
                .collect()
        })
    }

    /// The environment atelier ran with, as its `NAME=value` lines.
    fn atelier_env(&self) -> Vec<String> {
        self.read("stub/atelier-env")
            .expect("atelier ran")
            .lines()
            .map(String::from)
            .collect()
    }

    /// The step outputs written to `GITHUB_OUTPUT` (`name<<delimiter` form).
    fn outputs(&self) -> HashMap<String, String> {
        let text = self.read("output").unwrap_or_default();
        let mut out = HashMap::new();
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            let Some((name, delim)) = line.split_once("<<") else {
                panic!("unexpected line in GITHUB_OUTPUT: {line:?}");
            };
            let mut value = Vec::new();
            for l in lines.by_ref() {
                if l == delim {
                    break;
                }
                value.push(l);
            }
            out.insert(name.to_string(), value.join("\n").trim_end().to_string());
        }
        out
    }

    /// A repository with a `main` that `origin/main` points to and a
    /// checked-out `feature` branch that changes line 3 of `src/lib.rs` and
    /// adds `src/new.rs`. Returns the head commit.
    fn pull_request_repo(&self) -> String {
        let lines = |changed: bool| {
            (1..=20)
                .map(|n| {
                    if changed && n == 3 {
                        "line 3, changed\n".to_string()
                    } else {
                        format!("line {n}\n")
                    }
                })
                .collect::<String>()
        };
        self.git(&["init", "--quiet", "--initial-branch=main"]);
        fs::create_dir_all(self.repo().join("src")).unwrap();
        fs::write(self.repo().join("src/lib.rs"), lines(false)).unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "--quiet", "-m", "base"]);
        self.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        self.git(&["checkout", "--quiet", "-b", "feature"]);
        fs::write(self.repo().join("src/lib.rs"), lines(true)).unwrap();
        fs::write(self.repo().join("src/new.rs"), "fn new() {}\n").unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "--quiet", "-m", "feature"]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// A `pull_request` event for [`Runner::pull_request_repo`], whose title
    /// and head branch name are hostile.
    fn pull_request_event(&mut self, head: &str) {
        let hostile = self.hostile();
        self.event(
            "pull_request",
            &json!({
                "action": "synchronize",
                "number": 7,
                "pull_request": {
                    "number": 7,
                    "title": hostile,
                    "body": hostile,
                    "base": { "ref": "main", "repo": { "full_name": REPO } },
                    "head": { "ref": hostile, "sha": head, "repo": { "full_name": REPO } }
                }
            }),
        );
    }
}

/// `atelier review --output-format json` for the fixture repository: one
/// finding on the changed line, one in the added file, one on a line far
/// from any change, one in a file the pull request does not touch.
fn review_fixture() -> Value {
    json!({
        "findings": [
            { "file": "src/lib.rs", "line": 15, "severity": "P2",
              "summary": "unchanged line", "scenario": "far from the diff" },
            { "file": "src/lib.rs", "line": 3, "severity": "P1",
              "summary": "changed line is wrong", "scenario": "whenever it runs" },
            { "file": "src/other.rs", "line": 4, "severity": "P3",
              "summary": "untouched file", "scenario": "" },
            { "file": "src/new.rs", "line": 1, "severity": "P0",
              "summary": "new file is wrong", "scenario": "always" }
        ],
        "reviewed": { "target": "base", "base": "origin/main",
                      "files": ["src/lib.rs", "src/new.rs"], "truncated": false, "omitted": [] }
    })
}

fn comment_event(association: &str, body: &str, on_pull_request: bool) -> Value {
    let mut issue = json!({ "number": 7, "title": "A title", "body": "A description" });
    if on_pull_request {
        issue["pull_request"] = json!({ "url": "https://api.github.invalid/pulls/7" });
    }
    json!({
        "action": "created",
        "issue": issue,
        "comment": {
            "id": 4242,
            "body": body,
            "author_association": association,
            "user": { "login": "someone", "type": "User" }
        }
    })
}

// ------------------------------------------------------------------ review

#[test]
fn review_posts_one_review_with_inline_comments_on_diff_lines_only() {
    let Some(mut r) = Runner::new("review") else {
        return;
    };
    let head = r.pull_request_repo();
    r.pull_request_event(&head);
    r.atelier_says(&review_fixture(), 0);
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());

    // atelier: a read-only review against the fetched base.
    let args = r.atelier_args().expect("atelier ran");
    assert_eq!(
        args,
        [
            "review",
            "--output-format",
            "json",
            "--max-turns",
            "30",
            "--base",
            "origin/main"
        ]
    );
    let env = r.atelier_env();
    assert!(env.contains(&"ATELIER_MODE=plan".to_string()), "{env:?}");

    // GitHub: exactly one review.
    let posts = r.calls_to("POST", "repos/acme/widgets/pulls/7/reviews");
    assert_eq!(posts.len(), 1, "{:?}", r.calls());
    let review = &posts[0]["body"];
    assert_eq!(posts[0]["token"], TOKEN);
    assert_eq!(review["commit_id"], head);
    assert_eq!(review["event"], "COMMENT");

    // Findings on lines of the diff are inline, worst first, each marked.
    let comments = review["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert_eq!(comments[0]["path"], "src/new.rs");
    assert_eq!(comments[0]["line"], 1);
    assert!(comments[0]["body"].as_str().unwrap().starts_with("**P0** "));
    assert_eq!(comments[1]["path"], "src/lib.rs");
    assert_eq!(comments[1]["line"], 3);
    assert_eq!(comments[1]["side"], "RIGHT");
    let inline = comments[1]["body"].as_str().unwrap();
    assert!(
        inline.starts_with("**P1** changed line is wrong"),
        "{inline}"
    );
    assert!(inline.contains("**Fails when:** whenever it runs"));
    assert!(inline.ends_with(MARKER));

    // The others are in the body, not lost and not inline.
    let body = review["body"].as_str().unwrap();
    assert!(
        body.contains("4 findings (1 P0, 1 P1, 1 P2, 1 P3)"),
        "{body}"
    );
    assert!(body.contains("**P2** `src/lib.rs:15` — unchanged line"));
    assert!(body.contains("**P3** `src/other.rs:4` — untouched file"));
    assert!(!body.contains("changed line is wrong"));
    assert!(body.ends_with(MARKER));

    // Outputs: the findings as atelier gave them, and the status.
    let out = r.outputs();
    let findings: Value = serde_json::from_str(&out["findings"]).unwrap();
    assert_eq!(findings, review_fixture()["findings"]);
    assert_eq!(out["exit-code"], "0");
    assert!(out["result"].contains("`src/new.rs:1`"));

    // The hostile title and branch name of the event ran nothing.
    assert!(!r.canary().exists());
}

#[test]
fn a_second_run_replaces_the_first_review_instead_of_adding_to_it() {
    let Some(mut r) = Runner::new("rerun") else {
        return;
    };
    let head = r.pull_request_repo();
    r.pull_request_event(&head);
    r.atelier_says(&review_fixture(), 0);
    // The pull request already has: a review of an earlier run (11), a
    // person's review (12), and a review an earlier run superseded (13).
    r.respond(
        "GET",
        "repos/acme/widgets/pulls/7/reviews",
        &json!([
            { "id": 11, "body": format!("### atelier review\n\nold\n\n{MARKER}") },
            { "id": 12, "body": "Looks good to me" },
            { "id": 13, "body": format!("Superseded.\n\n{SUPERSEDED}") }
        ]),
    );
    r.respond(
        "GET",
        "repos/acme/widgets/pulls/7/reviews/11/comments",
        &json!([
            { "id": 101, "body": format!("**P1** old finding\n\n{MARKER}") },
            { "id": 102, "body": "a person's reply in the thread" }
        ]),
    );
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());

    let calls = r.calls();
    let made: Vec<String> = calls
        .iter()
        .filter(|c| c["method"] != "GET")
        .map(|c| {
            format!(
                "{} {}",
                c["method"].as_str().unwrap(),
                c["path"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        made,
        [
            // One new review…
            "POST repos/acme/widgets/pulls/7/reviews",
            // …the old run's inline comment goes (the person's reply stays)…
            "DELETE repos/acme/widgets/pulls/comments/101",
            // …and its review body becomes a one-line note.
            "PUT repos/acme/widgets/pulls/7/reviews/11",
        ],
        "{calls:?}"
    );
    let put = &r.calls_to("PUT", "repos/acme/widgets/pulls/7/reviews/11")[0];
    let note = put["body"]["body"].as_str().unwrap();
    assert!(
        note.contains("Superseded") && note.ends_with(SUPERSEDED),
        "{note}"
    );
    // The note no longer carries the live marker: a third run leaves it be.
    assert!(!note.contains(MARKER));
}

#[test]
fn a_clean_rerun_retires_the_old_review_and_posts_nothing_new() {
    let Some(mut r) = Runner::new("clean-rerun") else {
        return;
    };
    let head = r.pull_request_repo();
    r.pull_request_event(&head);
    r.atelier_says(
        &json!({ "findings": [], "reviewed": { "target": "base" } }),
        0,
    );
    r.respond(
        "GET",
        "repos/acme/widgets/pulls/7/reviews",
        &json!([{ "id": 11, "body": format!("old\n\n{MARKER}") }]),
    );
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        r.calls_to("POST", "").is_empty(),
        "no findings, no review: {:?}",
        r.calls()
    );
    let put = &r.calls_to("PUT", "repos/acme/widgets/pulls/7/reviews/11")[0];
    assert!(
        put["body"]["body"]
            .as_str()
            .unwrap()
            .contains("reported no findings")
    );
}

#[test]
fn a_review_github_refuses_is_posted_again_with_everything_in_the_body() {
    let Some(mut r) = Runner::new("fallback") else {
        return;
    };
    let head = r.pull_request_repo();
    r.pull_request_event(&head);
    r.atelier_says(&review_fixture(), 0);
    fs::write(r.path("stub/reject-inline"), "").unwrap();
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());
    let posts = r.calls_to("POST", "repos/acme/widgets/pulls/7/reviews");
    assert_eq!(posts.len(), 2);
    let second = &posts[1]["body"];
    assert!(second.get("comments").is_none());
    let body = second["body"].as_str().unwrap();
    assert!(
        body.contains("`src/lib.rs:3` — changed line is wrong"),
        "{body}"
    );
    assert!(body.contains("`src/new.rs:1`"));
}

#[test]
fn on_a_merge_checkout_files_that_differ_from_the_head_are_not_commented_inline() {
    let Some(mut r) = Runner::new("merge-checkout") else {
        return;
    };
    // What actions/checkout does by default: the base moved on (it changed
    // src/lib.rs too), and HEAD is a merge of the head into it.
    let head = r.pull_request_repo();
    r.git(&["checkout", "--quiet", "main"]);
    let text = fs::read_to_string(r.repo().join("src/lib.rs")).unwrap();
    let text = text.replace("line 18\n", "line 18, on main\n");
    fs::write(r.repo().join("src/lib.rs"), text).unwrap();
    r.git(&["commit", "--quiet", "-am", "main moves on"]);
    r.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    r.git(&["checkout", "--quiet", "--detach"]);
    r.git(&["merge", "--quiet", "--no-ff", "-m", "merge", "feature"]);
    r.pull_request_event(&head);
    r.atelier_says(&review_fixture(), 0);
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());

    let posts = r.calls_to("POST", "repos/acme/widgets/pulls/7/reviews");
    assert_eq!(posts.len(), 1);
    let review = &posts[0]["body"];
    // Comments are attached to the head commit, not to the merge.
    assert_eq!(review["commit_id"], head);
    // src/new.rs is the same in both: inline. src/lib.rs is not: its line
    // numbers at the merge are not to be trusted on the head.
    let comments = review["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0]["path"], "src/new.rs");
    let body = review["body"].as_str().unwrap();
    assert!(
        body.contains("`src/lib.rs:3` — changed line is wrong"),
        "{body}"
    );
}

#[test]
fn fail_on_fails_the_step_after_the_review_is_posted() {
    let Some(mut r) = Runner::new("fail-on") else {
        return;
    };
    let head = r.pull_request_repo();
    r.pull_request_event(&head);
    r.atelier_says(&review_fixture(), 5);
    r.set("INPUT_FAIL_ON", "P1");
    let run = r.run();
    assert_eq!(run.code, 5, "{}", run.all());
    let args = r.atelier_args().unwrap();
    assert!(
        args.windows(2).any(|w| w == ["--fail-on", "P1"]),
        "{args:?}"
    );
    assert_eq!(
        r.calls_to("POST", "repos/acme/widgets/pulls/7/reviews")
            .len(),
        1
    );
    assert_eq!(r.outputs()["exit-code"], "5");
    assert!(
        run.stdout
            .contains("::error title=atelier::atelier exited with status 5")
    );
}

#[test]
fn post_summary_writes_the_step_summary_and_calls_no_api() {
    let Some(mut r) = Runner::new("summary") else {
        return;
    };
    let head = r.pull_request_repo();
    r.pull_request_event(&head);
    r.atelier_says(&review_fixture(), 0);
    r.set("INPUT_POST", "summary");
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(r.calls().is_empty(), "{:?}", r.calls());
    let summary = r.read("summary").unwrap();
    assert!(summary.contains("### atelier review"), "{summary}");
    assert!(summary.contains("**P1** `src/lib.rs:3` — changed line is wrong"));
    // The marker is for what is posted, not for the summary.
    assert!(!summary.contains(MARKER));
}

#[test]
fn a_review_that_cannot_be_posted_falls_back_to_the_summary() {
    let Some(mut r) = Runner::new("readonly-token") else {
        return;
    };
    let head = r.pull_request_repo();
    r.pull_request_event(&head);
    r.atelier_says(&review_fixture(), 0);
    fs::write(r.path("stub/fail-all"), "").unwrap();
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout
            .contains("::warning title=atelier::could not post the review")
    );
    assert!(r.read("summary").unwrap().contains("changed line is wrong"));
}

#[test]
fn a_hostile_base_branch_name_is_a_ref_that_does_not_exist_and_nothing_more() {
    let Some(mut r) = Runner::new("hostile-base") else {
        return;
    };
    let head = r.pull_request_repo();
    let hostile = r.hostile();
    r.event(
        "pull_request",
        &json!({
            "pull_request": {
                "number": 7,
                "base": { "ref": hostile },
                "head": { "ref": "feature", "sha": head, "repo": { "full_name": REPO } }
            }
        }),
    );
    r.set("INPUT_EXTRA_ARGS", &hostile);
    let run = r.run();
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stdout
            .contains("could not fetch the pull request's base branch")
    );
    assert!(r.atelier_args().is_none(), "atelier must not have run");
    assert!(!r.canary().exists(), "the branch name was executed");
}

#[test]
fn findings_map_through_a_working_directory_and_skip_drifted_files() {
    let Some(r) = Runner::new("map") else {
        return;
    };
    // atelier ran in `crate/`: its paths are relative to that directory.
    fs::write(
        r.path("review.json"),
        json!({ "findings": [
            { "file": "src/a.rs", "line": 2, "severity": "P1", "summary": "in a", "scenario": "s" },
            { "file": "src/b.rs", "line": 1, "severity": "P1", "summary": "in b", "scenario": "s" },
            { "file": "src/a.rs", "line": 0, "severity": "P9", "summary": "no line", "scenario": "" }
        ]})
        .to_string(),
    )
    .unwrap();
    // A diff with a line that looks like a header inside a hunk (`+++ x`
    // is the added line `++ x`) and a deleted file.
    let diff = "\
diff --git a/crate/src/a.rs b/crate/src/a.rs
--- a/crate/src/a.rs
+++ b/crate/src/a.rs
@@ -1,2 +1,3 @@
 one
+++ x
 two
diff --git a/crate/src/b.rs b/crate/src/b.rs
--- a/crate/src/b.rs
+++ b/crate/src/b.rs
@@ -1 +1 @@
-old
+new
diff --git a/crate/gone.rs b/crate/gone.rs
--- a/crate/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
    fs::write(r.path("pr.diff"), diff).unwrap();
    // b.rs is not, where atelier read it, what the head commit has.
    fs::write(r.path("drift"), "crate/src/b.rs\n").unwrap();
    let run = r.script(
        "map-findings.sh",
        &[
            "--review",
            r.path("review.json").to_str().unwrap(),
            "--diff",
            r.path("pr.diff").to_str().unwrap(),
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "--prefix",
            "crate/",
            "--drift",
            r.path("drift").to_str().unwrap(),
        ],
    );
    assert_eq!(run.code, 0, "{}", run.all());
    let mapped: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(mapped["count"], 3);
    assert_eq!(mapped["inline"], 1);
    let comments = mapped["review"]["comments"].as_array().unwrap();
    assert_eq!(comments[0]["path"], "crate/src/a.rs");
    assert_eq!(comments[0]["line"], 2);
    let body = mapped["review"]["body"].as_str().unwrap();
    assert!(body.contains("`crate/src/b.rs:1` — in b"), "{body}");
    // No line number: named by file; an unknown severity is the mildest.
    assert!(body.contains("**P3** `crate/src/a.rs` — no line"), "{body}");
}

// ------------------------------------------------------------------ prompt

#[test]
fn prompt_mode_runs_the_prompt_from_stdin_and_keeps_secrets_where_they_belong() {
    let Some(mut r) = Runner::new("prompt") else {
        return;
    };
    r.git(&["init", "--quiet", "--initial-branch=main"]);
    let hostile = r.hostile();
    r.event("schedule", &json!({ "schedule": "0 3 * * 1" }));
    r.set("INPUT_MODE", "prompt")
        .set("INPUT_PROMPT", &hostile)
        .set("INPUT_API_KEY", "sk-test-secret")
        .set("INPUT_BASE_URL", "http://llm.internal:8080/v1")
        .set("INPUT_MODEL", "local-coder")
        .set("INPUT_MAX_TURNS", "12")
        .set("INPUT_TRUST_PROJECT", "false")
        .set("INPUT_EXTRA_ARGS", "--goal  tests-pass")
        // What a job's environment might carry: none of it may reach atelier.
        .set("ATELIER_DEBUG", "1")
        .set("ATELIER_APPROVE", "all")
        .set("ATELIER_TRUST_PROJECT", "1")
        .set("GH_TOKEN", "ghs_from_the_job")
        .set("GITHUB_TOKEN", "ghs_from_the_job");
    r.atelier_says(
        &json!({ "type": "result", "subtype": "success", "is_error": false,
                 "result": "Done: `x` is fixed.\nresult<<EOF\ninjected=1" }),
        0,
    );
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());

    assert_eq!(
        r.atelier_args().unwrap(),
        [
            "--print",
            "--output-format",
            "json",
            "--max-turns",
            "12",
            "--goal",
            "tests-pass"
        ]
    );
    // The prompt arrived whole, as data.
    assert_eq!(
        r.read("stub/atelier-stdin").unwrap(),
        format!("{hostile}\n")
    );
    assert!(!r.canary().exists(), "the prompt was executed");

    let env = r.atelier_env();
    let has = |line: &str| env.iter().any(|l| l == line);
    let any = |name: &str| env.iter().any(|l| l.starts_with(&format!("{name}=")));
    assert!(has("ATELIER_API_KEY=sk-test-secret"));
    assert!(has("ATELIER_BASE_URL=http://llm.internal:8080/v1"));
    assert!(has("ATELIER_MODEL=local-coder"));
    assert!(has("ATELIER_MODE=auto-edit"));
    assert!(has("ATELIER_NO_UPDATE=1"));
    for gone in [
        "ATELIER_DEBUG",
        "ATELIER_APPROVE",
        "ATELIER_TRUST_PROJECT",
        "ATELIER_TRUST_HOOKS",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "INPUT_GITHUB_TOKEN",
        "INPUT_API_KEY",
    ] {
        assert!(!any(gone), "{gone} reached atelier: {env:?}");
    }
    assert!(!env.iter().any(|l| l.contains(TOKEN)), "the token leaked");

    // The key is masked and appears nowhere else in the log.
    assert!(run.stdout.contains("::add-mask::sk-test-secret"));
    assert_eq!(run.stdout.matches("sk-test-secret").count(), 1);
    assert!(!run.stderr.contains("sk-test-secret"));

    // What atelier writes to the log is fenced off from workflow commands.
    let stop = run
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("::stop-commands::"))
        .expect("stop-commands");
    assert_eq!(stop.len(), 32);
    assert!(run.stdout.contains(&format!("::{stop}::")));

    // The result is an output and in the summary; text that imitates the
    // output file's syntax stays inside the value.
    let out = r.outputs();
    assert_eq!(
        out["result"],
        "Done: `x` is fixed.\nresult<<EOF\ninjected=1"
    );
    assert!(!out.contains_key("injected"));
    assert_eq!(out["exit-code"], "0");
    assert!(r.read("summary").unwrap().contains("Done: `x` is fixed."));
    assert!(r.calls().is_empty(), "prompt mode posts nothing");
}

#[test]
fn prompt_mode_reports_a_run_that_did_not_finish_and_trusts_only_when_told() {
    let Some(mut r) = Runner::new("prompt-max-turns") else {
        return;
    };
    r.git(&["init", "--quiet", "--initial-branch=main"]);
    r.event("workflow_dispatch", &json!({}));
    r.set("INPUT_MODE", "prompt")
        .set("INPUT_PROMPT", "tidy up")
        .set("INPUT_TRUST_PROJECT", "true")
        .set("INPUT_PERMISSION_MODE", "yolo");
    r.atelier_says(
        &json!({ "type": "result", "subtype": "max_turns", "is_error": true,
                 "result": "half done", "error": "stopped after 30 steps" }),
        3,
    );
    let run = r.run();
    assert_eq!(run.code, 3, "{}", run.all());
    let out = r.outputs();
    assert!(out["result"].contains("half done"));
    assert!(out["result"].contains("max_turns"));
    assert_eq!(out["exit-code"], "3");
    let env = r.atelier_env();
    assert!(env.contains(&"ATELIER_TRUST_PROJECT=1".to_string()));
    assert!(env.contains(&"ATELIER_MODE=yolo".to_string()));
}

#[test]
fn bad_inputs_stop_before_anything_runs() {
    let Some(mut r) = Runner::new("inputs") else {
        return;
    };
    r.event("workflow_dispatch", &json!({}));
    for (key, value, message) in [
        ("INPUT_MODE", "shell", "mode: expected"),
        ("INPUT_PERMISSION_MODE", "pln", "permission-mode: expected"),
        ("INPUT_MAX_TURNS", "1; touch x", "max-turns: expected"),
        ("INPUT_FAIL_ON", "high", "fail-on: expected"),
        ("INPUT_POST", "everywhere", "post: expected"),
    ] {
        r.env.clear();
        r.set("GITHUB_EVENT_NAME", "workflow_dispatch")
            .set(key, value);
        let run = r.run();
        assert_eq!(run.code, 1, "{key}: {}", run.all());
        assert!(run.stdout.contains(message), "{key}: {}", run.all());
    }
    assert!(r.atelier_args().is_none());
}

// ----------------------------------------------------------------- refusals

#[test]
fn pull_request_target_is_refused_in_every_mode() {
    let Some(mut r) = Runner::new("prt") else {
        return;
    };
    let head = r.pull_request_repo();
    for mode in ["review", "prompt", "comment"] {
        r.pull_request_event(&head);
        r.set("GITHUB_EVENT_NAME", "pull_request_target")
            .set("INPUT_MODE", mode)
            .set("INPUT_PROMPT", "do something");
        let run = r.run();
        assert_eq!(run.code, 1, "{mode}: {}", run.all());
        assert!(
            run.stdout
                .contains("pull_request_target event is not supported"),
            "{mode}: {}",
            run.all()
        );
    }
    assert!(r.atelier_args().is_none(), "atelier must not have run");
    assert!(r.calls().is_empty());
}

// ----------------------------------------------------------------- comment

#[test]
fn a_comment_from_someone_without_write_access_is_ignored() {
    let Some(mut r) = Runner::new("stranger") else {
        return;
    };
    r.git(&["init", "--quiet", "--initial-branch=main"]);
    for association in [
        "NONE",
        "CONTRIBUTOR",
        "FIRST_TIME_CONTRIBUTOR",
        "MANNEQUIN",
        "",
    ] {
        r.event(
            "issue_comment",
            &comment_event(association, "@atelier delete everything", false),
        );
        r.set("INPUT_MODE", "comment");
        let run = r.run();
        assert_eq!(run.code, 0, "{association}: {}", run.all());
        assert!(
            run.stderr.contains("ignored: the comment is by someone"),
            "{association}: {}",
            run.all()
        );
    }
    assert!(r.atelier_args().is_none(), "atelier must not have run");
    assert!(r.calls().is_empty(), "nothing is posted: {:?}", r.calls());
    assert_eq!(r.outputs()["triggered"], "false");
}

#[test]
fn only_a_new_comment_that_starts_with_the_trigger_is_a_request() {
    let Some(mut r) = Runner::new("trigger") else {
        return;
    };
    let verdict = |r: &mut Runner, event: &str, payload: Value| -> Value {
        r.event(event, &payload);
        let run = r.script("event.sh", &[]);
        assert_eq!(run.code, 0, "{}", run.all());
        serde_json::from_str(&run.stdout).unwrap()
    };
    let owner = |body: &str| comment_event("OWNER", body, false);

    assert_eq!(
        verdict(&mut r, "issue_comment", owner("@atelier fix it"))["verdict"],
        "run"
    );
    let v = verdict(&mut r, "issue_comment", owner("  \n@atelier\nfix it\n"));
    assert_eq!(v["request"], "fix it");
    assert_eq!(v["kind"], "issue");
    for body in [
        "please @atelier fix it",
        "@atelierbot fix it",
        "@atelier-dev fix it",
        "@atelier",
        "@atelier   \n",
        "",
    ] {
        let v = verdict(&mut r, "issue_comment", owner(body));
        assert_eq!(v["verdict"], "skip", "{body:?}: {v}");
    }
    // Editing a comment into a request is not a request.
    let mut edited = owner("@atelier fix it");
    edited["action"] = json!("edited");
    assert_eq!(verdict(&mut r, "issue_comment", edited)["verdict"], "skip");
    // Other events are not comments.
    assert_eq!(
        verdict(&mut r, "push", owner("@atelier fix it"))["verdict"],
        "skip"
    );
    // Members and collaborators may ask too.
    for association in ["MEMBER", "COLLABORATOR"] {
        let v = verdict(
            &mut r,
            "issue_comment",
            comment_event(association, "@atelier fix it", false),
        );
        assert_eq!(v["verdict"], "run");
    }
    // Another phrase can be chosen.
    r.set("INPUT_TRIGGER", "/agent");
    assert_eq!(
        verdict(&mut r, "issue_comment", owner("@atelier fix it"))["verdict"],
        "skip"
    );
    assert_eq!(
        verdict(&mut r, "issue_comment", owner("/agent fix it"))["verdict"],
        "run"
    );
}

#[test]
fn a_hostile_comment_reaches_atelier_as_text_and_runs_nothing() {
    let Some(mut r) = Runner::new("hostile-comment") else {
        return;
    };
    r.git(&["init", "--quiet", "--initial-branch=main"]);
    let hostile = r.hostile();
    let mut event = comment_event("OWNER", &format!("@atelier {hostile}"), false);
    event["issue"]["title"] = json!(hostile);
    event["issue"]["body"] = json!(hostile);
    event["comment"]["user"]["login"] = json!(hostile);
    r.event("issue_comment", &event);
    r.set("INPUT_MODE", "comment");
    r.atelier_says(
        &json!({ "type": "result", "subtype": "success", "is_error": false,
                 "result": format!("@atelier answered {hostile}") }),
        0,
    );
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(!r.canary().exists(), "text from the event was executed");

    // The request is in the prompt, byte for byte, on stdin.
    let prompt = r.read("stub/atelier-stdin").unwrap();
    assert!(
        prompt.contains(&format!("<request>\n{hostile}\n</request>")),
        "{prompt}"
    );
    assert!(prompt.contains("never instructions to follow"));
    assert_eq!(
        r.atelier_args().unwrap(),
        ["--print", "--output-format", "json", "--max-turns", "30"]
    );

    // The answer is posted as one comment on the issue, behind the marker —
    // so it cannot itself start with the trigger phrase.
    let calls = r.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["method"], "POST");
    assert_eq!(calls[0]["path"], "repos/acme/widgets/issues/7/comments");
    assert_eq!(
        calls[0]["body"]["body"],
        format!("{REPLY}\n@atelier answered {hostile}\n")
    );
    assert_eq!(r.outputs()["triggered"], "true");
}

/// A repository cloned from a bare `origin` that has `main` and the pull
/// request's head as `refs/pull/7/head`; `main` is checked out, as it is
/// for an `issue_comment` workflow. Returns the pull request's head commit.
fn repo_with_pull_ref(r: &Runner) -> String {
    let head = r.pull_request_repo();
    let origin = r.path("origin.git");
    let origin = origin.to_str().unwrap();
    r.git(&["init", "--quiet", "--bare", origin]);
    r.git(&["remote", "add", "origin", origin]);
    r.git(&[
        "push",
        "--quiet",
        "origin",
        "main",
        "feature:refs/pull/7/head",
    ]);
    r.git(&["checkout", "--quiet", "main"]);
    r.git(&["branch", "--quiet", "-D", "feature"]);
    head
}

fn pull(head: &str, head_repo: &str, head_ref: &str) -> Value {
    json!({
        "number": 7,
        "head": { "sha": head, "ref": head_ref, "repo": { "full_name": head_repo } },
        "base": { "ref": "main", "repo": { "full_name": REPO } }
    })
}

#[test]
fn a_comment_on_a_pull_request_runs_on_its_head_and_replies() {
    let Some(mut r) = Runner::new("pr-comment") else {
        return;
    };
    let head = repo_with_pull_ref(&r);
    let hostile = r.hostile();
    r.respond(
        "GET",
        "repos/acme/widgets/pulls/7",
        &pull(&head, REPO, &hostile),
    );
    r.event(
        "issue_comment",
        &comment_event("COLLABORATOR", "@atelier add a test", true),
    );
    r.set("INPUT_MODE", "comment");
    r.atelier_says(
        &json!({ "type": "result", "subtype": "success", "is_error": false, "result": "Added." }),
        0,
    );
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());

    // atelier ran with the pull request's head checked out.
    assert_eq!(r.read("stub/atelier-head").unwrap().trim(), head);
    assert!(r.read("stub/atelier-stdin").unwrap().contains("add a test"));
    let made: Vec<String> = r
        .calls()
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                c["method"].as_str().unwrap(),
                c["path"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        made,
        [
            "GET repos/acme/widgets/pulls/7",
            "POST repos/acme/widgets/issues/7/comments"
        ]
    );
    // The branch name is handed on as data, and ran nothing on the way.
    assert_eq!(r.outputs()["head-ref"], hostile);
    assert!(!r.canary().exists());
}

#[test]
fn a_comment_on_a_fork_pull_request_is_refused() {
    let Some(mut r) = Runner::new("fork") else {
        return;
    };
    let head = repo_with_pull_ref(&r);
    let main = r.git(&["rev-parse", "HEAD"]);
    r.respond(
        "GET",
        "repos/acme/widgets/pulls/7",
        &pull(&head, "mallory/widgets", "patch-1"),
    );
    r.event(
        "issue_comment",
        &comment_event("OWNER", "@atelier run the tests", true),
    );
    r.set("INPUT_MODE", "comment")
        .set("INPUT_TRUST_PROJECT", "true");
    let run = r.run();
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stdout.contains("comes from a fork"), "{}", run.all());
    assert!(r.atelier_args().is_none(), "atelier must not have run");
    assert_eq!(
        r.git(&["rev-parse", "HEAD"]),
        main,
        "nothing was checked out"
    );
    // The person who asked is told why.
    let posts = r.calls_to("POST", "repos/acme/widgets/issues/7/comments");
    assert_eq!(posts.len(), 1);
    let body = posts[0]["body"]["body"].as_str().unwrap();
    assert!(body.contains("Not run") && body.contains("fork"), "{body}");
    assert_eq!(r.outputs()["triggered"], "false");

    // With allow-forks the run happens — and the fork's atelier.toml is
    // still not trusted, whatever trust-project says.
    r.set("INPUT_ALLOW_FORKS", "true");
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());
    assert_eq!(r.read("stub/atelier-head").unwrap().trim(), head);
    assert!(run.stdout.contains("trust-project is ignored"));
    assert!(
        !r.atelier_env()
            .iter()
            .any(|l| l.starts_with("ATELIER_TRUST_PROJECT=")),
        "a fork's project file was trusted"
    );
}

#[test]
fn a_review_comment_is_answered_in_its_thread() {
    let Some(mut r) = Runner::new("review-comment") else {
        return;
    };
    let head = repo_with_pull_ref(&r);
    r.event(
        "pull_request_review_comment",
        &json!({
            "action": "created",
            "pull_request": {
                "number": 7, "title": "T", "body": "B",
                "head": { "sha": head, "ref": "feature", "repo": { "full_name": REPO } }
            },
            "comment": {
                "id": 555, "body": "@atelier why this line?", "author_association": "MEMBER",
                "path": "src/lib.rs", "line": 3, "diff_hunk": "@@ -1,3 +1,3 @@",
                "user": { "login": "dev" }
            }
        }),
    );
    r.set("INPUT_MODE", "comment");
    r.atelier_says(
        &json!({ "type": "result", "subtype": "success", "is_error": false, "result": "Because." }),
        0,
    );
    let run = r.run();
    assert_eq!(run.code, 0, "{}", run.all());
    let prompt = r.read("stub/atelier-stdin").unwrap();
    assert!(
        prompt.contains("The comment is on src/lib.rs line 3"),
        "{prompt}"
    );
    let calls = r.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        calls[0]["path"],
        "repos/acme/widgets/pulls/7/comments/555/replies"
    );
}

// ----------------------------------------------------------------- install

/// A release directory as `install.sh` downloads from it: the asset, and
/// `SHA256SUMS` listing `sum_of` (the asset itself, or something else).
/// `None` when the tools to build one are missing.
fn release(r: &Runner, listed_as: &str, tamper: bool) -> Option<String> {
    for tool in ["gzip", "sha256sum", "curl"] {
        if !have(tool) {
            eprintln!("skipping: `{tool}` is not installed");
            return None;
        }
    }
    // The runner under test is always "Linux x86_64".
    r.stub(
        "uname",
        "#!/bin/sh\ncase $1 in -s) echo Linux ;; -m) echo x86_64 ;; esac\n",
    );
    let dir = r.path("releases/download/v9.9.9");
    fs::create_dir_all(&dir).unwrap();
    fs::write(r.path("fake-atelier"), "#!/bin/sh\necho atelier 9.9.9\n").unwrap();
    let asset = dir.join("atelier-x86_64-unknown-linux-fullrust.gz");
    let gz = r
        .command("gzip")
        .arg("-c")
        .arg(r.path("fake-atelier"))
        .output()
        .unwrap();
    fs::write(&asset, &gz.stdout).unwrap();
    let sum = r.command("sha256sum").arg(&asset).output().unwrap();
    let sum = String::from_utf8_lossy(&sum.stdout);
    let sum = sum.split_whitespace().next().unwrap();
    fs::write(dir.join("SHA256SUMS"), format!("{sum}  {listed_as}\n")).unwrap();
    if tamper {
        // The asset changes after its checksum was published.
        let mut bytes = gz.stdout.clone();
        bytes.extend_from_slice(b"tampered");
        fs::write(&asset, bytes).unwrap();
    }
    Some(format!("file://{}", r.path("releases").display()))
}

fn installed(r: &Runner) -> PathBuf {
    r.path("tools/atelier/v9.9.9/x86_64-unknown-linux-fullrust/atelier")
}

#[test]
fn install_verifies_the_checksum_then_unpacks_and_caches() {
    let Some(mut r) = Runner::new("install") else {
        return;
    };
    let Some(url) = release(&r, "atelier-x86_64-unknown-linux-fullrust.gz", false) else {
        return;
    };
    r.set("ATELIER_ACTION_RELEASES_URL", &url)
        .set("INPUT_VERSION", "v9.9.9");
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 0, "{}", run.all());
    let bin = installed(&r);
    assert_eq!(r.outputs()["bin"], bin.to_str().unwrap());
    assert_eq!(
        fs::metadata(&bin).unwrap().permissions().mode() & 0o111,
        0o111
    );
    let out = r.command(bin.to_str().unwrap()).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "atelier 9.9.9\n");
    assert!(
        r.read("github-path")
            .unwrap()
            .contains("tools/atelier/v9.9.9")
    );

    // A second run finds it and downloads nothing: the release can be gone.
    fs::remove_dir_all(r.path("releases")).unwrap();
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stderr.contains("using the cached binary"));
}

#[test]
fn install_fails_closed_on_a_checksum_mismatch() {
    let Some(mut r) = Runner::new("install-mismatch") else {
        return;
    };
    let Some(url) = release(&r, "atelier-x86_64-unknown-linux-fullrust.gz", true) else {
        return;
    };
    r.set("ATELIER_ACTION_RELEASES_URL", &url)
        .set("INPUT_VERSION", "v9.9.9");
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stdout.contains("checksum mismatch"), "{}", run.all());
    assert!(
        !installed(&r).exists(),
        "an unverified binary was installed"
    );
    assert!(!r.outputs().contains_key("bin"));
}

#[test]
fn install_fails_closed_when_the_checksum_list_does_not_name_the_asset() {
    let Some(mut r) = Runner::new("install-unlisted") else {
        return;
    };
    let Some(url) = release(&r, "atelier-aarch64-unknown-linux-musl.gz", false) else {
        return;
    };
    r.set("ATELIER_ACTION_RELEASES_URL", &url)
        .set("INPUT_VERSION", "v9.9.9");
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stdout.contains("has no checksum for"), "{}", run.all());
    assert!(!installed(&r).exists());

    // And with no list at all.
    fs::remove_file(r.path("releases/download/v9.9.9/SHA256SUMS")).unwrap();
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stdout
            .contains("refusing to install an unverified binary")
    );
    assert!(!installed(&r).exists());
}

#[test]
fn install_says_so_on_a_platform_without_a_binary() {
    let Some(mut r) = Runner::new("install-platform") else {
        return;
    };
    r.set("INPUT_VERSION", "v9.9.9");
    r.stub(
        "uname",
        "#!/bin/sh\ncase $1 in -s) echo Darwin ;; -m) echo arm64 ;; esac\n",
    );
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stdout.contains("Linux binaries only"), "{}", run.all());

    r.stub(
        "uname",
        "#!/bin/sh\ncase $1 in -s) echo Linux ;; -m) echo riscv64 ;; esac\n",
    );
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stdout.contains("x86_64 and arm64 only"),
        "{}",
        run.all()
    );

    // A version that is not a tag never becomes part of a URL or a path.
    r.stub(
        "uname",
        "#!/bin/sh\ncase $1 in -s) echo Linux ;; -m) echo x86_64 ;; esac\n",
    );
    r.set("INPUT_VERSION", "../../etc");
    let run = r.script("install.sh", &[]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stdout.contains("not a release tag"), "{}", run.all());
}
