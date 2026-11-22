# Sandboxed `bash` (experimental)

With the sandbox on, the `bash` tool — foreground commands, background
jobs —, the shell lines of command files, and the commands the project has
atelier run by itself — its hooks and its diagnostics checkers
([what runs where](#what-runs-where)) — run inside
[nixvm](https://github.com/KarpelesLab/nixvm): a real Linux userland (Alpine,
typically) whose system calls are served by nixvm's kernel, in Rust, inside
atelier's own process tree — not by the host's. The guest sees the project
directory and a userland image, and nothing else of the machine; of the
network, only the hosts you list, and none unless you list some. A command
that cannot leave the project needs no approval, so `bash` stops prompting.

This is a first implementation, **off by default**; the x86-64 release
binary has it compiled in, the arm64 one does not ([builds](#builds)). It
isolates what it says it isolates (the tests check), but the
guest cannot yet do everything an agent's shell is asked to do — see
[what works](#what-works-in-the-guest-and-what-doesnt) before relying on it,
and [what nixvm needs next](#what-nixvm-needs-next) for where this is going.

## Turning it on

1. **Have a build with it.** The x86-64 release binary has; from source,
   `cargo build --release --features sandbox` (Linux x86-64 with glibc and
   with [fullrust](#builds) were tried; `atelier sandbox status` says if a
   build cannot).

2. **A root filesystem** — the guest's userland. If you name none, atelier
   fetches one the first time a guest needs it: the official Alpine image
   (3.22, pinned by digest: `docker.io/library/alpine:3.22@sha256:49340b34…`),
   pulled from Docker Hub over HTTPS, each part checked against its digest —
   3.5 MB, once, into `$XDG_DATA_HOME/atelier/sandbox/image`
   (`~/.local/share/…`). That download is the only thing the sandbox ever
   fetches by itself. It happens only with the sandbox enabled and no root
   of yours configured; it is said in the result of the command that
   caused it (`atelier: sandbox: … fetching the image … from its registry,
   once, into …`), and `atelier sandbox status` says beforehand that it
   will. `atelier sandbox fetch` does it at a moment you choose. Without a
   network the command fails saying so, nothing runs on the host instead,
   and the next command tries again.

   That image is busybox and `apk`, nothing else. **Add the tools your
   projects use** with the two switches that exist for this and only for
   this — `--rw-root`, which keeps what the command changes in the root,
   and `--net`, the host's network:

   ```sh
   atelier sandbox run --net --rw-root -- apk add git make python3 ripgrep
   ```

   What it installs goes to a layer of its own on disk
   (`…/sandbox/image/layers/<image>`) and is there for every guest from
   then on, in every project. One such command at a time (a second is told
   so); other guests go on reading the layer meanwhile. The files are yours
   on the host, where nobody can hand them to root: atelier shows them to
   the guest as root's, and takes a `chown` the host refuses as done — so
   `apk add` succeeds rather than installing everything and exiting with an
   error per file. (Git with make is 25 MB; adding Python, Node, gcc and
   Rust comes to about 950 MB.)

   **A root of your own** is used instead if you name one: a directory
   holding a Linux userland, in `ATELIER_SANDBOX_ROOT` or `[sandbox] root`
   of the *user-global* `atelier.toml` — or one unpacked at
   `$XDG_DATA_HOME/atelier/sandbox/rootfs`. Then nothing is downloaded, and
   a root you named that is not there is an error, not a reason to fetch
   one:

   ```sh
   root=~/.local/share/atelier/sandbox/rootfs
   mkdir -p "$root"
   curl -fsSL https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.10-x86_64.tar.gz \
     | tar xz -C "$root"
   ```

   With a directory, `--rw-root` writes into the directory itself, and
   `apk` complains that it cannot set ownership (and exits with an error)
   while installing all the same. Use the architecture that matches the
   machine: only then can nixvm use hardware virtualization.

3. **Enable it**, per project or for yourself:

   ```toml
   [sandbox]
   enabled = true
   # memory_mb = 4096     # guest RAM ceiling
   # network = ["crates.io", "*.crates.io"]   # hosts the guest may reach
   ```

   or `ATELIER_SANDBOX=1` (`0` turns it off whatever the files say).
   `ATELIER_SANDBOX_INTERP=1` forces nixvm's software CPU. In a project's
   `atelier.toml` the switch is a [gated setting](trust.md): it counts —
   on or off — once you have accepted it with `/trust`, so a repository
   cannot switch your sandbox off, or stop `bash` from asking, by itself.
   The same goes for `network` ([below](#the-network)).

`atelier sandbox status` says what will happen in the current directory —
whether it is on and what then runs in it, which root (and whether it is
still to be fetched), which CPU backend, what is read-only, which hosts can
be reached — and `atelier sandbox run -- <command>` runs one command the way
the tool would (with your terminal's input as its stdin).

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
- **Background jobs run in a guest of their own**, unasked like foreground
  commands, until they end or `bash_kill` stops them. Each command gets a
  fresh guest, so a server started in one cannot be reached from the next:
  start the server and its client in the same command.
- **A [command file's](commands.md) `` !`command` `` lines run in a guest**
  too, with their arguments. (With the sandbox on but unavailable they run
  on the host, and ask like a `"sandbox": false` call.)
- **A failure that looks like the sandbox's doing says so**, e.g.

  ```
  [exit status: 127]
  [stderr]
  /bin/sh: cargo: not found
  [sandbox: a program it needs is not in the sandbox's image (the host's
  toolchains are not: the guest has its own userland). If this command needs
  the host, call bash again with "sandbox": false and say why — the user
  will be asked to approve it.]
  ```

  Recognised: a missing program (status 126/127), name resolution and
  unreachable-network errors — with a network allowlist, exactly what it
  refused, by name ([the network](#the-network)) —, writes refused as
  read-only or for want of
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

## What runs where

`bash` is not the only thing atelier runs. With the sandbox on, a command
goes to the guest when it **executes what is in the project** and atelier
runs it **without asking**: the guest can write the project, unasked, so
such a command on the host would be the way out — the agent edits
`scripts/check.sh` or `build.rs` in the guest, and atelier runs it outside.
What is the user's own, or cannot work in a guest that sees only the
project, stays on the host, and is listed here so that nobody has to guess.

| What | Runs | Why |
|---|---|---|
| `bash`, background jobs, a command file's `` !`command` `` lines | guest | [above](#what-changes-for-bash) |
| The commands of a [`node` script](scripting.md) | guest | A script starts no process: its commands are `tools.call("bash", …)`, served like the model's own calls — in the guest unasked, on the host with `"sandbox": false` and your approval |
| **Project hooks** (`[[hooks]]` in the project's `atelier.toml`) | guest | They run the repository's scripts, on every tool call, unasked |
| **Diagnostics checkers**, configured or detected | guest | They run the project's build: `cargo check` runs build scripts and proc macros, `npx tsc` a binary from `node_modules` |
| **Your own hooks** (the user-global `atelier.toml`) | host | They are your commands, they usually live outside the project (a notifier, a script in your home), and nothing in the project can add one |
| `node`'s own `fs`, and its `fetch` with `network: true` | host, in atelier's process | Not a command: `fs` is confined to the project by atelier, the network asks. `fs` is **not** held to the guest's [read-only paths](#what-the-guest-sees): a script can write `.git/hooks` or `atelier.toml`, as `write` can |
| MCP servers (`[[mcp]]`) | host | [below](#what-stays-on-the-host-and-what-it-would-take) |
| Language servers (the `symbols` tool) | host | below — and note that they run the project's code |
| The Claude backend (`claude`) | host | below |
| A provider's `api_key_command`, the browser for a login, atelier's own `git` calls for context | host | The first two are about your credentials; `git` only reads, and what it could be made to run (`.git/config`, hooks) the guest cannot write |

### Project hooks

A hook from the project's `atelier.toml` runs as `sh -c` in a fresh guest,
in the project, for its `timeout_secs`. It is told about the event as on the
host — the JSON on stdin, `ATELIER_EVENT`, `ATELIER_TOOL`, `ATELIER_FILE`,
`ATELIER_PROJECT` — and its exit status and stdout [mean what they mean
there](hooks.md#how-a-hook-answers). What differs:

- **The image's programs, not yours.** A hook that calls `rustfmt`, `jq` or
  `prettier` needs them in the image (`apk add`), and gets the image's
  version. Of the environment it has the guest's (`PATH`, `HOME`, …) and the
  four `ATELIER_*` variables, nothing of yours.
- **No network** beyond [`[sandbox] network`](#the-network), and nothing of
  the machine outside the project: a hook that posts to a chat, plays a
  sound or writes a log in your home cannot. That hook is yours rather than
  the project's: put it in your own `atelier.toml`, where it runs on the
  host.
- **About 50 ms each** on KVM (a guest is created per hook), and much more
  for anything that computes on the software CPU.
- **A failure that looks like the sandbox's doing says so**, in the notice a
  failed hook gets: `hook post_tool (prettier --write …) failed: exit 127:
  sh: prettier: not found [sandbox: a program it needs is not in the
  sandbox's image …]`. As on the host, a hook that fails decides nothing and
  the session goes on.
- **On but unavailable, project hooks do not run** — no root filesystem, a
  build without the feature —, with a notice each time saying so. They are
  never run on the host instead. A hook that guards something (`exit 2`)
  therefore guards nothing until the sandbox works or is switched off;
  `bash` refuses its commands in that state too.
- The event's JSON is the hook's stdin and the `ATELIER_*` variables its
  environment, as on the host: handed to the process that runs the guest,
  not written into a command line or a file. Nothing is created in the
  project for a hook to run.

Project hooks still need [the project trusted](trust.md) before they run at
all; the sandbox adds to that, it replaces none of it. `/hooks` says where
each hook runs.

Your own hooks are not sandboxed, whatever they run. One that runs the
project's code — `command = "npm run lint"` in your global file — runs it on
the host, where a sandboxed command may have changed what `lint` means. Keep
those to tools that do not execute the project, or make them project hooks.

### Diagnostics checkers

A [checker](configuration.md#diagnostics) runs in a fresh guest, in the
project, with its words as written (no shell, as on the host), for its
timeout. Without `[[diagnostics]]`, the checkers are picked by what **the
image** has — `cargo`, `npx`, `go`, `ruff`, a `python3` that can import
pyflakes — not by what your machine has: a Rust project gets `cargo check`
once the image has cargo (`apk add cargo`), and none before.

- A checker whose program the image lacks is reported once, as the
  sandbox's doing, and skipped for the session — as one that cannot start is
  on the host.
- Its dependencies must be on disk or fetchable: `cargo check` of a project
  with crates.io dependencies needs `crates.io` and `*.crates.io` on
  [`[sandbox] network`](#the-network), or a vendored/fetched registry in
  the project. A run that fails for want of the network says so.
- It is the image's toolchain, so the diagnostics are that version's, and
  `target/` is shared with whatever you build on the host. A first `cargo
  check` in a cold guest may pass the 60 s a detected checker gets; it is
  tried once more after the next change.
- On but unavailable: reported once as not run, and not run on the host.

### What stays on the host, and what it would take

**MCP servers.** A stdio server is a long-lived process that atelier talks
to over its stdin and stdout, usually an `npx` or `uvx` package that calls a
service with a token from your environment. In the guest it would have none of your environment, an empty home, no
network beyond the allowlist, and no `node` unless the image has one — and
a fresh guest per command, where a server needs one for the session. Servers are
not the project's content either: `[[mcp]]` names a command, which
[trust](trust.md) gates, and `atelier.toml` is read-only in the guest, so a
sandboxed command cannot add or change one. *It would take:* a guest that lives as long as the session, with the
server's stdin and stdout streamed (a guest now has a stdin and an
environment, which hooks use), and a per-server network list —
and would be worth it for a server the project ships in its own tree (`command
= "./tools/server.py"`), which today runs on the host with the project's
code. That case is not covered: trust such a project knowing it.

**Language servers.** Same shape (long-lived, spoken to over stdio), and
they need the host's toolchain to say anything true about your build. They
are the one thing left that runs the project's code on the host unasked:
rust-analyzer runs build scripts and proc macros, the TypeScript server
loads `node_modules/typescript`. They start only in a trusted project and
only when `symbols` is first used; with the sandbox on that is still a way
for a sandboxed command to have something run outside. An
[`[[lsp]]`](configuration.md#lsp) entry replaces the detected server for
its extensions, and one whose command does not speak LSP is given up on:
`symbols` then answers from the text.

**The Claude backend.** `claude` is your own installed CLI: it reads its
login from `~/.claude`, talks to Anthropic, and is a glibc program — the
guest has no home, no such host on its network, and a musl userland (tried:
`claude` is not there, `api.anthropic.com` does not resolve). It is not the
project's content, and it is the model, not a command the model runs. What
matters is what it runs, and that is covered where atelier runs it: by
default claude's `Bash`, file and fetch tools are switched off and
atelier's replace them ([Claude backend](claude.md)), so its commands are
`bash` calls like any other — in the guest. With
`ATELIER_CLAUDE_BUILTINS=keep`, claude's own `Bash` runs inside claude, on
the host; it is then asked about like an unsandboxed command (the sandbox
does not wave it through). Background shells and subagents claude starts
itself are claude's, on the host, under its own rules.

## What the guest sees

| Guest path | What it is |
|---|---|
| `/` | the root filesystem — the fetched image with the packages you installed over it, or your directory — read-only, under a writable layer in memory |
| the project's own absolute path | the project, read-write; the working directory |
| where the project's `.git` file points | in a linked worktree or a submodule: the git directory, and only that ([below](#linked-worktrees-and-submodules)) |
| `tool-output/` and `worktrees/` of the project's [state directory](sessions.md#where-atelier-keeps-a-projects-state), at their own absolute paths | **read-only**, when they exist: the output of tools that was too long for the conversation, which results name by path, and the worktrees of the project's [isolated subagents](agents.md#worktree-isolation), which a subagent's report names in the command that brings its changes over. Nothing else of the state directory (the session, memory) |
| `/tmp`, `/dev`, `/proc`, `/sys` | the guest's own |

The project is mounted where it is on the host, so paths in a command's
output are the paths the other tools use. The directories above it exist in
the guest as empty directories. Nothing else of the host is reachable: not
your home, not `~/.ssh`, not the environment (the guest gets `PATH`, `HOME`,
`USER`, `TERM`, `LANG`, `PWD` and `safe.directory = *` for git, whose
repositories there are the project's), not the network beyond
[the hosts you listed](#the-network). It runs as root in the guest, which is
you on the host: files it creates are yours.

**Read-only inside the project**: `.git/hooks`, `.git/config` and
`atelier.toml` — what decides which programs run on the host *later* (git
hooks and `core.hooksPath`/`core.fsmonitor`/aliases; atelier's permission
rules, hooks and MCP servers) — with `.git/config.worktree` and
`.git/commondir`, two files git reads more configuration from, or the name
of another directory to take all of it from. Where `.git` is not a directory (a linked
worktree, a submodule, no repository yet) all of `.git` is. The same goes
for the same four of each submodule whose git directory is under
`.git/modules` when the command starts, and for what ties each linked
worktree of the project to it (`.git/worktrees/<name>/commondir`, `gitdir`,
`config.worktree`). They cannot be
written, replaced, removed, or carried off by renaming a directory above
them; the tests try twenty-odd routes, through links and `..` included.
What is *not* covered: other files a later host command may run — a
`Makefile`, `build.rs`, `package.json` scripts. That is the project's
content, and the agent can edit it with `write` as well. Nor is a
repository the guest makes itself somewhere in the project: a new
directory with a `.git` of its own has its own configuration and hooks,
which `git` on the host obeys if you run it there (or from above, once that
directory is recorded as a submodule).

### Linked worktrees and submodules

In a linked worktree (`git worktree add`) and in a submodule's checkout,
`.git` is a one-line file, `gitdir: <path>`, and git's data is elsewhere:
in the main repository's `.git` (the worktree's own part in
`.git/worktrees/<name>`), or in the superproject's `.git/modules/<name>`.
atelier mounts that git directory in the guest, at the path the file
spells, so git works there as it does on the host.

The `.git` file is part of the project, and a project you cloned or
unpacked may say anything in it — `gitdir: /home/you/.ssh`, `gitdir: /`. So
the file alone decides nothing. A directory is mounted only if **it names
the project back**, in a file the project could not have written:

- for a worktree, `<repository>/worktrees/<name>/gitdir` holds the path of
  this project's `.git` (git writes it at `git worktree add`), the
  directory's `commondir` leads exactly two levels up, and what is there
  has a git directory's shape (`HEAD`, `objects`, `refs`);
- for a submodule (or a `git init --separate-git-dir` checkout), the
  directory has that shape and its `config` has `core.worktree` pointing at
  this project.

Whoever can write such a file outside the project already has the
directory. A directory that contains the project, or that is inside it, is
not mounted (the first would bring a checkout's files with it); anything
else — no such directory, a repository that is someone else's, a worktree
that was moved without `git worktree repair` — is not followed, git finds
no repository in the guest as before, and `atelier sandbox status` says
why.

What is mounted is the git directory, never the checkout beside it: of the
main repository the guest sees `<main>/.git` and not `<main>`'s files; of
a superproject, `<super>/.git/modules/<name>` and nothing else of
`<super>/.git`. In it:

| | |
|---|---|
| a worktree's repository (`<main>/.git`) | **read-only**, except `objects`, `refs`, `logs/refs`, `packed-refs`, `reftable` and the worktree's own `worktrees/<name>` — what a commit writes. Its `config` and `hooks`, the main checkout's `HEAD` and index, and the other worktrees' directories can be read (git reads them, to know which branch is checked out where) and not changed. In `worktrees/<name>`, `commondir`, `gitdir` and `config.worktree` are read-only: they say which repository, and so whose hooks, the worktree uses |
| a submodule's git directory | read-write, except its `config`, `config.worktree`, `hooks` and `commondir` (and those of its own submodules) |

So in a worktree the guest can commit, branch, stash and reset, and it can
move or delete the branches of the repository — as it can in an ordinary
project — but `git config` and `git worktree add` are refused there as
read-only. The repository's `config` is readable, as an ordinary
project's is: a token written into a remote's URL is in it.

Nothing persists between commands except what was written in the project:
the next command starts in a fresh guest (`/tmp`, the environment, and
whatever the command itself installed are gone). Packages that stay are the
ones *you* install with `atelier sandbox run --rw-root`
([above](#turning-it-on)). An agent's `apk add` lasts for its command, on
purpose: the root is shared by every project, so a command that could
change it for good could leave a `git` of its own making for the next
project's commands to run — unasked, with that project's files and network.

Each command runs in a process of its own (`atelier sandbox-exec`), killed if
it outlives its timeout by three seconds, so a guest that wedges the emulator
cannot wedge the session.

## The network

None, unless you name hosts:

```toml
[sandbox]
network = ["dl-cdn.alpinelinux.org", "crates.io", "*.crates.io"]
```

Then the guest can open TCP connections to those hosts, and to nothing else.
With the list above, `apk add`, `cargo fetch` and `cargo build` of a project
with dependencies run in the sandbox, unasked.

**An entry** is one of:

| Entry | Allows |
|---|---|
| `crates.io` | that name |
| `*.crates.io` | every name under it, at any depth (`static.crates.io`, `a.b.crates.io`) — not `crates.io` itself: list both if you mean both |
| `192.168.0.50`, `2001:db8::5` | connecting to that address — the only way to a private one (see below) |

Names are compared without regard to case. There are no ports and no
protocols in an entry: an allowed host is allowed on every TCP port. A URL,
a name with a port, a bare `*`, the loopback address: not understood, allow
nothing, and `atelier sandbox status` says which.

**Where the list comes from.** Your user-global `atelier.toml` and the
project's, together. The project's entries are a [gated
setting](trust.md), like `enabled`: they count once you have accepted them
with `/trust`, which shows the hosts, and stop counting — all of them — when
the list changes, until you accept it again. A repository you just cloned
cannot open the network for itself, and neither can the agent by editing
`atelier.toml` (which the guest cannot write at all). The reason is the same
as for `enabled`: sandboxed commands are not asked about, so a host on the
list is somewhere they can send whatever they can read — the project — with
nobody looking. List what the project's tools fetch from; think twice about
hosts anyone can upload to.

**How it works, and what it holds against.** nixvm passes every connection
the guest makes to atelier, by IP address. atelier makes a list of names
exact on top of that by being the guest's DNS (`src/sandbox/net.rs`):

- The guest's `/etc/resolv.conf` names one server, `192.0.2.53` — an address
  that belongs to nobody. A question sent there is answered by atelier:
  for a name on the list, with what your machine's resolver says, and those
  addresses are remembered; for any other name, with "no such name", at
  once, and nothing is asked of anyone — so a name cannot be used to carry
  data out through your resolver either.
- A TCP connection goes through only to a remembered address (or one the
  list names). Typing an address (`curl https://1.1.1.1/`), using another
  resolver, DNS over TCP, DNS over HTTPS to a host that is not on the list:
  each is a connection to an address no allowed name resolved to, and fails
  with "Network unreachable".
- No UDP leaves the machine: not other resolvers, not QUIC (clients fall
  back to TCP), not NTP. No ICMP either: `ping` does not work.
- An allowed name that resolves to a loopback, private (`10/8`, `172.16/12`,
  `192.168/16`, `100.64/10`), link-local (`169.254/16`, where a cloud's
  metadata service lives; `fe80::/10`), unique-local (`fc00::/7`) or
  otherwise non-public address is **not** answered with it. Whoever runs an
  allowed name's DNS would otherwise decide what of *your* network the guest
  reaches (DNS rebinding). To reach a private server, list its address:
  then that address, and a name that resolves to it, work. IPv4 addresses
  wrapped in IPv6 ones (mapped, NAT64, 6to4) are judged as the IPv4 address.
- IPv6 works where your machine has a route for it; where it has none, the
  guest is not given `AAAA` answers.
- The memory lasts one guest, which is one command.

**What it does not hold against**, said plainly because it is a filter by
address in the end:

- *Hosts that share an address.* A name on a CDN resolves to addresses that
  serve thousands of other sites; a program in the guest can connect to
  that address and ask it for another site (another `Host:` header, another
  TLS server name). Allowing `dl-cdn.alpinelinux.org` allows, to someone who
  tries, whatever else Fastly serves from those addresses. Telling them
  apart would take reading the TLS handshake of every connection, which
  atelier does not do.
- *What an allowed host does with what it is sent.* A host that accepts
  uploads, or logs requests for someone to read, is a way out for data.
  `*.github.com` or a paste site on the list is the network, in effect.
- *Everything of a name is allowed*: every port, and every address the name
  resolves to while the command runs.

Also not there: record types other than `A` and `AAAA` (an allowed name has
no `TXT`, `SRV` or `HTTPS` records in the guest), names only your
`/etc/hosts` knows (the question goes to the resolver as an absolute name,
without your search domains: `crates.io.corp.example` is not what was
allowed), and listening for connections from outside.

**When a command wanted more.** A failed command's result names what was
refused, so the agent can tell you which host to add rather than guess:

```
[exit status: 1]
[stderr]
wget: bad address 'static.crates.io'
[sandbox: the sandbox's network reaches only the hosts on its allowlist, and
refused the name `static.crates.io`, which is not on the allowlist (a host
is allowed by the user, who adds its name to `[sandbox] network` in their
atelier.toml). If this command needs the host, call bash again with
"sandbox": false and say why — the user will be asked to approve it.]
```

The same is said of a connection to an address without a name, of UDP, of
an allowed name that resolves to a private address, and of an allowed name
that does not resolve. A command that succeeds says nothing, whatever it
was refused on the way. A background job and `atelier sandbox run` print
the line on stderr.

`atelier sandbox run --net` is something else: the host's whole network,
unfiltered, for preparing the root filesystem by hand.

## What works in the guest, and what doesn't

Measured with nixvm 0.0.7 (this is now built on nixvm 0.0.9, where the tests pass; the figures here and under [Speed](#speed) were not taken again) and Alpine 3.22
x86-64 on KVM.

| | |
|---|---|
| busybox, `ls`/`find`/`grep -r`, `rg` over the project | works |
| `git status`, `diff`, `log`, `checkout <file>` | works |
| `git add`, `git commit` | works. Git creates each object read-only (`0444`) and then fills it; the guest is root and may, so atelier lends the file its write bit for the write |
| a linked git worktree: `git status`, `diff`, `log`, `add`, `commit`, `branch` | works: the repository's git directory is mounted ([how, and what of it](#linked-worktrees-and-submodules)). `git config` and `git worktree add` are refused |
| a submodule's checkout as the project: the same | works, likewise |
| reading a kept tool output by the path a result gave (`cat`, `grep`) | works (nixvm 0.0.8, Alpine 3.22, KVM): it is mounted read-only, and nothing else of the state directory is there |
| an [isolated subagent](agents.md#worktree-isolation): its guest has the worktree as the project, and commits there; in the parent's guest, the report's `git -C <worktree> diff --binary <base> \| git apply` | works (same setup). The parent's guest cannot change the worktree — the mount is read-only — so `git worktree remove` there is refused: `/agents clean` does it, or `"sandbox": false` |
| a project with submodules: `git status`, `git submodule status`, `git add <submodule>`, `git -C <submodule> commit` | works (their git directories are in the project's `.git/modules`). `git -C <submodule> config` is refused |
| `python3`, `node` (JIT included), subprocesses from both | works |
| `gcc`, `make`, `make -j` | works |
| `cargo build`, `cargo run`, `cargo test` | works, with the toolchain of the image (Alpine 3.22 has Rust 1.87); crates are fetched if the registry's hosts are on the [network list](#the-network), and must be there already otherwise |
| the host's own toolchains (`~/.cargo`, `~/.rustup`, nvm, system compilers) | not there: they are outside the project, and glibc programs do not run on a musl root anyway |
| `apk add`, `pip install`, `npm install`, `cargo fetch` | with their hosts on the [network list](#the-network); no network otherwise. Tried: `apk add jq` with `dl-cdn.alpinelinux.org` allowed (2 s); `apk add cargo git`, then `cargo run` of a crate with a crates.io dependency, with `crates.io` and `*.crates.io` (38 s in all). What they install is gone with the guest, except what lands in the project (`node_modules`, `target`, a virtualenv) |
| `wget`, `curl`, `git clone` over HTTPS to an allowed host | works (tried: a clone from `github.com`) |
| a host that is not on the list, an address typed in, another DNS server, `ping`, QUIC | refused: "bad address" or "Network unreachable", at once, and the result says what to ask for |
| FIFOs, sockets and device files in the project | refused (`mkfifo`: Operation not permitted): the host would open them, and block. They work in the guest's `/tmp`. Reading a FIFO that is already in the project ends the command ("every process is blocked") |

The story for a project's own toolchain, today: it has to be in the image
(`apk add`), in a version Alpine packages; its dependencies are fetched in
the guest if you allow the registry's hosts, and have to be on disk already
if you don't. A project whose toolchain fits that builds, tests and commits
in the sandbox; one that needs the host's `rustup` toolchain still goes to
the host for those commands, and they still ask.

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

The feature adds 5.0 MB to the x86-64 release binary (25.2 → 30.2 MB,
measured on the fullrust build with nixvm at 7761ff8, the revision that became 0.0.9). A build without it
is unchanged: the code that touches nixvm is compiled only with the
feature, on Unix and on fullrust.

What it links: nixvm, and through nixvm's `images` feature — the registry
client and the squashfs reader the fetched root needs — `fstool` (with
`uuid` and `getrandom` 0.4 behind it), `compcol` 0.7 and `intl` 0.6, with
`rsurl` and `purecrypto` 0.10, which atelier links anyway. All Rust, none
needing a libc. nixvm asks for `rsurl` 0.1.22, which is also atelier's own
HTTP client (it was 0.1.12).

**fullrust.** The x86-64 release binary is built for
`x86_64-unknown-linux-fullrust`, which has no libc. Since nixvm makes
Linux's system calls itself there (host directories, KVM, sockets), the
sandbox is in that binary: built with the fullrust 1.99 toolchain, it ran
guests on KVM and passes the sandbox's tests, the live ones included. The
arm64 release binary (musl) is built without the feature: it compiles
there, but nixvm has no hardware virtualization on arm64 Linux yet and it
was not run.

## What is not done

- **The network list is by address in the end** ([the
  network](#the-network)): two names on one address cannot be told apart,
  an entry has no port, and names have only addresses (no `SRV`, `TXT` or
  `HTTPS` records). Telling names apart needs the name at connection time —
  the TLS server name, or nixvm passing along what the guest resolved (see
  below).
- **`[sandbox] root` from a project file** is ignored on purpose: the root is
  what the guest can read, and a cloned repository naming `/` would hand it
  the machine.
- **Typing to a command in the guest.** On the host a foreground command
  runs on a terminal you can be handed
  ([A command you can type to](tools.md#a-command-you-can-type-to)); in the
  guest a `bash` command has no terminal and an empty stdin. A command that stops at a prompt there is reported to the
  model as having asked for input, and `interactive: true` as not honoured;
  `"sandbox": false` runs it on the host, with approval.
- **MCP servers, language servers and the Claude backend run on the host**,
  as do your own hooks ([what runs where](#what-runs-where)). Of these,
  language servers and an MCP server that lives in the project's tree run
  the project's code there. Running them in a guest needs a guest that
  outlives one command.
- **The `node` tool's `fs` is not held to the guest's read-only paths**
  (`.git/hooks`, `.git/config`, `atelier.toml`): it writes them on the
  host, unasked, sandbox or not.
- **A hook that must run on the host cannot be a project hook** while the
  sandbox is on: there is no per-hook switch, only your own file.
- **Packages an agent installs do not stay**, and there is one layer of
  yours for all projects, not one per project
  ([what the guest sees](#what-the-guest-sees)). A guest started while
  `--rw-root` is installing reads a layer that is changing under it.
- **The fetched image is Alpine for this machine's architecture, from
  Docker Hub**: no mirror is tried when Docker Hub refuses (its anonymous
  rate limit), no proxy setting, no other distribution. A root of your own
  is the way round each.
- **The layer does not record ownership**: everything in it is root's to
  the guest, including files a package meant for a service account.

## What nixvm needs next

In the order that unblocks atelier, with what atelier does once each lands.

1. *(Done in nixvm 0.0.9, nixvm #13: the release binary has the sandbox.)* **A host-directory filesystem without libc, and KVM on fullrust.**
   `fs::Passthrough` on raw system calls (or on `std::os::fd` where std
   suffices) and gated on something fullrust satisfies; the same for
   `vcpu::kvm`'s `target_os = "linux"` and `kernel::egress`'s `setsockopt`.
   *Then:* the `sandbox` feature can be in the release binaries.
2. *(Worked around in atelier, which lends the write bit for the write; still worth doing in nixvm, where an unlinked file cannot be helped.)* **Writing through a descriptor to a file the path no longer allows**
   (created `0444`, or since made read-only, or unlinked): the host-directory
   filesystem should keep the host file open for the life of the guest's
   descriptor instead of reopening by path. *Then:* `git add`/`git commit`
   work, and the commit flow stays in the sandbox.
3. *(Done in nixvm 0.0.6: `cargo` builds and tests.)* **`socketpair(AF_UNIX, SOCK_SEQPACKET)`** (and whatever is behind it once
   that answers): *then* `cargo` and `rustc` can start their children, and
   an image with a Rust toolchain can build.
4. *(Worked around: atelier refuses special files in the project.)* **Never block the kernel thread on the host**: opening or removing a FIFO
   in a host directory hangs the VM with its wall-clock limit unenforced
   (`mkfifo f; rm f`). Either refuse special files in a host directory or
   open them non-blocking; and enforce `Limits::wall_time` from outside the
   thread that may be stuck. *Then:* atelier could drop the kill-the-helper
   fallback (it will keep the helper process for the reasons below).
5. *(Done in nixvm 0.0.9, nixvm #14: atelier's guest is `Sandbox::execute`. Left over: the builder takes a root that is a directory, a squashfs or an image name, not a filesystem of the embedder's — atelier mounts its own at `/` over the empty one, which works because the later of two mounts at one path wins.)* **An embedding API for "run this command"**, so atelier stops assembling
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
6. *(Done in nixvm 0.0.9, nixvm #15: atelier's path filter is gone; `Passthrough::protect` keeps them.)* **Read-only paths inside a writable host directory**, including single
   files, that survive a rename of a parent. Mounts are matched by path, so
   a read-only mount at `.git/hooks` is defeated by `mv .git g` (tested);
   atelier wraps `Passthrough` in a filter of its own instead. A
   `Passthrough::protect(paths)` would do.
7. *(Done in atelier, as far as addresses go: it answers the guest's DNS
   through `Egress::open_udp` and lets `connect_tcp` through to the
   addresses it gave out.)* **Egress by name.** What the `Egress` trait of
   0.0.7 does not give, and what each would buy:
   - `connect_tcp(ip, v6, port)` knows no name. If the kernel kept, per
     guest, which answer of the embedder's resolver an address came from and
     passed that name along — or let the embedder peek at the first bytes a
     connection sends before it is opened — a list could tell two names on
     one CDN address apart (by the TLS server name) and carry ports per
     name. Today whatever shares an address with an allowed name is
     reachable.
   - The resolver is reached by pretending to be a UDP peer: atelier writes
     a `resolv.conf` naming an address nobody has and answers datagrams
     sent to it. A resolver hook proper (`Kernel::set_resolver`, called
     with the question) would spare the wire format and the fake address.
   - `Egress::connect_tcp` has no way to say *why* it refused: every error
     but a timeout and "unreachable" becomes `ECONNREFUSED`. atelier
     answers "Network unreachable" and reports the refusal out of band.

   *Then:* an allowlist exact by name rather than by address.
8. *(Done in nixvm 0.0.9, nixvm #16: `image::pull` and `Overlay::persistent`. Left over: a persistent layer in a directory of a user who is not root cannot record ownership — atelier papers over it, see [turning it on](#turning-it-on) — and the layer has one writer and no way to be read safely while written.)* **A released root image and a fetch that needs no C**: today the image is
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
