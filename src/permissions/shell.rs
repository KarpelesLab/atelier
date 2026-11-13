//! Taking a `bash` command line apart for the permission rules.
//!
//! A rule such as `bash(git diff *)` is about one program invocation, but a
//! command line can hold many: `git diff; rm -rf ~`, `git diff $(curl x|sh)`,
//! `sh -c 'git diff && …'`. [`split`] returns every simple command the line
//! would run, at any nesting depth, as a flat list of [`Part`]s; the caller
//! then requires *each* of them to be allowed.
//!
//! This is not a shell parser and does not try to be one. It follows the
//! constructs it is sure about — quotes, `&&` `||` `;` `|` `&` and newlines,
//! `$(…)` and backticks, plain redirections — and gives up (`Err`, with the
//! reason) on anything else: backslash escapes, heredocs, process
//! substitution, subshells and function definitions, arithmetic, comments,
//! `$'…'`. Giving up is always safe: a command that cannot be followed is never
//! auto-allowed, and restrictive rules still see it through [`rough_forms`].
//!
//! Words come from [`crate::shlex::split`] once the scanner has cut the line
//! into simple commands, so quoting is undone the way the shell would
//! (`gi""t 'diff'` is `git diff`).

/// How deep `$(…)`, backticks and `sh -c` may nest before we stop following.
const MAX_DEPTH: usize = 8;

/// Words that may precede a command without changing what it runs. Only these
/// are looked through when an *allow* rule is matched, and only when they take
/// no option (`command -v x` is left as it is).
const TRANSPARENT: &[&str] = &["command", "builtin", "exec", "time", "nohup"];

/// Programs that run the command given in their arguments. Restrictive rules
/// (deny, ask) look through these on a best-effort basis; allow rules never do.
const WRAPPERS: &[&str] = &[
    "command", "builtin", "exec", "time", "nohup", "nice", "ionice", "timeout", "stdbuf", "setsid",
    "env", "sudo", "doas", "xargs", "watch", "chronic",
];

/// Shells whose `-c <script>` is followed into.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];

/// Reserved words that are followed by a command on the same "line"
/// (`then rm x`, `! rm x`): stripped so rules see the command.
const LEADING_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "while", "until", "do", "!", "{",
];

/// Reserved words that open or close a construct we do not interpret.
const COMPOUND_KEYWORDS: &[&str] = &[
    "for", "case", "select", "function", "coproc", "in", "[[", "fi", "done", "esac", "}",
];

/// One simple command found in a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Its words, unquoted, without redirections or leading shell keywords.
    pub words: Vec<String>,
    /// Why an allow rule must not apply to it, if so.
    pub blocker: Option<&'static str>,
    /// A plain `sh -c '<script>'`: its script's commands are parts of their
    /// own, so this one needs no allow rule itself.
    pub wrapper: bool,
}

/// Every simple command `command` would run, or why it cannot be followed.
pub fn split(command: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    split_into(command, 0, &mut parts)?;
    Ok(parts)
}

fn split_into(command: &str, depth: usize, parts: &mut Vec<Part>) -> Result<(), String> {
    let src: Vec<char> = command.chars().collect();
    let mut scanner = Scanner {
        src: &src,
        pos: 0,
        out: Vec::new(),
    };
    scanner.list(Until::End, depth)?;
    for simple in scanner.out {
        let words = crate::shlex::split(&simple.text)
            .map_err(|_| "quoting that cannot be followed".to_string())?;
        let stripped = words
            .iter()
            .take_while(|w| LEADING_KEYWORDS.contains(&w.as_str()))
            .count();
        let words = words[stripped..].to_vec();
        let mut blocker = None;
        if simple.writes {
            blocker = Some("redirects output to a file");
        }
        let compound = words
            .first()
            .is_none_or(|w| COMPOUND_KEYWORDS.contains(&w.as_str()));
        if stripped > 0 || (compound && !words.is_empty()) {
            blocker = blocker.or(Some("uses shell control flow"));
        }
        if words.is_empty() {
            // Nothing runs (`fi`-less leftovers, a trailing `;`) — unless it
            // was a bare redirection, which still truncates its target.
            if blocker.is_some() {
                parts.push(Part {
                    words,
                    blocker,
                    wrapper: false,
                });
            }
            continue;
        }
        let run = allow_form(&words);
        // A name the shell expands (variables, substitutions, braces, globs)
        // is not the name that runs.
        if run[0].contains(['$', '`', '{', '*', '?', '[']) {
            blocker = blocker.or(Some("runs a command whose name is computed"));
        }
        let mut wrapper = false;
        if let Some((script, plain)) = shell_script(&words) {
            split_into(script, depth + 1, parts)?;
            wrapper = plain && blocker.is_none();
        }
        parts.push(Part {
            words,
            blocker,
            wrapper,
        });
    }
    Ok(())
}

/// The script of a `sh -c <script>` found in `words`, and whether the
/// invocation is plain enough (`sh -c`, `bash -lc`, nothing before the shell
/// but transparent wrappers, a system shell) to be nothing more than its
/// script. Looked for anywhere in the words, so `sudo bash -c '…'` is still
/// followed for the benefit of deny rules.
fn shell_script(words: &[String]) -> Option<(&str, bool)> {
    let at = words.iter().position(|w| SHELLS.contains(&basename(w)))?;
    let flag = words[at + 1..]
        .iter()
        .position(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('c'))?
        + at
        + 1;
    let script = words.get(flag + 1)?;
    let run = allow_form(words);
    let shell = &words[at];
    let system_shell = SHELLS.contains(&shell.as_str())
        || ["/bin/", "/usr/bin/"]
            .iter()
            .any(|dir| shell.strip_prefix(dir).is_some_and(|s| SHELLS.contains(&s)));
    let plain = std::ptr::eq(&run[0], shell)
        && system_shell
        && flag == at + 1
        && words[flag][1..].chars().all(|c| "cleux".contains(c));
    Some((script, plain))
}

/// `words` without the transparent wrappers in front (`time cargo test` →
/// `cargo test`): what an allow rule is matched against.
pub fn allow_form(words: &[String]) -> &[String] {
    let mut w = words;
    while w.len() > 1 && TRANSPARENT.contains(&w[0].as_str()) && !w[1].starts_with('-') {
        w = &w[1..];
    }
    w
}

/// Every reading of `words` a restrictive rule (deny, ask) is matched
/// against: as written, without leading `VAR=value` assignments, with the
/// program's directory dropped (`/bin/rm` → `rm`), and — behind a wrapper such
/// as `sudo`, `env`, `xargs` or `timeout` — every suffix of the words, since
/// where the wrapper's own options end is not ours to know. This over-matches
/// on purpose (`sudo echo rm x` is caught by `bash(rm *)`).
pub fn restrict_forms(words: &[String]) -> Vec<String> {
    fn push(out: &mut Vec<String>, words: &[String]) {
        let Some(first) = words.first() else { return };
        out.push(render(words));
        let base = basename(first);
        if base != first {
            let mut renamed = words.to_vec();
            renamed[0] = base.to_string();
            out.push(render(&renamed));
        }
    }
    let mut out = Vec::new();
    push(&mut out, words);
    let assigned = words.iter().take_while(|w| is_assignment(w)).count();
    if assigned > 0 {
        push(&mut out, &words[assigned..]);
    }
    if words
        .get(assigned)
        .is_some_and(|w| WRAPPERS.contains(&basename(w)))
    {
        for k in assigned + 1..words.len() {
            push(&mut out, &words[k..]);
        }
    }
    out
}

/// [`restrict_forms`] for a command [`split`] gave up on: cut at anything that
/// could be an operator, drop quote characters, and read each piece as a
/// command. Crude, and only ever used to *restrict*.
pub fn rough_forms(command: &str) -> Vec<String> {
    command
        .split([';', '|', '&', '\n', '(', ')', '{', '}', '`', '<', '>'])
        .flat_map(|piece| {
            let words: Vec<String> = piece
                .split_whitespace()
                .map(|w| w.replace(['\'', '"', '\\', '$'], ""))
                .filter(|w| !w.is_empty())
                .skip_while(|w| LEADING_KEYWORDS.contains(&w.as_str()))
                .collect();
            restrict_forms(&words)
        })
        .collect()
}

/// The words as one line: joined by single spaces, a word that is empty or
/// holds whitespace kept in single quotes so it cannot pass for several.
pub fn render(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            if w.is_empty() || w.contains(char::is_whitespace) {
                format!("'{w}'")
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `word` is a `NAME=value` (or `NAME+=value`) assignment.
pub fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The last path component of a program name.
pub fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// A simple command as the scanner found it.
#[derive(Default)]
struct Simple {
    /// Its source text, redirections removed.
    text: String,
    /// It redirects output to something other than `/dev/null` or a
    /// descriptor.
    writes: bool,
}

/// What ends the list being scanned.
#[derive(Clone, Copy, PartialEq)]
enum Until {
    /// The end of the text.
    End,
    /// The `)` closing a `$(`.
    Paren,
}

struct Scanner<'a> {
    src: &'a [char],
    pos: usize,
    /// Simple commands found so far, at every nesting depth.
    out: Vec<Simple>,
}

impl Scanner<'_> {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.src.get(self.pos + ahead).copied()
    }

    fn flush(&mut self, cur: &mut Simple) {
        let done = std::mem::take(cur);
        if !done.text.trim().is_empty() || done.writes {
            self.out.push(done);
        }
    }

    /// Scan a list of commands up to `until`, pushing each simple command.
    fn list(&mut self, until: Until, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("commands nested too deeply".into());
        }
        let mut cur = Simple::default();
        loop {
            let Some(c) = self.peek(0) else {
                if until == Until::Paren {
                    return Err("an unclosed `$(`".into());
                }
                self.flush(&mut cur);
                return Ok(());
            };
            match c {
                '\'' => self.single(&mut cur.text)?,
                '"' => self.double(&mut cur.text, depth)?,
                '`' => self.backtick(&mut cur.text, depth)?,
                '$' => self.dollar(&mut cur.text, depth, false)?,
                '\\' => return Err("a backslash escape".into()),
                '#' if cur.text.is_empty() || cur.text.ends_with(char::is_whitespace) => {
                    return Err("a comment".into());
                }
                '(' => return Err("a subshell, function or grouping".into()),
                ')' if until == Until::Paren => {
                    self.pos += 1;
                    self.flush(&mut cur);
                    return Ok(());
                }
                ')' => return Err("an unbalanced `)`".into()),
                '\n' | ';' => {
                    self.pos += 1;
                    self.flush(&mut cur);
                }
                '|' => {
                    self.pos += 1;
                    if matches!(self.peek(0), Some('|' | '&')) {
                        self.pos += 1;
                    }
                    self.flush(&mut cur);
                }
                '&' if self.peek(1) == Some('>') => self.redirect(&mut cur)?,
                '&' => {
                    self.pos += 1;
                    if self.peek(0) == Some('&') {
                        self.pos += 1;
                    }
                    self.flush(&mut cur);
                }
                '<' | '>' => self.redirect(&mut cur)?,
                c if c.is_control() && c != '\t' => {
                    return Err("a control character".into());
                }
                c => {
                    cur.text.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// A `'…'` string, copied as is.
    fn single(&mut self, text: &mut String) -> Result<(), String> {
        text.push('\'');
        self.pos += 1;
        loop {
            let c = self.peek(0).ok_or("an unterminated quote")?;
            text.push(c);
            self.pos += 1;
            if c == '\'' {
                return Ok(());
            }
        }
    }

    /// A `"…"` string: copied, with the command substitutions inside it
    /// followed.
    fn double(&mut self, text: &mut String, depth: usize) -> Result<(), String> {
        text.push('"');
        self.pos += 1;
        loop {
            match self.peek(0).ok_or("an unterminated quote")? {
                '"' => {
                    text.push('"');
                    self.pos += 1;
                    return Ok(());
                }
                '\\' => {
                    let escaped = self.peek(1).ok_or("an unterminated quote")?;
                    text.push('\\');
                    text.push(escaped);
                    self.pos += 2;
                }
                '`' => self.backtick(text, depth)?,
                '$' => self.dollar(text, depth, true)?,
                c => {
                    text.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// A `$`: a command substitution (followed), a `${…}` (accepted when it
    /// plainly holds no command), or a plain variable. `quoted`: inside
    /// `"…"`, where a `$` before a quote is just a `$` (`grep "x$"`).
    fn dollar(&mut self, text: &mut String, depth: usize, quoted: bool) -> Result<(), String> {
        let start = self.pos;
        match self.peek(1) {
            Some('(') if self.peek(2) == Some('(') => {
                return Err("an arithmetic expansion".into());
            }
            Some('(') => {
                self.pos += 2;
                self.list(Until::Paren, depth + 1)?;
            }
            Some('{') => {
                self.pos += 2;
                loop {
                    let c = self.peek(0).ok_or("an unclosed `${`")?;
                    self.pos += 1;
                    if c == '}' {
                        break;
                    }
                    if "$`'\"\\{(\n".contains(c) {
                        return Err("a nested parameter expansion".into());
                    }
                }
            }
            Some('\'' | '"') if !quoted => return Err("`$'…'`-style quoting".into()),
            Some('[') => return Err("an arithmetic expansion".into()),
            _ => self.pos += 1,
        }
        text.extend(&self.src[start..self.pos]);
        Ok(())
    }

    /// A `` `…` `` command substitution, followed.
    fn backtick(&mut self, text: &mut String, depth: usize) -> Result<(), String> {
        let start = self.pos;
        let len = self.src[start + 1..]
            .iter()
            .position(|&c| c == '`')
            .ok_or("an unterminated backtick")?;
        let inner = &self.src[start + 1..start + 1 + len];
        if inner.contains(&'\\') {
            return Err("a backslash escape".into());
        }
        let mut nested = Scanner {
            src: inner,
            pos: 0,
            out: Vec::new(),
        };
        nested.list(Until::End, depth + 1)?;
        self.out.append(&mut nested.out);
        self.pos = start + len + 2;
        text.extend(&self.src[start..self.pos]);
        Ok(())
    }

    /// A redirection, at its `<`, `>` or `&>`. It is dropped from the
    /// command's text; writing anywhere but `/dev/null` or a descriptor is
    /// noted.
    fn redirect(&mut self, cur: &mut Simple) -> Result<(), String> {
        // A descriptor number glued to the operator (`2>`) belongs to it.
        let digits = cur
            .text
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .count();
        let before = cur.text.len() - digits;
        if digits > 0 && (before == 0 || cur.text[..before].ends_with(char::is_whitespace)) {
            cur.text.truncate(before);
        }
        // The removed redirection still separates words (`a>x b`).
        cur.text.push(' ');

        if self.peek(0) == Some('<') {
            match self.peek(1) {
                Some('<') => return Err("a heredoc".into()),
                Some('(') => return Err("a process substitution".into()),
                Some('>') => {
                    self.pos += 2;
                    cur.writes = true;
                }
                Some('&') => self.pos += 2,
                _ => self.pos += 1,
            }
            self.target()?;
            return Ok(());
        }
        let mut dup = false;
        if self.peek(0) == Some('&') {
            // `&>` / `&>>`
            self.pos += 2;
            if self.peek(0) == Some('>') {
                self.pos += 1;
            }
        } else {
            self.pos += 1;
            match self.peek(0) {
                Some('(') => return Err("a process substitution".into()),
                Some('>' | '|') => self.pos += 1,
                _ => {}
            }
            if self.peek(0) == Some('&') {
                self.pos += 1;
                dup = true;
            }
        }
        let target = self.target()?;
        let descriptor = dup && (target == "-" || target.chars().all(|c| c.is_ascii_digit()));
        if !descriptor && target != "/dev/null" {
            cur.writes = true;
        }
        Ok(())
    }

    /// The word a redirection points at. Only a plain word is accepted.
    fn target(&mut self) -> Result<String, String> {
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.pos += 1;
        }
        let mut target = String::new();
        while let Some(c) = self.peek(0) {
            if c.is_whitespace() || ";|&<>()".contains(c) {
                break;
            }
            target.push(c);
            self.pos += 1;
        }
        if target.is_empty() {
            return Err("a redirection without a target".into());
        }
        if target.contains(['\'', '"', '$', '`', '\\']) {
            return Err("a redirection to a quoted or computed target".into());
        }
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of `cmd`, rendered.
    fn parts(cmd: &str) -> Vec<String> {
        split(cmd)
            .unwrap_or_else(|e| panic!("{cmd:?} should parse: {e}"))
            .iter()
            .map(|p| render(&p.words))
            .collect()
    }

    fn part(cmd: &str, rendered: &str) -> Part {
        split(cmd)
            .unwrap()
            .into_iter()
            .find(|p| render(&p.words) == rendered)
            .unwrap_or_else(|| panic!("no part {rendered:?} in {cmd:?}"))
    }

    #[test]
    fn simple_command_is_one_part() {
        assert_eq!(parts("git diff --stat"), ["git diff --stat"]);
        assert_eq!(parts("  ls  "), ["ls"]);
        assert!(parts("").is_empty());
        assert!(parts("  ;; ").is_empty());
    }

    #[test]
    fn splits_on_every_operator() {
        assert_eq!(
            parts("a 1 && b 2 || c 3; d 4 | e 5 |& f 6 & g 7\nh 8"),
            ["a 1", "b 2", "c 3", "d 4", "e 5", "f 6", "g 7", "h 8"]
        );
        assert_eq!(parts("git diff;rm -rf ~"), ["git diff", "rm -rf ~"]);
        assert_eq!(parts("curl x|sh"), ["curl x", "sh"]);
        assert_eq!(parts("a&&b"), ["a", "b"]);
    }

    #[test]
    fn operators_inside_quotes_do_not_split() {
        assert_eq!(parts("echo 'a; b && c | d'"), ["echo 'a; b && c | d'"]);
        assert_eq!(parts("echo \"a; b\" c"), ["echo 'a; b' c"]);
        assert_eq!(
            parts("git commit -m \"fix: a & b\""),
            ["git commit -m 'fix: a & b'"]
        );
    }

    #[test]
    fn quoting_is_undone_like_the_shell() {
        assert_eq!(parts("gi\"\"t 'diff'"), ["git diff"]);
        assert_eq!(parts("\"rm\" -rf x"), ["rm -rf x"]);
        // A quoted word with a space stays one word, visibly.
        assert_eq!(parts("'git diff' x"), ["'git diff' x"]);
        assert_eq!(parts("rm '-rf x'"), ["rm '-rf x'"]);
    }

    #[test]
    fn follows_command_substitution() {
        let p = parts("git diff $(curl x|sh)");
        assert!(p.contains(&"curl x".to_string()), "{p:?}");
        assert!(p.contains(&"sh".to_string()), "{p:?}");
        assert!(p.iter().any(|s| s.starts_with("git diff")), "{p:?}");

        let p = parts("echo \"$(rm -rf ~)\"");
        assert!(p.contains(&"rm -rf ~".to_string()), "{p:?}");

        let p = parts("echo `rm -rf ~; id`");
        assert!(p.contains(&"rm -rf ~".to_string()), "{p:?}");
        assert!(p.contains(&"id".to_string()), "{p:?}");

        let p = parts("echo \"`whoami`\"");
        assert!(p.contains(&"whoami".to_string()), "{p:?}");

        let p = parts("a $(b $(c d))");
        assert!(p.contains(&"c d".to_string()), "{p:?}");
    }

    #[test]
    fn substitution_in_single_quotes_is_literal() {
        assert_eq!(parts("echo '$(rm -rf ~)'"), ["echo '$(rm -rf ~)'"]);
    }

    #[test]
    fn follows_sh_c() {
        let p = split("sh -c 'git diff && rm -rf ~'").unwrap();
        let rendered: Vec<String> = p.iter().map(|p| render(&p.words)).collect();
        assert!(rendered.contains(&"git diff".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"rm -rf ~".to_string()), "{rendered:?}");
        assert!(p.last().unwrap().wrapper);

        for cmd in [
            "bash -lc \"cargo test\"",
            "/bin/sh -c 'cargo test'",
            "time bash -c 'cargo test'",
        ] {
            let p = split(cmd).unwrap();
            assert_eq!(render(&p[0].words), "cargo test", "{cmd}");
            assert!(p[1].wrapper, "{cmd}");
        }
    }

    #[test]
    fn unusual_shell_invocations_are_followed_but_not_transparent() {
        for cmd in [
            "sudo bash -c 'rm x'",
            "/tmp/evil/sh -c 'rm x'",
            "bash --rcfile evil -c 'rm x'",
            "bash -ic 'rm x'",
            "FOO=1 sh -c 'rm x'",
            "sh -c 'rm x' > out",
        ] {
            let p = split(cmd).unwrap();
            assert!(p.iter().any(|p| render(&p.words) == "rm x"), "{cmd}");
            assert!(p.iter().all(|p| !p.wrapper), "{cmd}");
        }
    }

    #[test]
    fn sh_c_script_that_cannot_be_followed_fails_the_whole() {
        assert!(split("sh -c 'cat <<EOF'").is_err());
        assert!(split("bash -c \"echo \\$(rm x)\"").is_err());
    }

    #[test]
    fn harmless_redirections_are_dropped() {
        assert_eq!(
            parts("cargo test 2>&1 | tail -5"),
            ["cargo test", "tail -5"]
        );
        assert_eq!(parts("git status 2>/dev/null"), ["git status"]);
        assert_eq!(parts("git status >/dev/null 2>&1"), ["git status"]);
        assert_eq!(parts("make &>/dev/null"), ["make"]);
        assert_eq!(parts("wc -l < input.txt"), ["wc -l"]);
        assert_eq!(parts("cmd >&2"), ["cmd"]);
        assert_eq!(parts("cmd 2>&-"), ["cmd"]);
        for cmd in ["cargo test 2>&1", "git status 2> /dev/null", "wc -l <in"] {
            assert!(split(cmd).unwrap()[0].blocker.is_none(), "{cmd}");
        }
    }

    #[test]
    fn a_digit_argument_is_not_a_descriptor() {
        assert_eq!(parts("echo 2 >/dev/null"), ["echo 2"]);
        assert_eq!(parts("echo a2>/dev/null"), ["echo a2"]);
    }

    #[test]
    fn writing_redirections_block_allow() {
        for cmd in [
            "echo hi > out.txt",
            "echo hi >> ~/.bashrc",
            "echo hi >| out",
            "echo hi &> out",
            "echo hi &>> out",
            "echo hi 2> err",
            "echo hi >& out",
            "echo hi <> out",
            "echo hi>out",
            "> out",
        ] {
            let p = split(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"));
            assert_eq!(p.len(), 1, "{cmd}");
            assert_eq!(p[0].blocker, Some("redirects output to a file"), "{cmd}");
        }
        assert_eq!(parts("echo hi>out there"), ["echo hi there"]);
    }

    #[test]
    fn gives_up_on_what_it_cannot_follow() {
        for cmd in [
            "echo 'unterminated",
            "echo \"unterminated",
            "echo `unterminated",
            "echo $(unterminated",
            "echo a\\ b",
            "git diff\\\n; rm x",
            "cat <<EOF\nrm x\nEOF",
            "cat <<< text",
            "diff <(ls a) <(ls b)",
            "tee >(cat)",
            "(cd x && make)",
            "f() { rm x; }; f",
            "echo $((1+2))",
            "echo $'a\\nb'",
            "echo $\"x\"",
            "echo ${x:-$(rm y)}",
            "echo ${x",
            "git diff # then; rm x",
            "# comment\nls",
            "echo hi > \"my file\"",
            "echo hi > $OUT",
            "echo hi >",
            "echo )",
            "case x in a) rm y;; esac",
            "echo `echo \\`rm x\\``",
            "echo a\rb",
        ] {
            assert!(split(cmd).is_err(), "{cmd:?} should not parse");
        }
    }

    #[test]
    fn dollar_before_a_closing_quote_is_literal() {
        assert_eq!(parts("grep \"foo$\" f"), ["grep foo$ f"]);
        assert!(split("echo $[1+2]").is_err());
    }

    #[test]
    fn plain_variables_are_kept() {
        assert_eq!(
            parts("echo $HOME ${USER} \"$x\" a#b $#"),
            ["echo $HOME ${USER} $x a#b $#"]
        );
    }

    #[test]
    fn computed_command_name_blocks_allow() {
        for cmd in [
            "$CMD arg",
            "${CMD} arg",
            "$(which git) diff",
            "`which git` diff",
            "{rm,-rf,x}",
            "/bin/r? x",
            "g[i]t diff",
            "* x",
        ] {
            let p = split(cmd).unwrap();
            assert_eq!(
                p.last().unwrap().blocker,
                Some("runs a command whose name is computed"),
                "{cmd}"
            );
        }
        // …also behind a transparent wrapper.
        assert!(split("time $CMD").unwrap()[0].blocker.is_some());
    }

    #[test]
    fn control_flow_blocks_allow_but_shows_the_command() {
        let p = split("for f in *; do rm $f; done").unwrap();
        let rendered: Vec<String> = p.iter().map(|p| render(&p.words)).collect();
        assert_eq!(rendered, ["for f in *", "rm $f", "done"]);
        assert!(p.iter().all(|p| p.blocker.is_some()));

        let p = split("if git diff --quiet; then rm x; fi").unwrap();
        let rendered: Vec<String> = p.iter().map(|p| render(&p.words)).collect();
        assert_eq!(rendered, ["git diff --quiet", "rm x", "fi"]);
        assert!(p.iter().all(|p| p.blocker.is_some()));

        assert_eq!(
            part("! rm x", "rm x").blocker,
            Some("uses shell control flow")
        );
        assert!(
            split("{ rm x; }")
                .unwrap()
                .iter()
                .all(|p| p.blocker.is_some())
        );
    }

    #[test]
    fn allow_form_strips_only_transparent_wrappers() {
        let w = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(allow_form(&w("time cargo test")), &w("cargo test")[..]);
        assert_eq!(
            allow_form(&w("nohup exec command git diff")),
            &w("git diff")[..]
        );
        // An option changes what the wrapper does: left alone.
        assert_eq!(allow_form(&w("command -v git")), &w("command -v git")[..]);
        assert_eq!(allow_form(&w("exec -a x git")), &w("exec -a x git")[..]);
        // Never looked through for allow.
        for cmd in [
            "sudo git diff",
            "env git diff",
            "xargs git diff",
            "FOO=1 git diff",
        ] {
            assert_eq!(allow_form(&w(cmd)), &w(cmd)[..]);
        }
        assert_eq!(allow_form(&w("time")), &w("time")[..]);
    }

    #[test]
    fn restrict_forms_look_through_wrappers_and_assignments() {
        let forms = |s: &str| {
            let w: Vec<String> = s.split(' ').map(String::from).collect();
            restrict_forms(&w)
        };
        assert_eq!(forms("rm -rf x"), ["rm -rf x"]);
        assert!(forms("FOO=1 BAR=2 rm -rf x").contains(&"rm -rf x".to_string()));
        assert!(forms("/bin/rm -rf x").contains(&"rm -rf x".to_string()));
        for cmd in [
            "sudo rm -rf x",
            "sudo -u root rm -rf x",
            "env -i A=1 rm -rf x",
            "nice -n 10 rm -rf x",
            "timeout 5 rm -rf x",
            "nohup rm -rf x",
            "command rm -rf x",
            "exec rm -rf x",
            "time rm -rf x",
            "xargs -n 1 rm -rf x",
            "sudo env /bin/rm -rf x",
            "A=1 /usr/bin/sudo rm -rf x",
        ] {
            assert!(forms(cmd).contains(&"rm -rf x".to_string()), "{cmd}");
        }
        assert!(!forms("echo rm -rf x").contains(&"rm -rf x".to_string()));
    }

    #[test]
    fn rough_forms_see_through_broken_syntax() {
        for cmd in [
            "echo 'oops; rm -rf x",
            "(rm -rf x)",
            "r\\m -rf x",
            "cat <<EOF; rm -rf x",
            "f() { rm -rf x; }",
            "echo $((1)) && sudo rm -rf x",
            "if true; then rm -rf x; fi # c",
        ] {
            assert!(
                rough_forms(cmd).contains(&"rm -rf x".to_string()),
                "{cmd}: {:?}",
                rough_forms(cmd)
            );
        }
    }

    #[test]
    fn assignments() {
        assert!(is_assignment("FOO=1"));
        assert!(is_assignment("_x="));
        assert!(is_assignment("PATH+=:/x"));
        assert!(!is_assignment("--flag=1"));
        assert!(!is_assignment("1x=2"));
        assert!(!is_assignment("a/b=c"));
        assert!(!is_assignment("plain"));
    }

    #[test]
    fn nesting_is_bounded() {
        let mut cmd = "id".to_string();
        for _ in 0..20 {
            cmd = format!("echo $({cmd})");
        }
        assert!(split(&cmd).is_err());
    }
}
