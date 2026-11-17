//! A pseudo-terminal for a command, as much of one as [`super::terminal`]
//! needs: open a pair, start a child on the slave side as the terminal's
//! own session, set the window size, and — where the system lets it be seen
//! cheaply — tell whether the command is blocked reading the terminal.
//!
//! Made the way the interface's own terminal layer is (`src/tui/term.rs`):
//! `/dev/ptmx` and a handful of `ioctl`s, through the kernel directly on
//! fullrust and through the C library std already links elsewhere, so there
//! is no libc crate and the fullrust build has it too. The one thing that
//! must run in the child before it becomes the command — `setsid` and
//! taking the terminal as its controlling one, without which `/dev/tty`
//! does not open and `ssh`, `sudo` and the like do not prompt — is a
//! `pre_exec` hook making those two system calls and nothing else.
//!
//! Supported where `build.rs` sets `cfg(pty)`: Linux (fullrust included) on
//! the architectures that share its generic terminal `ioctl` numbers, and
//! macOS. Elsewhere [`open`] fails and commands run on pipes, as they did
//! before there was any of this.

use std::fs::File;
#[cfg(not(pty))]
use std::{io, process::Command};

/// The two ends of a pseudo-terminal.
pub struct Pair {
    /// atelier's end: what the command prints is read here, what the user
    /// types is written here.
    pub master: File,
    /// The command's end, to start it on ([`Pair::attach`]) and then drop:
    /// the master reads end-of-file only once nobody holds this side.
    slave: File,
    /// The slave's path (`/dev/pts/7`).
    pub path: String,
}

/// Whether this build can open a pseudo-terminal at all.
pub const SUPPORTED: bool = cfg!(pty);

#[cfg(not(pty))]
pub fn open() -> io::Result<Pair> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this build of atelier has no pseudo-terminal support for this system",
    ))
}

#[cfg(not(pty))]
impl Pair {
    pub fn attach(self, _cmd: &mut Command) -> io::Result<File> {
        Ok(self.master)
    }
}

#[cfg(not(pty))]
pub fn resize(_master: &File, _cols: u16, _rows: u16) {}

#[cfg(not(pty))]
pub fn reading(_session: u32, _path: &str) -> Option<bool> {
    None
}

#[cfg(pty)]
pub use imp::{open, reading, resize};

#[cfg(pty)]
mod imp {
    use super::Pair;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    /// `struct winsize`.
    #[repr(C)]
    #[derive(Default)]
    struct WinSize {
        rows: u16,
        cols: u16,
        xpixel: u16,
        ypixel: u16,
    }

    /// Requests and flags: Linux's generic values (x86, ARM, RISC-V).
    #[cfg(not(target_os = "macos"))]
    mod os {
        pub const O_NOCTTY: i32 = 0o400;
        pub const SET_SIZE: u64 = 0x5414; // TIOCSWINSZ
        pub const SET_CONTROLLING: u64 = 0x540E; // TIOCSCTTY
        pub const UNLOCK: u64 = 0x4004_5431; // TIOCSPTLCK
        pub const NUMBER: u64 = 0x8004_5430; // TIOCGPTN
    }

    #[cfg(target_os = "macos")]
    mod os {
        pub const O_NOCTTY: i32 = 0x20000;
        pub const SET_SIZE: u64 = 0x8008_7467; // TIOCSWINSZ
        pub const SET_CONTROLLING: u64 = 0x2000_7461; // TIOCSCTTY
        pub const GRANT: u64 = 0x2000_7454; // TIOCPTYGRANT
        pub const UNLOCK: u64 = 0x2000_7452; // TIOCPTYUNLK
        pub const NAME: u64 = 0x4080_7453; // TIOCPTYGNAME
    }

    /// `ioctl(fd, request, arg)` with `arg` a plain value (or an address).
    #[cfg(target_os = "fullrust")]
    fn ioctl_value(fd: i32, request: u64, arg: usize) -> io::Result<()> {
        use std::os::fullrust::syscall::{nr, syscall3};
        // SAFETY: the callers pass, for each request, the value or the
        // address of the live structure the kernel expects for it.
        unsafe { syscall3(nr::IOCTL, fd as usize, request as usize, arg) }.map(drop)
    }

    #[cfg(not(target_os = "fullrust"))]
    fn ioctl_value(fd: i32, request: u64, arg: usize) -> io::Result<()> {
        use std::ffi::{c_int, c_ulong};
        unsafe extern "C" {
            fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        }
        // SAFETY: as above; `ioctl` is the C library's, which std links.
        if unsafe { ioctl(fd, request as c_ulong, arg) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `ioctl(fd, request, arg)`, where `arg` is the structure `request`
    /// reads or fills.
    fn ioctl<T>(fd: i32, request: u64, arg: &mut T) -> io::Result<()> {
        ioctl_value(fd, request, std::ptr::from_mut(arg) as usize)
    }

    /// `setsid()`: the calling process becomes the leader of a new session,
    /// which has no controlling terminal yet.
    #[cfg(target_os = "fullrust")]
    fn setsid() -> io::Result<()> {
        use std::os::fullrust::syscall::{nr, syscall0};
        // SAFETY: a system call with no arguments.
        unsafe { syscall0(nr::SETSID) }.map(drop)
    }

    #[cfg(not(target_os = "fullrust"))]
    fn setsid() -> io::Result<()> {
        unsafe extern "C" {
            fn setsid() -> std::ffi::c_int;
        }
        // SAFETY: the C library's, which std links; no arguments.
        if unsafe { setsid() } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The path of the slave side of `master`, made ready to be opened.
    #[cfg(not(target_os = "macos"))]
    fn slave_path(master: &File) -> io::Result<String> {
        let fd = master.as_raw_fd();
        let mut unlocked: i32 = 0;
        ioctl(fd, os::UNLOCK, &mut unlocked)?;
        let mut number: u32 = 0;
        ioctl(fd, os::NUMBER, &mut number)?;
        Ok(format!("/dev/pts/{number}"))
    }

    #[cfg(target_os = "macos")]
    fn slave_path(master: &File) -> io::Result<String> {
        let fd = master.as_raw_fd();
        ioctl_value(fd, os::GRANT, 0)?;
        ioctl_value(fd, os::UNLOCK, 0)?;
        let mut name = [0u8; 128];
        ioctl(fd, os::NAME, &mut name)?;
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Ok(String::from_utf8_lossy(&name[..len]).into_owned())
    }

    /// Open a pseudo-terminal. Neither end becomes atelier's own
    /// controlling terminal, whatever atelier was started from.
    pub fn open() -> io::Result<Pair> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).custom_flags(os::O_NOCTTY);
        let master = options.open("/dev/ptmx")?;
        let path = slave_path(&master)?;
        let slave = options.open(&path)?;
        Ok(Pair {
            master,
            slave,
            path,
        })
    }

    impl Pair {
        /// Make the slave side `cmd`'s stdin, stdout and stderr, and its
        /// controlling terminal, in a session of its own — what a terminal
        /// emulator does for a shell. Returns the master; the slave is
        /// `cmd`'s from here, and is closed on this side when `cmd` is
        /// dropped.
        pub fn attach(self, cmd: &mut Command) -> io::Result<File> {
            cmd.stdin(Stdio::from(self.slave.try_clone()?))
                .stdout(Stdio::from(self.slave.try_clone()?))
                .stderr(Stdio::from(self.slave));
            // SAFETY: the hook runs in the forked child before `exec` and
            // makes two system calls; it allocates nothing and takes no
            // lock.
            unsafe {
                cmd.pre_exec(|| {
                    setsid()?;
                    // Descriptor 0 is the slave by now. 0: do not take the
                    // terminal from another session (there is none).
                    ioctl_value(0, os::SET_CONTROLLING, 0)
                });
            }
            Ok(self.master)
        }
    }

    /// Tell the terminal behind `master` its window size. Best effort.
    pub fn resize(master: &File, cols: u16, rows: u16) {
        let mut size = WinSize {
            rows,
            cols,
            ..WinSize::default()
        };
        let _ = ioctl(master.as_raw_fd(), os::SET_SIZE, &mut size);
    }

    /// Whether a process of the session `session` (the command's: its
    /// shell's pid) is blocked in a `read` of the terminal at `path`:
    /// `Some(true)` when one is seen to be, `Some(false)` when every one of
    /// them was seen doing something else, `None` when that cannot be told —
    /// a process that may not be looked at (`sudo`), one waiting in `poll`
    /// for it does not say what, or a system without `/proc/<pid>/syscall`.
    #[cfg(any(
        target_os = "macos",
        not(any(target_arch = "x86_64", target_arch = "aarch64"))
    ))]
    pub fn reading(_session: u32, _path: &str) -> Option<bool> {
        None
    }

    #[cfg(all(
        not(target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub fn reading(session: u32, path: &str) -> Option<bool> {
        // The system calls' numbers: `read`, and the ones that wait for any
        // of several descriptors.
        #[cfg(target_arch = "x86_64")]
        const CALLS: (u64, &[u64]) = (0, &[7, 23, 232, 270, 271, 281]);
        #[cfg(target_arch = "aarch64")]
        const CALLS: (u64, &[u64]) = (63, &[21, 22, 72, 73]);
        let (read, waits) = CALLS;

        let mut seen = false;
        let mut unknown = false;
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // After the name, which may hold anything: state, parent,
            // group, session.
            let fields = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
            let mut fields = fields.split_whitespace();
            let state = fields.next().unwrap_or("");
            if fields.nth(2).and_then(|s| s.parse::<u32>().ok()) != Some(session) || state == "Z" {
                continue;
            }
            seen = true;
            let Ok(call) = std::fs::read_to_string(format!("/proc/{pid}/syscall")) else {
                unknown = true;
                continue;
            };
            let mut words = call.split_whitespace();
            let number = words.next().and_then(|n| n.parse::<u64>().ok());
            let descriptor = words
                .next()
                .and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok());
            match (number, descriptor) {
                (Some(n), Some(fd)) if n == read => {
                    let target = std::fs::read_link(format!("/proc/{pid}/fd/{fd}"));
                    if target.is_ok_and(|t| t.as_os_str() == path || t.as_os_str() == "/dev/tty") {
                        return Some(true);
                    }
                }
                (Some(n), _) if waits.contains(&n) => unknown = true,
                _ => {}
            }
        }
        (seen && !unknown).then_some(false)
    }
}

#[cfg(all(test, pty))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::process::Command;

    #[test]
    fn a_command_on_the_pair_has_a_terminal_of_its_own() {
        let pair = open().unwrap();
        let path = pair.path.clone();
        let mut cmd = Command::new("sh");
        // `tty` names the terminal on stdin; `/dev/tty` opens only for a
        // process that has a controlling one.
        cmd.arg("-c")
            .arg("tty; stty size; read answer < /dev/tty; echo \"got $answer\"");
        let mut master = pair.attach(&mut cmd).unwrap();
        resize(&master, 132, 43);
        let mut child = cmd.spawn().unwrap();
        drop(cmd);
        master.write_all(b"hello\n").unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        // The read fails (Linux) or ends (macOS) once the command is gone.
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert!(child.wait().unwrap().success());
        let out = String::from_utf8_lossy(&out);
        assert!(out.contains(&path), "{out}");
        assert!(out.contains("43 132"), "{out}");
        assert!(out.contains("got hello"), "{out}");
    }
}
