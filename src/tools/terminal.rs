//! A foreground command on a terminal of its own, which the user can be
//! handed the keyboard of.
//!
//! A command run on pipes with no stdin cannot ask anything: `fastlane`
//! wanting a 2FA code, `ssh` a passphrase, `sudo` a password, `npm login`,
//! a "continue? [y/N]" — each fails or sits there until the timeout. And
//! nobody can say beforehand which command will ask. So where somebody is
//! at an interface that can pass keys on ([`Console`]), every foreground
//! command runs on a pseudo-terminal ([`super::pty`]), and can *become*
//! interactive at any point of its run:
//!
//! - the user takes the keyboard whenever they want ([`CommandInput::Attach`]
//!   — a key in the terminal interface, a method over `serve`);
//! - atelier notices a command that is probably waiting for input
//!   ([`wanted`]) and says so ([`CommandEvent::InputWanted`]), with time to
//!   answer ([`ANSWER_GRACE`]) whatever the command's timeout was;
//! - the model can say up front that the user will have to type
//!   (`interactive: true`), which asks for the keyboard as the command
//!   starts.
//!
//! # Why a terminal for every command
//!
//! A command cannot be moved onto a terminal once it runs, and the programs
//! this is for prompt on the terminal — often only when they see one, and
//! not on stdin at all (`/dev/tty`). So it is a terminal from the start or
//! no hand-over. What that costs an ordinary command is kept small:
//!
//! - `TERM=dumb`, `NO_COLOR=1`, `PAGER=cat` and `GIT_PAGER=cat`, so
//!   programs that see a terminal still print plainly and page nothing
//!   (with `interactive: true` the user's own `TERM` is kept: that command
//!   is theirs to drive);
//! - what the model receives is cleaned ([`clean`]): escape sequences
//!   removed, `\r\n` back to `\n`, a line redrawn with `\r` or backspaces
//!   reduced to what it ended as;
//! - the window is wide ([`COLS`]) until the user's own size is known, so
//!   less is cut to fit.
//!
//! What does change: stdout and stderr are one stream, in the order things
//! were printed, with no `[stderr]` section; and a command that reads its
//! stdin gets a terminal instead of an immediate end of file, so `cat` with
//! no argument waits (and is reported as waiting) rather than returning at
//! once.
//!
//! # What the user types is theirs
//!
//! A code or a password typed to a command goes to the command and nowhere
//! else: it is written to the terminal and never stored in anything that
//! outlives this call. The terminal echoes it, though, and a program may
//! print it back. So of what the command prints, the model is given
//! everything except:
//!
//! - the rest of the line the user typed on — from each keystroke to the
//!   next line break in the output, which is where the echo is;
//! - any later occurrence of a line the user typed, if it is at least
//!   [`MIN_SECRET`] characters long, replaced with `[typed]` — the echo that
//!   arrived out of step, the "verifying 123456…" a program adds.
//!
//! The interface is given the raw output as it comes
//! ([`CommandEvent::Output`]), echo included: that is the user's own
//! screen. The text returned here is the only thing that reaches the
//! conversation, and so the saved session, a trace, or a kept output file.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::pty;

/// How long a command has to have printed nothing before it is looked at
/// for whether it waits for input.
const QUIET: Duration = Duration::from_millis(1000);
/// How long a command that wants input — it was seen waiting, or the model
/// said it would — is given for the user to come and answer, whatever is
/// left of its timeout. While the user has the keyboard, no time counts.
pub const ANSWER_GRACE: Duration = Duration::from_secs(300);
/// How often the command is looked at: for exit, for input to pass on.
const POLL: Duration = Duration::from_millis(20);
/// How long after the command's exit its last output is waited for, when
/// something it left running still holds the terminal.
const DRAIN: Duration = Duration::from_millis(300);
/// The window until the interface says what it really is: wide, so that
/// programs that fit their output to it cut less.
const COLS: u16 = 200;
const ROWS: u16 = 50;
/// The shortest typed line that is searched for in the output and removed.
/// Shorter ones (`y`, `2`) are no secret, and removing them everywhere
/// would mangle the output; their echo is withheld all the same.
pub const MIN_SECRET: usize = 4;
/// What stands for a typed line in the text the model is given.
pub const TYPED: &str = "[typed]";
/// The most of a prompt that is passed on, in characters.
const PROMPT_CHARS: usize = 200;

/// What a running command tells the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandEvent<'a> {
    /// It started, on a terminal the user can be given the keyboard of.
    /// `interactive`: the model expects the user to have to type.
    Started { command: &'a str, interactive: bool },
    /// What it printed, raw, as a terminal is sent it. For the user's
    /// screen only: it may hold the echo of what they typed.
    Output(&'a [u8]),
    /// It seems to be waiting for input. `prompt` is its unfinished last
    /// line (`Enter the 6 digit code:`), or empty.
    InputWanted { prompt: &'a str },
    /// It is over: there is nothing left to type to.
    Ended,
}

/// What the user does to a running command, through the interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandInput {
    /// They took the keyboard: from here the command's time does not run.
    Attach,
    /// They gave it back.
    Detach,
    /// Keys, as a terminal sends them (Enter is `\r`, Ctrl-C is `0x03`).
    Bytes(Vec<u8>),
    /// The size of what they see the command in.
    Resize { cols: u16, rows: u16 },
}

/// The interface, as a running command sees it. [`crate::agent::Ui`] has
/// the same three under `command_` names; this is what the tool is written
/// against, so that it does not need the agent.
pub trait Console {
    fn event(&mut self, event: CommandEvent);
    /// What the user did since last asked, in order.
    fn input(&mut self) -> Vec<CommandInput>;
    /// Whether the user asked to stop the turn: the command is killed.
    fn interrupted(&mut self) -> bool {
        false
    }
}

/// How a command's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Exited,
    /// Killed: its time ran out.
    TimedOut,
    /// Killed: the user stopped the turn.
    Stopped,
}

/// What a command did on its terminal.
pub struct Finished {
    pub end: End,
    /// `None` if it was killed.
    pub status: Option<ExitStatus>,
    /// What it printed, as the model is to see it: cleaned, and without
    /// what the user typed (see the module's notes).
    pub output: String,
    /// The user typed to it.
    pub typed: bool,
    /// It was killed while it seemed to wait for input nobody gave: the
    /// prompt it was at.
    pub unanswered: Option<String>,
}

/// A terminal a command can be started on.
pub struct Terminal(pty::Pair);

/// Whether this build can run commands on a terminal at all.
pub fn supported() -> bool {
    pty::SUPPORTED
}

/// Open a terminal for one command. An error is a reason to run it on
/// pipes instead, and to say so.
pub fn open() -> std::io::Result<Terminal> {
    pty::open().map(Terminal)
}

impl Terminal {
    /// Run `command` (`sh -c`) in `root` on this terminal until it exits,
    /// `timeout` passes or the user stops the turn, passing what the user
    /// types through `console` on to it.
    pub fn run(
        self,
        command: &str,
        root: &Path,
        timeout: Duration,
        interactive: bool,
        console: &mut dyn Console,
    ) -> Result<Finished> {
        let path = self.0.path.clone();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(root);
        // Seen as a terminal, and still asked to print plainly.
        cmd.env("PAGER", "cat").env("GIT_PAGER", "cat");
        if !interactive || std::env::var_os("TERM").is_none() {
            cmd.env("TERM", "dumb").env("NO_COLOR", "1");
        }
        let master = self.0.attach(&mut cmd).context("preparing the terminal")?;
        pty::resize(&master, COLS, ROWS);
        let child = cmd.spawn().context("spawning shell command")?;
        // The command's side of the terminal is the command's alone now:
        // the master reads its end when the last process on it is gone.
        drop(cmd);

        let reader = master.try_clone().context("reading the terminal")?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            // Ends with the terminal: an error on Linux, 0 on macOS.
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        console.event(CommandEvent::Started {
            command,
            interactive,
        });
        let mut run = Run {
            child,
            master,
            path,
            output: rx,
            record: Record::default(),
            last_output: Instant::now(),
            deadline: Instant::now() + timeout,
            attached: None,
            waiting: None,
            looked: false,
        };
        if interactive {
            run.allow(ANSWER_GRACE);
        }
        let (end, status) = run.until_done(console);
        console.event(CommandEvent::Ended);
        Ok(Finished {
            end,
            status,
            typed: run.record.typed_anything,
            unanswered: run.waiting.filter(|_| end == End::TimedOut),
            output: run.record.finish(),
        })
    }
}

/// A command while it runs.
struct Run {
    child: Child,
    master: File,
    /// The terminal's path, to recognize a read of it.
    path: String,
    output: Receiver<Vec<u8>>,
    record: Record,
    last_output: Instant,
    /// When the command is killed, unless the user has the keyboard.
    deadline: Instant,
    /// Since when the user has had the keyboard.
    attached: Option<Instant>,
    /// The prompt the command was seen waiting at, and has not been
    /// answered since.
    waiting: Option<String>,
    /// This quiet stretch was already looked at ([`wanted`]).
    looked: bool,
}

impl Run {
    /// Give the command at least `time` from now.
    fn allow(&mut self, time: Duration) {
        self.deadline = self.deadline.max(Instant::now() + time);
    }

    fn until_done(&mut self, console: &mut dyn Console) -> (End, Option<ExitStatus>) {
        loop {
            // What it printed; the wait doubles as the loop's sleep.
            match self.output.recv_timeout(POLL) {
                Ok(chunk) => self.printed(&chunk, console),
                Err(RecvTimeoutError::Timeout) => {}
                // The terminal is gone with the last process on it: only
                // the exit status is left to collect.
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(POLL),
            }
            for input in console.input() {
                self.take(input, console);
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                self.drain(console);
                return (End::Exited, Some(status));
            }
            let end = if console.interrupted() {
                Some(End::Stopped)
            } else if self.attached.is_none() && Instant::now() >= self.deadline {
                Some(End::TimedOut)
            } else {
                None
            };
            if let Some(end) = end {
                // What it was waiting at, if anything, is the caller's to
                // report: the last output must not erase it.
                let waiting = self.waiting.take();
                self.kill();
                self.drain(console);
                self.waiting = waiting;
                return (end, None);
            }
            // Quiet for a while: is it waiting for somebody to type?
            if !self.looked && self.last_output.elapsed() >= QUIET {
                self.looked = true;
                let prompt = self.record.prompt();
                if wanted(pty::reading(self.child.id(), &self.path), &prompt) {
                    console.event(CommandEvent::InputWanted { prompt: &prompt });
                    self.allow(ANSWER_GRACE);
                    self.waiting = Some(prompt);
                }
            }
        }
    }

    fn printed(&mut self, chunk: &[u8], console: &mut dyn Console) {
        console.event(CommandEvent::Output(chunk));
        self.record.printed(chunk);
        self.last_output = Instant::now();
        self.looked = false;
        self.waiting = None;
    }

    fn take(&mut self, input: CommandInput, console: &mut dyn Console) {
        match input {
            CommandInput::Attach => {
                self.attached.get_or_insert_with(Instant::now);
            }
            CommandInput::Detach => {
                // The time the user held the keyboard was not the command's.
                if let Some(since) = self.attached.take() {
                    self.deadline += since.elapsed();
                }
            }
            CommandInput::Resize { cols, rows } => pty::resize(&self.master, cols, rows),
            CommandInput::Bytes(bytes) => {
                // What was printed before the keys comes first, so that
                // their echo is not taken for it (nor it for the echo).
                while let Ok(chunk) = self.output.try_recv() {
                    self.printed(&chunk, console);
                }
                self.record.typed(&bytes);
                let _ = self.master.write_all(&bytes);
                self.waiting = None;
                self.looked = false;
                self.last_output = Instant::now();
            }
        }
    }

    /// The last of what the command printed, once it has exited.
    fn drain(&mut self, console: &mut dyn Console) {
        let until = Instant::now() + DRAIN;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.output.recv_timeout(left) {
                Ok(chunk) => self.printed(&chunk, console),
                Err(_) => break,
            }
        }
    }

    /// Kill the command and what it started: its session's processes are
    /// its process group unless it made others, which lose the terminal
    /// (and get `SIGHUP`) when the master is closed.
    fn kill(&mut self) {
        #[cfg(unixlike)]
        {
            let group = format!("-{}", self.child.id());
            let _ = Command::new("kill")
                .args(["-KILL", "--", &group])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a command that has been quiet is waiting for input. `reading`
/// is what could be seen of it ([`pty::reading`]): when that says yes or
/// no, it decides — a `read` with no prompt is caught, a progress line
/// with no line break is not mistaken for one. When it cannot be told, the
/// unfinished last line has to look like a question.
fn wanted(reading: Option<bool>, prompt: &str) -> bool {
    reading.unwrap_or_else(|| looks_like_prompt(prompt))
}

/// Whether `line` — the unfinished last line of some output — reads like a
/// prompt: it ends the way prompts do, or names what prompts ask for.
pub fn looks_like_prompt(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() || line.chars().count() > PROMPT_CHARS {
        return false;
    }
    let lower = line.to_lowercase();
    line.ends_with([':', '?', '>', ']'])
        || [
            "password",
            "passphrase",
            "passcode",
            "verification code",
            "digit code",
            "[y/n",
            "(y/n",
            "(yes/no",
            "press enter",
            "press any key",
        ]
        .iter()
        .any(|asks| lower.contains(asks))
}

/// The prompt a command was left at, when its output — which ended without
/// a line break — reads like one. For a command that ran with nobody to
/// answer it and did not end well.
pub fn prompt_left(output: &str) -> Option<String> {
    let last = output.rsplit('\n').next()?;
    let last = clean(last.as_bytes());
    looks_like_prompt(&last).then(|| clip(last.trim()))
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(PROMPT_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// What a command printed and what was typed to it, kept apart.
#[derive(Default)]
struct Record {
    /// What it printed, raw, less what is withheld.
    printed: Vec<u8>,
    /// The user typed on the line being printed: what follows, up to the
    /// line's end, is (or may be) the echo, and is withheld.
    echoing: bool,
    /// The line being typed, and the ones typed before it that are long
    /// enough to look for in the output.
    line: String,
    lines: Vec<String>,
    typed_anything: bool,
}

impl Record {
    fn printed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if !self.echoing {
                self.printed.push(byte);
            } else if byte == b'\n' {
                // The line the user typed on ends here; the break is kept,
                // so what follows starts a line as it did on the screen.
                self.echoing = false;
                self.printed.push(byte);
            }
        }
    }

    fn typed(&mut self, bytes: &[u8]) {
        // An interrupt (Ctrl-C, Ctrl-\, Ctrl-Z) is no text: its echo is
        // `^C`, and what the command says to it is for the model to see.
        if bytes.iter().any(|b| !matches!(b, 0x03 | 0x1c | 0x1a)) {
            self.echoing = true;
        }
        self.typed_anything = true;
        for c in String::from_utf8_lossy(bytes).chars() {
            match c {
                // Enter, and what else ends a line of input: Ctrl-C, Ctrl-D.
                '\r' | '\n' | '\x03' | '\x04' => self.end_line(),
                '\x7f' | '\x08' => drop(self.line.pop()),
                // Ctrl-U: the line is erased.
                '\x15' => self.line.clear(),
                c if c.is_control() => {}
                c => self.line.push(c),
            }
        }
    }

    fn end_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        if line.chars().count() >= MIN_SECRET && !self.lines.contains(&line) {
            self.lines.push(line);
        }
    }

    /// The unfinished last line of what was printed, as a prompt.
    fn prompt(&self) -> String {
        // Enough for any prompt, and cheap to clean each time.
        let recent = &self.printed[self.printed.len().saturating_sub(1024)..];
        let text = clean(recent);
        clip(text.rsplit('\n').next().unwrap_or("").trim())
    }

    /// The text for the model: cleaned, and without any line the user
    /// typed that the output repeats.
    fn finish(mut self) -> String {
        self.end_line();
        let mut text = clean(&self.printed);
        // Longest first: a typed line may contain another.
        self.lines
            .sort_by_key(|line| std::cmp::Reverse(line.chars().count()));
        for line in &self.lines {
            text = text.replace(line.as_str(), TYPED);
        }
        text
    }
}

/// What a terminal was sent, as the text it would show, more or less: for
/// the model, which is given lines and not a screen.
///
/// - Escape sequences (colours, cursor movement, titles) are dropped.
/// - `\r\n` is `\n`. A lone `\r` returns to the start of the line, and what
///   is printed next replaces it — a progress bar is left as its last
///   state, not as every state it went through.
/// - A backspace removes the character before it.
/// - Other control characters are dropped; tabs stay.
pub fn clean(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len());
    // Where the current line starts in `out`, and whether a `\r` was seen
    // and nothing printed since.
    let mut line_start = 0;
    let mut returned = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push('\n');
                line_start = out.len();
                returned = false;
            }
            '\r' => returned = true,
            '\x08' => {
                if out.len() > line_start {
                    out.pop();
                }
            }
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte in `@`..=`~`.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC (a title, a link): up to BEL, or ESC `\`.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                // A character set: one more character.
                Some('(' | ')' | '*' | '+' | '#' | '%') => {
                    chars.next();
                }
                // Anything else is ESC and one character.
                _ => {}
            },
            '\t' => {
                overwrite(&mut out, line_start, &mut returned);
                out.push('\t');
            }
            c if c.is_control() => {}
            c => {
                overwrite(&mut out, line_start, &mut returned);
                out.push(c);
            }
        }
    }
    out
}

/// Before a character is printed: after a `\r`, the line starts over.
fn overwrite(out: &mut String, line_start: usize, returned: &mut bool) {
    if std::mem::take(returned) {
        out.truncate(line_start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_output_is_cleaned_into_plain_lines() {
        assert_eq!(clean(b"one\r\ntwo\r\n"), "one\ntwo\n");
        // Colours, cursor movement, a title, a character set.
        assert_eq!(
            clean(b"\x1b[1;32mok\x1b[0m \x1b]0;title\x07done\x1b(B\x1b[K\r\n"),
            "ok done\n"
        );
        assert_eq!(clean(b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), "link");
        // A progress line is what it ended as.
        assert_eq!(clean(b"  0%\r 50%\r100%\r\ndone\r\n"), "100%\ndone\n");
        // A `\r` at the very end changes nothing by itself.
        assert_eq!(clean(b"Password: \r"), "Password: ");
        // Backspaces erase, but not past the line.
        assert_eq!(clean(b"abc\x08\x08xy\r\n\x08z"), "axy\nz");
        // Bells and other control characters go; tabs and UTF-8 stay.
        assert_eq!(clean("a\x07\tb\x00 é✓".as_bytes()), "a\tb é✓");
    }

    #[test]
    fn a_prompt_is_an_unfinished_line_that_asks() {
        for prompt in [
            "Password:",
            "Enter the 6 digit code: ",
            "Continue? [y/N] ",
            "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
            "[sudo] password for me: ",
            "Enter passphrase for key '/home/me/.ssh/id': ",
            "Press ENTER to continue...",
            "> ",
            "Please enter the 6 digit code you received at +1 (•••) •••-••12:",
        ] {
            assert!(looks_like_prompt(prompt), "{prompt}");
        }
        for not in [
            "",
            "   ",
            "Compiling atelier v0.1.2",
            "Downloading 45%",
            "done.",
        ] {
            assert!(!looks_like_prompt(not), "{not}");
        }
        assert!(!looks_like_prompt(&format!("{}:", "x".repeat(300))));

        // Seeing the command read the terminal, or not, decides when it can
        // be seen; the text only when it cannot.
        assert!(wanted(Some(true), ""));
        assert!(!wanted(Some(false), "Password:"));
        assert!(wanted(None, "Password:"));
        assert!(!wanted(None, "Downloading 45%"));

        // What a command with nobody to answer was left at.
        assert_eq!(
            prompt_left("Fetching…\nEnter the 6 digit code: ").as_deref(),
            Some("Enter the 6 digit code:")
        );
        assert_eq!(prompt_left("all good\n"), None);
        assert_eq!(prompt_left("Downloading 45%"), None);
    }

    #[test]
    fn what_is_typed_and_its_echo_stay_out_of_the_record() {
        let mut record = Record::default();
        record.printed(b"Enter code: ");
        assert_eq!(record.prompt(), "Enter code:");
        // Typed key by key, with a correction; echoed by the terminal.
        for key in [&b"12"[..], b"9", b"\x7f", b"3456"] {
            record.typed(key);
        }
        record.printed(b"129\x08 \x083456");
        record.typed(b"\r");
        record.printed(b"\r\nVerifying 123456...\r\nok\r\n");
        assert!(record.typed_anything);
        let text = record.finish();
        assert_eq!(text, "Enter code: \nVerifying [typed]...\nok\n");
        assert!(!text.contains("123456") && !text.contains("3456"));

        // No echo (a password): nothing is lost but the line's own end.
        let mut record = Record::default();
        record.printed(b"Password: ");
        record.typed(b"hunter2\r");
        record.printed(b"\r\nWelcome\r\n");
        assert_eq!(record.finish(), "Password: \nWelcome\n");

        // A short answer is not searched for, only its echo withheld.
        let mut record = Record::default();
        record.printed(b"Continue? [y/N] ");
        record.typed(b"y\r");
        record.printed(b"y\r\nyes, continuing\r\n");
        assert_eq!(record.finish(), "Continue? [y/N] \nyes, continuing\n");

        // A line never ended (the user left mid-word) is still theirs.
        let mut record = Record::default();
        record.typed(b"s3cret");
        record.printed(b"s3cret\r\nyou typed s3cret\r\n");
        assert_eq!(record.finish(), "\nyou typed [typed]\n");

        // An interrupt hides nothing: what the command answers is shown.
        let mut record = Record::default();
        record.printed(b"Waiting: ");
        record.typed(b"\x03");
        record.printed(b"^Cinterrupted\r\n");
        assert_eq!(record.finish(), "Waiting: ^Cinterrupted\n");
    }
}
