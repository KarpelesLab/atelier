//! The terminal, as much of it as the interface needs: raw mode, the
//! window size, key presses, and the handful of escape sequences the
//! renderer writes.
//!
//! Self-contained so the interface also builds where there is no libc
//! (fullrust). The only system call is `ioctl` — to read and set the
//! terminal modes and to read the window size — made through the kernel
//! directly on fullrust and through the C library std already links
//! elsewhere. Keys are read by a thread blocked on stdin, which hands the
//! bytes to [`poll`] over a channel, so waiting with a timeout needs no
//! `poll(2)`; and a resize is noticed by comparing the size between waits
//! rather than by catching `SIGWINCH`.
//!
//! Supported: Linux (fullrust included) and macOS — see `build.rs`, which
//! only enables the interface there. Elsewhere atelier uses the line REPL.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Escape sequences the renderer writes.
pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";
pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const REVERSE: &str = "\x1b[7m";
/// Clear from the cursor to the end of the screen.
pub const CLEAR_DOWN: &str = "\x1b[J";
pub const CLEAR_SCREEN: &str = "\x1b[2J";
pub const CLEAR_LINE: &str = "\x1b[2K";
pub const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
pub const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// Move the cursor up `n` rows (nothing for 0, which terminals read as 1).
pub fn move_up(n: u16) -> String {
    if n == 0 {
        String::new()
    } else {
        format!("\x1b[{n}A")
    }
}

/// Move the cursor to column `col` (0-based) of its row.
pub fn move_to_column(col: u16) -> String {
    format!("\x1b[{}G", u32::from(col) + 1)
}

/// Move the cursor to `col`, `row` (0-based).
pub fn move_to(col: u16, row: u16) -> String {
    format!("\x1b[{};{}H", u32::from(row) + 1, u32::from(col) + 1)
}

/// A key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Ctrl with a letter (lowercase), e.g. `Ctrl('c')`.
    Ctrl(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Esc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    /// The window changed size.
    Resize,
}

// --- The system side: ioctl, and the structures it fills. ---

const STDIN: i32 = 0;
const STDOUT: i32 = 1;

/// `struct termios` as `ioctl` takes it.
#[cfg(not(target_os = "macos"))]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Termios {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    line: u8,
    cc: [u8; 19],
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Termios {
    iflag: u64,
    oflag: u64,
    cflag: u64,
    lflag: u64,
    cc: [u8; 20],
    ispeed: u64,
    ospeed: u64,
}

/// `struct winsize`.
#[repr(C)]
#[derive(Default)]
struct WinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

/// Requests and mode bits: Linux's generic values (x86, ARM, RISC-V).
#[cfg(not(target_os = "macos"))]
mod os {
    pub const GET_MODES: u64 = 0x5401; // TCGETS
    pub const SET_MODES: u64 = 0x5402; // TCSETS
    pub const GET_SIZE: u64 = 0x5413; // TIOCGWINSZ
    // IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON
    pub const IFLAG_OFF: u32 = 0o1 | 0o2 | 0o10 | 0o40 | 0o100 | 0o200 | 0o400 | 0o2000;
    pub const OPOST: u32 = 0o1;
    // ECHO | ECHONL | ICANON | ISIG | IEXTEN
    pub const LFLAG_OFF: u32 = 0o10 | 0o100 | 0o2 | 0o1 | 0o100000;
    pub const CSIZE: u32 = 0o60;
    pub const PARENB: u32 = 0o400;
    pub const CS8: u32 = 0o60;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
}

#[cfg(target_os = "macos")]
mod os {
    pub const GET_MODES: u64 = 0x4048_7413; // TIOCGETA
    pub const SET_MODES: u64 = 0x8048_7414; // TIOCSETA
    pub const GET_SIZE: u64 = 0x4008_7468; // TIOCGWINSZ
    pub const IFLAG_OFF: u64 = 0x1 | 0x2 | 0x8 | 0x20 | 0x40 | 0x80 | 0x100 | 0x200;
    pub const OPOST: u64 = 0x1;
    pub const LFLAG_OFF: u64 = 0x8 | 0x10 | 0x100 | 0x80 | 0x400;
    pub const CSIZE: u64 = 0x300;
    pub const PARENB: u64 = 0x1000;
    pub const CS8: u64 = 0x300;
    pub const VMIN: usize = 16;
    pub const VTIME: usize = 17;
}

/// `ioctl(fd, request, arg)`, where `arg` is the structure `request` reads
/// or fills.
#[cfg(target_os = "fullrust")]
fn ioctl<T>(fd: i32, request: u64, arg: &mut T) -> io::Result<()> {
    use std::os::fullrust::syscall::{nr, syscall3};
    // SAFETY: `arg` is a live, exclusively borrowed `T`, the structure the
    // kernel expects for each request used in this module.
    unsafe {
        syscall3(
            nr::IOCTL,
            fd as usize,
            request as usize,
            std::ptr::from_mut(arg) as usize,
        )
    }
    .map(drop)
}

#[cfg(not(target_os = "fullrust"))]
fn ioctl<T>(fd: i32, request: u64, arg: &mut T) -> io::Result<()> {
    use std::ffi::{c_int, c_ulong};
    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }
    // SAFETY: as above; `ioctl` is the C library's, which std links.
    if unsafe { ioctl(fd, request as c_ulong, std::ptr::from_mut(arg)) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The window's size as `(columns, rows)`; `None` if stdout isn't a
/// terminal (or reports no size).
pub fn size() -> Option<(u16, u16)> {
    let mut ws = WinSize::default();
    ioctl(STDOUT, os::GET_SIZE, &mut ws).ok()?;
    (ws.cols > 0 && ws.rows > 0).then_some((ws.cols, ws.rows))
}

/// Raw mode: keys arrive as typed — unbuffered, unechoed, Ctrl-C included —
/// and output is written as is (so a newline is `\r\n`). Dropping it
/// restores the modes the terminal had.
pub struct RawMode {
    saved: Termios,
}

impl RawMode {
    pub fn enable() -> io::Result<Self> {
        let mut saved = Termios::default();
        ioctl(STDIN, os::GET_MODES, &mut saved)?;
        let mut raw = saved;
        raw.iflag &= !os::IFLAG_OFF;
        raw.oflag &= !os::OPOST;
        raw.lflag &= !os::LFLAG_OFF;
        raw.cflag = (raw.cflag & !(os::CSIZE | os::PARENB)) | os::CS8;
        // A read returns as soon as one byte is there.
        raw.cc[os::VMIN] = 1;
        raw.cc[os::VTIME] = 0;
        ioctl(STDIN, os::SET_MODES, &mut raw)?;
        Ok(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = ioctl(STDIN, os::SET_MODES, &mut self.saved);
    }
}

// --- Keys. ---

/// How long a lone ESC waits for the rest of an escape sequence before it
/// counts as the Esc key.
const ESC_WAIT: Duration = Duration::from_millis(25);
/// How often a blocking [`read`] looks at the window size.
const RESIZE_CHECK: Duration = Duration::from_millis(200);

/// What the front of the input buffer holds.
#[derive(Debug, PartialEq)]
enum Parsed {
    /// A key, and the bytes it took.
    Key(Key, usize),
    /// Bytes that mean nothing to us (an unknown sequence), to drop.
    Skip(usize),
    /// The start of something; more bytes are needed.
    Incomplete,
}

/// Parse one key from the front of `buf` (not empty).
fn parse(buf: &[u8]) -> Parsed {
    let key = |k| Parsed::Key(k, 1);
    match buf[0] {
        b'\r' | b'\n' => key(Key::Enter),
        0x7f | 0x08 => key(Key::Backspace),
        0x1b => parse_escape(buf),
        // Ctrl-A … Ctrl-Z (Tab, a.k.a. Ctrl-I, has no use here).
        b'\t' => Parsed::Skip(1),
        c @ 0x01..=0x1a => key(Key::Ctrl((c - 1 + b'a') as char)),
        0x00 | 0x1c..=0x1f => Parsed::Skip(1),
        c if c < 0x80 => key(Key::Char(c as char)),
        // UTF-8: a lead byte says how long the character is.
        lead => {
            let len = match lead {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Parsed::Skip(1),
            };
            if buf.len() < len {
                return Parsed::Incomplete;
            }
            match std::str::from_utf8(&buf[..len]) {
                Ok(s) => Parsed::Key(Key::Char(s.chars().next().expect("one char")), len),
                Err(_) => Parsed::Skip(1),
            }
        }
    }
}

/// Parse an escape sequence (`buf[0]` is ESC).
fn parse_escape(buf: &[u8]) -> Parsed {
    match buf.get(1) {
        None => Parsed::Incomplete,
        // CSI: parameters, then a final byte in `@`..=`~`.
        Some(b'[') => {
            let Some(end) = buf[2..].iter().position(|b| (0x40..=0x7e).contains(b)) else {
                return Parsed::Incomplete;
            };
            let (params, last) = (&buf[2..2 + end], buf[2 + end]);
            let len = end + 3;
            let key = match (params, last) {
                (_, b'A') => Key::Up,
                (_, b'B') => Key::Down,
                (_, b'C') => Key::Right,
                (_, b'D') => Key::Left,
                (_, b'H') => Key::Home,
                (_, b'F') => Key::End,
                (b"1" | b"7", b'~') => Key::Home,
                (b"4" | b"8", b'~') => Key::End,
                (b"3", b'~') => Key::Delete,
                _ => return Parsed::Skip(len),
            };
            Parsed::Key(key, len)
        }
        // SS3: what some terminals send for arrows, Home and End.
        Some(b'O') => match buf.get(2) {
            None => Parsed::Incomplete,
            Some(b'A') => Parsed::Key(Key::Up, 3),
            Some(b'B') => Parsed::Key(Key::Down, 3),
            Some(b'C') => Parsed::Key(Key::Right, 3),
            Some(b'D') => Parsed::Key(Key::Left, 3),
            Some(b'H') => Parsed::Key(Key::Home, 3),
            Some(b'F') => Parsed::Key(Key::End, 3),
            Some(_) => Parsed::Skip(3),
        },
        // Esc pressed twice: the first is the key.
        Some(0x1b) => Parsed::Key(Key::Esc, 1),
        // Alt with a key.
        Some(_) => Parsed::Skip(2),
    }
}

/// The keyboard: bytes from the reader thread, and what is being parsed.
struct Input {
    bytes: Receiver<u8>,
    buf: VecDeque<u8>,
    last_size: Option<(u16, u16)>,
}

impl Input {
    fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        // Lives as long as the process; ends when stdin does, which shows
        // up in `poll` as the channel closing.
        std::thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut chunk = [0u8; 256];
            loop {
                match stdin.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => {
                        for b in &chunk[..n] {
                            if tx.send(*b).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        });
        Self {
            bytes: rx,
            buf: VecDeque::new(),
            last_size: size(),
        }
    }

    /// Wait up to `wait` for a byte; whether one arrived.
    fn fill(&mut self, wait: Duration) -> io::Result<bool> {
        match self.bytes.recv_timeout(wait) {
            Ok(b) => {
                self.buf.push_back(b);
                self.buf.extend(self.bytes.try_iter());
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the terminal's input ended",
            )),
        }
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        let now = size();
        if now != self.last_size {
            self.last_size = now;
            return Ok(Some(Event::Resize));
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.buf.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                if !self.fill(left)? {
                    return Ok(None);
                }
            }
            match parse(self.buf.make_contiguous()) {
                Parsed::Key(key, n) => {
                    self.buf.drain(..n);
                    return Ok(Some(Event::Key(key)));
                }
                Parsed::Skip(n) => {
                    self.buf.drain(..n);
                }
                // The rest of a sequence follows at once if it is one.
                Parsed::Incomplete => {
                    if !self.fill(ESC_WAIT)? {
                        let lone_esc = self.buf.len() == 1 && self.buf[0] == 0x1b;
                        self.buf.clear();
                        if lone_esc {
                            return Ok(Some(Event::Key(Key::Esc)));
                        }
                    }
                }
            }
        }
    }
}

fn input() -> std::sync::MutexGuard<'static, Input> {
    static INPUT: OnceLock<Mutex<Input>> = OnceLock::new();
    INPUT
        .get_or_init(|| Mutex::new(Input::start()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// The next event, waiting up to `timeout` for one. Fails when the
/// terminal's input ends.
pub fn poll(timeout: Duration) -> io::Result<Option<Event>> {
    input().poll(timeout)
}

/// The next event, however long it takes.
pub fn read() -> io::Result<Event> {
    loop {
        if let Some(ev) = poll(RESIZE_CHECK)? {
            return Ok(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key in `bytes`, parsed as if it had all arrived.
    fn keys(mut bytes: &[u8]) -> Vec<Key> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            match parse(bytes) {
                Parsed::Key(k, n) => {
                    out.push(k);
                    bytes = &bytes[n..];
                }
                Parsed::Skip(n) => bytes = &bytes[n..],
                Parsed::Incomplete => panic!("incomplete: {bytes:?}"),
            }
        }
        out
    }

    #[test]
    fn plain_and_control_keys() {
        assert_eq!(
            keys(b"aZ \r\n\x7f\x08"),
            [
                Key::Char('a'),
                Key::Char('Z'),
                Key::Char(' '),
                Key::Enter,
                Key::Enter,
                Key::Backspace,
                Key::Backspace,
            ]
        );
        assert_eq!(
            keys(b"\x03\x04\x1a"),
            [Key::Ctrl('c'), Key::Ctrl('d'), Key::Ctrl('z')]
        );
        // Tab and stray control bytes are dropped.
        assert_eq!(keys(b"\t\x00\x1fx"), [Key::Char('x')]);
    }

    #[test]
    fn utf8_characters() {
        assert_eq!(
            keys("é›💭".as_bytes()),
            [Key::Char('é'), Key::Char('›'), Key::Char('💭')]
        );
        // A character still arriving is waited for, not mangled.
        assert_eq!(parse(&"💭".as_bytes()[..2]), Parsed::Incomplete);
        // Invalid bytes are skipped one at a time.
        assert_eq!(keys(b"\xff\xc3(a"), [Key::Char('('), Key::Char('a')]);
    }

    #[test]
    fn escape_sequences() {
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1b[H\x1b[F\x1b[3~"),
            [
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Home,
                Key::End,
                Key::Delete
            ]
        );
        assert_eq!(
            keys(b"\x1b[1~\x1b[4~\x1b[7~\x1b[8~\x1bOA\x1bOH\x1bOF"),
            [
                Key::Home,
                Key::End,
                Key::Home,
                Key::End,
                Key::Up,
                Key::Home,
                Key::End
            ]
        );
        // Modified arrows still move; unknown sequences vanish whole.
        assert_eq!(keys(b"\x1b[1;5C"), [Key::Right]);
        assert_eq!(
            keys(b"\x1b[15~\x1b[200~\x1bOPx\x1bfy"),
            [Key::Char('x'), Key::Char('y')]
        );
        // Esc Esc is the key once, then whatever the second begins.
        assert_eq!(keys(b"\x1b\x1b[A"), [Key::Esc, Key::Up]);
    }

    #[test]
    fn partial_sequences_wait_for_more() {
        for partial in [&b"\x1b"[..], b"\x1b[", b"\x1b[1;", b"\x1bO"] {
            assert_eq!(parse(partial), Parsed::Incomplete, "{partial:?}");
        }
    }

    #[test]
    fn cursor_sequences() {
        assert_eq!(move_up(0), "");
        assert_eq!(move_up(2), "\x1b[2A");
        assert_eq!(move_to_column(0), "\x1b[1G");
        assert_eq!(move_to(0, 3), "\x1b[4;1H");
    }

    #[test]
    fn structure_sizes_match_the_kernel() {
        #[cfg(not(target_os = "macos"))]
        assert_eq!(std::mem::size_of::<Termios>(), 36);
        #[cfg(target_os = "macos")]
        assert_eq!(std::mem::size_of::<Termios>(), 72);
        assert_eq!(std::mem::size_of::<WinSize>(), 8);
    }
}
