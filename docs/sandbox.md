# Sandboxed `bash` (experimental)

With the sandbox on, the `bash` tool runs its commands inside
[nixvm](https://github.com/KarpelesLab/nixvm): a real Linux userland (Alpine,
typically) whose system calls are served by nixvm's kernel, in Rust, inside
atelier's own process tree — not by the host's. The guest sees the project
directory and a userland image, and nothing else of the machine. A command
that cannot leave the project needs no approval, so `bash` stops prompting.

This is a first implementation, **off by default and not in the release
binaries**. It isolates what it says it isolates (the tests check), but the
guest cannot yet do everything an agent's shell is asked to do — see
[what works](#what-works-in-the-guest-and-what-doesnt) before relying on it,
and [what nixvm needs next](#what-nixvm-needs-next) for where this is going.

## Turning it on

1. **Build with the feature.** `cargo build --release --features sandbox`.
   It needs a Unix target with a libc (tried on Linux x86-64 with glibc
   only); in a fullrust build the feature compiles but reports itself
   unavailable ([why](#builds)).

2. **Provide a root filesystem**: a directory holding a Linux userland.
   atelier looks at `ATELIER_SANDBOX_ROOT`, then `[sandbox] root` in the
   *user-global* `atelier.toml`, then
   `$XDG_DATA_HOME/atelier/sandbox/rootfs` (`~/.local/share/…`). Nothing is
   downloaded for you. An Alpine minirootfs (3.5 MB to download, 9 MB
   unpacked) is enough to start:

   ```sh
   root=~/.local/share/atelier/sandbox/rootfs
   mkdir -p "$root"
   curl -fsSL https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.10-x86_64.tar.gz \
     | tar xz -C "$root"
   ```

   That is busybox and `apk`, nothing else. Add the tools your projects use
   by running `apk` in the guest, with the two switches that exist for this
   and only for this — the root mounted read-write, and the host's network:

   ```sh
   atelier sandbox run --net --rw-root -- apk add git make python3 ripgrep
   ```

   (`apk` complains that it cannot set ownership — the files belong to you,
   not to root — and installs anyway. Git with make is 25 MB; adding
   Python, Node, gcc and Rust brings the root to about 950 MB.) Use the
   image's architecture that matches the machine: only then can nixvm use
   hardware virtualization.

3. **Enable it**, per project or for yourself:

   ```toml
   [sandbox]
   enabled = true
   # memory_mb = 4096     # guest RAM ceiling
   ```

   or `ATELIER_SANDBOX=1` (`0` turns it off whatever the files say).
   `ATELIER_SANDBOX_INTERP=1` forces nixvm's software CPU.

`atelier sandbox status` says what will happen in the current directory —
whether it is on, which root, which CPU backend, what is read-only — and
`atelier sandbox run -- <command>` runs one command the way the tool would.

## What changes for `bash`

- **Foreground commands run in the guest and are not put to you.** Deny and
  ask rules in `[permissions]` still apply: they are judged before a tool's
  own default, so `deny = ["bash(rm -rf *)"]` refuses the command sandbox or
  not, and `ask = ["bash(git push *)"]` still asks.
- **`"sandbox": false`** on a call asks for the host instead. That is an
  unconfined command like any `bash` call without the sandbox: it prompts,
  unless an allow rule you wrote covers the command or approvals are
  switched off (`ATELIER_APPROVE=all`). The result ends with
  `[ran on the host, not in the sandbox]`.
- **Background jobs run on the host**, and so ask as before; the result says
  so. Each command gets a fresh guest, so a server started in one could not
  be reached from the next.
- **A failure that looks like the sandbox's doing says so**, e.g.

  ```
  [exit status: exit status: 127]
  [stderr]
  /bin/sh: cargo: not found
  [sandbox: a program it needs is not in the sandbox's image (the host's
  toolchains are not: the guest has its own userland). If this command needs
  the host, call bash again with "sandbox": false and say why — the user
  will be asked to approve it.]
  ```

  Recognised: a missing program (status 126/127), name resolution and
  unreachable-network errors, writes refused as read-only or for want of
  permission, "not a git repository", system calls nixvm does not implement
  (its own ledger, and `ENOSYS`/`EOPNOTSUPP` in the output), crashes, a
  timeout on the software CPU, and the sandbox failing to start. An ordinary
  failure — a failing test, a compile error — gets no such line.
- **On but unavailable is a refusal**, not a silent run on the host: with
  `enabled = true` and no root filesystem (or a build without the feature),
  `bash` returns an error naming the reason and the `"sandbox": false` way
  out.
- The tool's description gains a paragraph telling the model all this, only
  while the sandbox is on.

## What the guest sees

| Guest path | What it is |
|---|---|
| `/` | the root filesystem, read-only, under a writable layer in memory |
| the project's own absolute path | the project, read-write; the working directory |
| `/tmp`, `/dev`, `/proc`, `/sys` | the guest's own |

The project is mounted where it is on the host, so paths in a command's
output are the paths the other tools use. The directories above it exist in
the guest as empty directories. Nothing else of the host is reachable: not
your home, not `~/.ssh`, not the environment (the guest gets `PATH`, `HOME`,
`USER`, `TERM`, `LANG`, `PWD` and a `safe.directory` entry for git), not the
network. It runs as root in the guest, which is you on the host: files it
creates are yours.

**Read-only inside the project**: `.git/hooks`, `.git/config` and
`atelier.toml` — what decides which programs run on the host *later* (git
hooks and `core.hooksPath`/`core.fsmonitor`/aliases; atelier's permission
rules, hooks and MCP servers). Where `.git` is not a directory (a linked
worktree, a submodule, no repository yet) all of `.git` is. They cannot be
written, replaced, removed, or carried off by renaming a directory above
them; the tests try twenty-odd routes, through links and `..` included.
What is *not* covered: other files a later host command may run — a
`Makefile`, `build.rs`, `package.json` scripts. That is the project's
content, and the agent can edit it with `write` as well.

Nothing persists between commands except what was written in the project:
the next command starts in a fresh guest (`/tmp`, installed packages and
environment are gone).

Each command runs in a process of its own (`atelier sandbox-exec`), killed if
it outlives its timeout by three seconds, so a guest that wedges the emulator
cannot wedge the session.

## What works in the guest, and what doesn't

Measured with nixvm 0.0.5 (the version this is built on) and Alpine 3.20
x86-64 on KVM; "HEAD" is nixvm's unreleased master at the time of writing.

| | |
|---|---|
| busybox, `ls`/`find`/`grep -r`, `rg` over the project | works |
| `git status`, `diff`, `log`, `checkout <file>` | works |
| **`git add`, `git commit`** | **fails**: "unable to write loose object file: Permission denied". Git creates each object read-only (`0444`) and writes through the descriptor it still holds; nixvm's host-directory filesystem works by path and reopens the file, which the host refuses. Any program writing a file it created read-only hits this. Also at HEAD. |
| a linked git worktree, a submodule | git finds no repository: its data is outside the project |
| `python3`, `node` (JIT included), subprocesses from both | works |
| `gcc`, `make` | works; `make -j` fails on 0.0.5 ("init jobserver pipe"), works at HEAD or with `--jobserver-style=pipe` |
| **`cargo`, `rustc`** | **fails** to start child processes: Rust's `posix_spawn` path asks for `socketpair(AF_UNIX, SOCK_SEQPACKET)`, which nixvm answers `EOPNOTSUPP`. `rustc --emit=obj` works; linking and anything through `cargo` does not. Also at HEAD. |
| the host's own toolchains (`~/.cargo`, `~/.rustup`, nvm, system compilers) | not there: they are outside the project, and glibc programs do not run on a musl root anyway |
| `apk add`, `pip install`, `npm install`, `cargo fetch` | no network. (With the network, by hand: `apk add` works.) |
| FIFOs in the project | `mkfifo f; rm f` wedges nixvm's kernel thread, past its own time limit; on 0.0.5 so does reading a FIFO already there (`grep -r` over it). atelier kills the process. |

The story for a project's own toolchain, today: it has to be in the image
(`apk add`), in a version Alpine packages (Rust 1.78 in Alpine 3.20; this
repository needs 1.95), and even then `cargo` does not run. In practice the
commands an agent runs most — build, test, commit — still go to the host and
still ask. What the sandbox takes off your hands now is the long tail of
looking around: listing, searching, reading git state, running scripts.

## Speed

One machine: AMD Ryzen Threadripper 9970X (32 cores), 125 GiB, Linux 6.18,
KVM available. Wall time of the whole call, release build; the project is
this repository's `src/` and top level (116 files, 1.6 MB).

| Command | Host | Guest, KVM | Guest, software CPU |
|---|---|---|---|
| `echo hi` | 2 ms | 45–65 ms | 4–17 ms |
| `ls -R \| wc -l` | 3 ms | 60–80 ms | 35 ms |
| `grep -r requires_approval src` (busybox) | 4 ms | 140–190 ms | 5.6 s |
| `rg requires_approval src` | — | 85–105 ms | 0.25 s |
| `git status --short; git log` | — | 105–135 ms | 0.32 s |
| shell loop, 200,000 iterations | 0.29 s | 0.25 s | 68 s |
| `python3 -c 'sum(i*i for i in range(10**6))'` | 0.05 s | 0.10 s | 20.7 s |
| `node -e` loop to 10⁷ | 0.08 s | 0.32 s | 23.5 s |
| `gcc` hello world, compile and run | — | 0.20 s | 1.8 s |
| `gcc -O2` on a 4,000-function file | 2.6 s | 13.0 s | not tried |
| the same, four at once: 1 vCPU / 4 vCPUs | — | 55 s / 30 s | — |
| `python3 -m venv v` | — | 7.0 s | — |
| `apk add git make` (network, 20 MiB) | — | 10.8 s | — |

On KVM there is a floor of about 50 ms per command (creating the VM), and
compute is 1–5× the host: fine for an agent's small commands, a real cost
for a compiler. The software CPU starts faster than KVM and is two orders of
magnitude slower at anything that computes: usable for `ls` and `git
status`, not for a build. It is what runs wherever `/dev/kvm` is not
accessible (most containers and CI), on macOS without a signed binary, and
for an image of the other architecture. atelier gives the guest one vCPU.

## Builds

nixvm 0.0.5 as a library has no dependencies; the feature adds 3.4 MB to the
release binary (18.3 → 21.8 MB; 15.0 → 17.8 MB stripped) and about 35 s to a
release build. The default build is unchanged: the code that touches nixvm is
compiled only with the feature, on `cfg(unix)`.

**fullrust.** nixvm builds for `x86_64-unknown-linux-fullrust` and boots
Alpine there from an image unpacked into memory — but its host-directory
filesystem (`fs::Passthrough`) is `#[cfg(unix)]` and calls libc's `openat`,
`readdir` and friends through `extern "C"`, and KVM is gated on
`target_os = "linux"`. fullrust is neither `unix` nor `linux` and has no
libc, so in a fullrust build there is no way to mount the project, and no
hardware virtualization. `--features sandbox` still compiles there, and
`atelier sandbox status` says the sandbox is unavailable in this build. This
is the first thing to fix for the sandbox to ship in the release binaries.

## What is not done

- **Network allowlist** (`[sandbox] network = ["host.example"]`). nixvm has
  the hook — `Kernel::set_egress` takes an `Egress` whose `connect_tcp(ip,
  port)` and `open_udp()` see every outside connection — but it is handed IP
  addresses, not names. Allowing by host name needs atelier to answer the
  guest's DNS itself and remember which addresses it gave out for allowed
  names; that is not written, and a filter that could be talked round is
  worse than none. The guest has no network at all, and a `network` list in
  `atelier.toml` is reported by `sandbox status` as ignored.
- **Background jobs** in the guest.
- **`[sandbox] root` from a project file** is ignored on purpose: the root is
  what the guest can read, and a cloned repository naming `/` would hand it
  the machine.
- **The Claude backend's and the `node` tool's commands**, hooks, diagnostics
  checkers and MCP servers are not sandboxed; only the `bash` tool is.

## What nixvm needs next

In the order that unblocks atelier, with what atelier does once each lands.

1. **A host-directory filesystem without libc, and KVM on fullrust.**
   `fs::Passthrough` on raw system calls (or on `std::os::fd` where std
   suffices) and gated on something fullrust satisfies; the same for
   `vcpu::kvm`'s `target_os = "linux"` and `kernel::egress`'s `setsockopt`.
   *Then:* the `sandbox` feature can be in the release binaries.
2. **Writing through a descriptor to a file the path no longer allows**
   (created `0444`, or since made read-only, or unlinked): the host-directory
   filesystem should keep the host file open for the life of the guest's
   descriptor instead of reopening by path. *Then:* `git add`/`git commit`
   work, and the commit flow stays in the sandbox.
3. **`socketpair(AF_UNIX, SOCK_SEQPACKET)`** (and whatever is behind it once
   that answers): *then* `cargo` and `rustc` can start their children, and
   an image with a Rust toolchain can build.
4. **Never block the kernel thread on the host**: opening or removing a FIFO
   in a host directory hangs the VM with its wall-clock limit unenforced
   (`mkfifo f; rm f`). Either refuse special files in a host directory or
   open them non-blocking; and enforce `Limits::wall_time` from outside the
   thread that may be stuck. *Then:* atelier could drop the kill-the-helper
   fallback (it will keep the helper process for the reasons below).
5. **An embedding API for "run this command"**, so atelier stops assembling
   the guest from `Kernel`, `MountTable`, `loader` and `vcpu` by hand (about
   80 lines that track nixvm's internals, hence the exact version pin):
   `SandboxBuilder` with the working directory, binds at arbitrary guest
   paths (it has `bind`/`bind_ro`, but `run()` always starts in `/root`,
   mounts the host's working directory at `/work` and takes the terminal),
   stdout/stderr/stdin as writers and readers, a wall-clock limit, an
   explicit network switch, and a result carrying the exit status, whether
   the limit was hit, and the unsupported-call ledger. Details found on the
   way: the vDSO clock calibration is unreachable from outside
   (`GuestMemory::phys_arc` is `pub(crate)`); the address-space size is a
   magic constant per release (32 GiB in 0.0.5, where 1 TiB makes the x86-64
   interpreter fault at startup); `InterpBackend::new(X86_64)` errors while
   `vcpu::select` knows to use `interp_x86::X86Backend`; the limit, guest
   faults and policy denials are `eprintln!`ed by the library, which a
   terminal application cannot have on its screen (atelier's helper process
   exists partly to catch them); `NIXVM_NET=host` and a dozen other
   environment variables change a library's behaviour from outside
   (atelier clears the helper's environment). *Then:* the pin becomes a
   normal version requirement.
6. **Read-only paths inside a writable host directory**, including single
   files, that survive a rename of a parent. Mounts are matched by path, so
   a read-only mount at `.git/hooks` is defeated by `mv .git g` (tested);
   atelier wraps `Passthrough` in a filter of its own instead. A
   `Passthrough::protect(paths)` would do.
7. **Egress by name**: the guest's resolver answered by the embedder (or the
   name passed to `Egress::connect_tcp`), so an allowlist of hosts can be
   exact. *Then:* `[sandbox] network`.
8. **A released root image and a fetch that needs no C**: today the image is
   the user's to download and unpack. A pinned, digest-verified Alpine (plus
   a "dev" variant with git, make, a C compiler) that nixvm or atelier can
   fetch with a pure-Rust client and read without unpacking (squashfs is
   there behind `fstool`), and a way to keep installed packages: a
   persistent writable layer on disk instead of the in-memory one. *Then:*
   first run is "enable it", and `apk add` in a session sticks.
9. **Speed where there is no KVM**: the interpreters are 100–300× the host
   on compute. A JIT, or KVM/arm64 and HVF without a signed binary, decide
   whether the sandbox is usable in CI, in containers and on Macs.
10. **A warm VM**: the 50 ms KVM floor is mostly VM creation; keeping one
    guest per session (a persistent shell, or a snapshot to fork from) would
    remove it and make background jobs and a persistent `/tmp` possible.
11. **Host toolchains in the guest**: a glibc root (HEAD's `oci` feature
    runs Docker images, Ubuntu's among them by its changelog; not tried
    here) plus read-only binds of `~/.cargo`,
    `~/.rustup` and the like, so the project's own toolchain is the host's.
    *Then:* "until a command is known to work there, it falls back to
    asking" can become the exception rather than the rule.
