# The coordinator

Every atelier running on your machine is connected to the others through
a small background process, the **coordinator**. It lets the instances see
each other and talk — you to an agent in another terminal, or the agents
among themselves.

It is also what knows **which atelier owns the session of a project
directory**, so that a second one started there is told instead of writing
over the first (see [One session, one
owner](sessions.md#one-session-one-owner)).

It also runs the **server of the [model on this machine](local.md)**, for
the ateliers that use one (`atelier --local`): one server for all of them,
started when the first asks and stopped when the last is gone.

There is nothing to install or start. The first atelier you launch starts
the coordinator; the others find it. What it holds in memory each atelier
can tell it again, and the one thing it keeps on disk is where the model
server is, so it can end at any time and be replaced:

- **It ended** (crashed, was killed): every instance notices, the first one
  to retry starts a new coordinator, and they all rejoin it under the same
  names, with the same topic subscriptions and locks, each owner claiming
  its session again. This takes a fraction of a second and needs nothing
  from you. An atelier that was taking a session over when it happened —
  or that starts with `--take-over` just then — takes its place behind the
  owner again at the new coordinator, and finishes.
- **atelier was updated**: the coordinator ends by itself when its
  executable is replaced on disk (a self-update, a rebuild), and when a
  newer atelier connects to it. The next one runs the new version.
- **Nobody is left**: it ends a minute after the last instance exits and
  the last session is given up — once it has stopped the model server,
  which it does a minute after the last atelier using it is gone.

The model server is not ended with the coordinator: it stays, with the
models it has loaded, and the next coordinator finds it again
([below](#the-local-models-server)).

While the coordinator is away, `/peers`, `/send` and the agent's tools fail
with a message saying so, and an atelier started in a directory where
another is running is not told ([Without the
coordinator](sessions.md#without-the-coordinator) says what happens in
each case). Nothing else in atelier depends on it.

## Using it

| Command | |
|---|---|
| `/peers` | The connection's state and the instances connected: name, idle or working, model, directory, topics |
| `/send <name> <message>` | Message one instance |
| `/send * <message>` | Message every other instance |
| `/send #<topic> <message>` | Message the instances subscribed to a topic |
| `/subscribe <topic>`, `/unsubscribe <topic>` | Start or stop receiving a topic |

An instance is named after its project directory (`atelier`, and
`atelier-2` for a second one in a directory of the same name). A
[subagent that runs as a process](agents.md#as-a-process) is an instance
too, named after its parent and its agent (`atelier.explore`) and listed as
`subagent of atelier`; it is there for as long as its task takes.

One instance is one session. A second terminal attached to a session as its
[remote control](sessions.md#remote-control) is not an instance — it is
another way into the one there is — and an atelier that
[takes a session over](sessions.md#take-over) joins under the name the old
owner had, once that one is gone with the locks it held.

A message that arrives is shown in the receiving instance (`✉ name: …`)
and handed to its agent: at the agent's next step if it is working, or
together with your next prompt if it is idle. **An idle instance does not
start working because a message arrived** — it waits for you, unless you
have told it to [wake](#waking-an-idle-instance).

The agent sees a peer's message marked as coming from another agent, not
from you, and is told to treat it as information.

The agent has tools for this (see [tools](tools.md#peers-peer_send)):
`peers` lists the instances and manages its topic subscriptions, and
`peer_send` sends a message. `peer_send` asks for your approval like any
tool that acts outside the project; answer "always" to let the agent talk
to its peers freely.

## Asking another agent

A message is left and forgotten; a **question** is waited for. With the
`peer_ask` tool an agent puts a question to another instance and gets that
instance's agent's answer back as the tool's result:

1. The asking agent calls `peer_ask` (you approve it, like `peer_send`). Its
   turn waits there — up to 5 minutes unless it asks for another timeout, 30
   minutes at most.
2. The question arrives in the other instance like a message (`✉ name asks
   (q1): …`), and reaches its agent the same way: at its next step if it is
   working, with your next prompt if it is idle. The agent sees it marked as
   a question from another agent, with the id to answer under.
3. That agent answers with `peer_answer` (no approval: it only completes
   an exchange you already allowed on the asking side), and the asking
   agent's `peer_ask` returns the answer.

When there is no answer, `peer_ask` still returns — a result that says so
(`no answer: …`), not a failure:

| What happened | The asker is told |
|---|---|
| The turn the question was delivered to ended without a `peer_answer` | `<name> ended its turn without answering` |
| Nobody answered before the timeout — typically the other instance is idle and nobody prompted it | `<name> did not answer within <n>s …` |
| The other instance exited | `<name> disconnected before answering` |
| The other agent is itself waiting on a `peer_ask` to the asker | `<name> is itself waiting for an answer from <asker> …` |

A question that timed out before you prompted the idle instance is still
passed on to its agent with that prompt, as an ordinary message: nobody is
waiting any more, but what was asked may still matter.

This is also how one agent hands a piece of work to an instance you opened
yourself: the question is the task, the answer the report. The other
instance does it under its own mode and rules, and asks *its* user what
they would be asked. (A [subagent](agents.md) is for work the asking agent
wants done under its own.)

The asking turn cannot be interrupted while it waits (no tool call can), so
an agent should give `timeout_secs` when a short wait is all the question is
worth. An idle instance does not answer by itself — the question waits for
its user's next prompt, like any message — unless it is set to
[wake](#waking-an-idle-instance) for questions.

## Saying what you are working on

Two agents in one checkout will sooner or later edit the same file. With
the `peer_lock` tool an agent announces the path it is working on — a file,
or a directory and everything under it — with a note:

```
peer_lock {action: "acquire", path: "src/api", note: "renaming the handlers"}
```

- Another agent that tries to acquire `src/api`, something inside it
  (`src/api/mod.rs`) or something around it (`src`) is not granted the
  lock: it is told who holds what, for how long, and the note, and decides
  for itself — wait, `peer_ask` the holder, or work elsewhere.
- Another agent that **edits** a path under the lock (`write`, `edit`,
  `multiedit`, `apply_patch`, `notebook_edit`) still makes the edit, and
  finds one more line in the tool's result:
  `note: api says it is working on /home/you/proj/src/api: renaming the handlers`.
- `peers` (the tool and the `/peers` command) lists the locks under the
  instances; `peer_lock {action: "list"}` lists only them.
- `peer_lock {action: "release", path: …}` gives a path back; without a
  path, all of the instance's.

Locks are **advisory**. Nothing is refused or delayed because of one, and
atelier never takes one by itself: an agent locks a path because it (or
you, in your instructions to it) judged the work worth announcing. They are
not kept anywhere either. A lock ends when its holder releases it or exits;
if the coordinator restarts, each instance announces what it holds to the
new one, so the table comes back as it was.

Subagents (the `task` tool) edit without the note: only the instance's own
conversation is checked against the table. The exception is a subagent that
runs [as a process](agents.md#as-a-process): it is an instance, so its edits
are checked and its locks are its own.

## Waking an idle instance

By default an instance at its prompt does nothing until you type. You can
let your other instances set it to work:

```toml
[coordinator]
wake = "questions"   # never (default) | questions | always
wake_max = 5         # self-started turns before you must type again (default 5)
```

or `ATELIER_COORDINATOR_WAKE=questions` for one run. With `questions`, a
`peer_ask` that arrives while the instance is idle starts a turn in it, with
the question as the prompt, so the asking agent gets its answer without you;
ordinary messages still wait. With `always`, any message from a peer starts
a turn.

The turn is an ordinary one. It is announced in the transcript (`⏰ woken by
<peer> (2 of 5 without you): …`), and it runs under the session's mode and
permission rules: **nothing is approved because the instance woke**. A woken
turn that wants to run something you have not allowed stops at the approval
prompt and waits for you, like any turn — set the instance's
[mode](permissions.md#modes) and rules for what you want done unattended
(`plan` for an instance that should only answer questions about the code).

What keeps this bounded:

- **Only your own instances can wake one.** The coordinator's socket is in
  a directory only your user can enter, so every message comes from a
  process of yours. Nothing arriving from the network or from another user
  can start a turn.
- **A cap.** An instance starts at most `wake_max` turns by itself. Typing
  anything in it — a prompt, a command — resets the count; until then,
  further messages are shown and wait for your prompt as usual.
- **Never under your fingers.** While there is text on the input line, the
  wake waits; it starts when the line is empty again. (The line REPL — what
  atelier is when its input is not a terminal — cannot see a half-typed
  line, and does not wait.)
- **No ping-pong.** Every message carries a `wake_depth`: 0 when it is sent
  from a turn somebody typed the prompt of, and one more than the message
  that woke it when it is sent from a woken turn. An instance that has
  already woken since you last typed in it does not wake for a message of
  depth 2 or more. So if you prompt A, A may wake B, and B's reply may wake
  A once — but what A's woken turn sends back wakes nobody. Two instances
  set to `always` cannot keep each other busy.
- **Decided on arrival.** Whether a message wakes is settled when it
  arrives. One that did not (the cap was reached, the chain was too deep)
  does not start a turn later; it leads your next prompt.

`atelier --print` never wakes: it runs its prompt and exits.

A wake and a [`/loop`](loops.md) share the same mechanism — the idle prompt
asks the session, a few times a second, whether it has a turn of its own to
start — so they never overlap: one turn at a time, a wake before a loop.

From a shell:

```sh
atelier coordinator status   # is one running, who is connected, which sessions are owned
atelier coordinator stop     # end it (instances will start another)
```

## Settings

| Variable | |
|---|---|
| `ATELIER_NO_COORDINATOR` | Any non-empty value other than `0`: this atelier runs alone — it neither joins nor starts a coordinator, has none of the `peer*` tools, and neither claims the session of its directory nor finds out that another atelier has it: it starts next to one [as if it were not there](sessions.md#without-the-coordinator) |
| `ATELIER_COORDINATOR_WAKE` | `never`, `questions` or `always`: what [wakes](#waking-an-idle-instance) this instance when idle, overriding `[coordinator] wake`. Any other value means `never` |
| `ATELIER_COORDINATOR_DIR` | Directory for the coordinator's socket, lock and log, and for the sockets of session owners. Default: `$XDG_RUNTIME_DIR/atelier`, else `~/.local/state/atelier`. Instances only meet — and a session only has one owner — among ateliers that use the same directory |

The coordinator is available on Unix systems. Its log is
`coordinator.log` in that directory.

## How it works

The coordinator is the same executable, run as `atelier coordinator`,
detached from the terminal that happened to start it. It listens on a Unix
socket (`coordinator.sock`) in a directory only you can enter, so only your
own processes reach it. A lock on `coordinator.lock`, held for as long as
it runs and released by the kernel however it dies, guarantees a single
coordinator no matter how many instances try to start one at once.

Instances and coordinator exchange JSON-RPC 2.0, one message per line.
An instance opens with `hello` (its id, the name it would like, pid,
directory, model, protocol revision, topics, the locks it holds); the
coordinator answers with the name it gets. After that:

| Request | Does |
|---|---|
| `list` | The connected instances |
| `update {status, model}` | Change what `list` says about the caller |
| `send {to, body, wake_depth?}` | Deliver `body` to the instance named `to` (or with that id), or to all others with `to: "*"` |
| `subscribe {topic}`, `unsubscribe {topic}` | Topic membership |
| `publish {topic, body, wake_depth?}` | Deliver `body` to the topic's subscribers |
| `call {to, method, params, timeout_ms}` | Ask another instance to run `method` and return its answer |
| `lock {path, note}` | Announce work on `path` (absolute). `{acquired: true, since}`, or `{acquired: false, held_by: […]}` when another instance holds the path, one containing it, or one inside it |
| `unlock {path?}` | Release the caller's lock on `path`, or all its locks. `{released: n}` |
| `locks` | The lock table: `{locks: [{holder, holder_id, path, note, since}]}` |
| `status`, `shutdown` | About the coordinator itself; allowed without `hello` |
| `claim {dir, record, version, proto, wait?, again?}` | Claim the session of the project directory `dir` for this connection (see [below](#session-claims)); allowed without `hello` |
| `unclaim {dir?}` | Give up the caller's claim on `dir`, or its place behind one — all of them without `dir`. `{released: n}` |
| `claims {dir?}` | The sessions that are owned: `{claims: [{dir, owner, heir}]}`; allowed without `hello` |
| `local {bin, update, version, proto, pid, start?, lease?}` | The local model's server, for this connection to use from now on (see [below](#the-local-models-server)); allowed without `hello` |
| `local-status`, `local-stop` | Where the server is (with its key), and end it now; allowed without `hello` |

Deliveries arrive as a `message` notification (`from`, `body`, `topic` or
`broadcast`, and the sender's `wake_depth`, which the coordinator passes on
untouched — see [waking](#waking-an-idle-instance)). A `call` arrives at its target as a `call` request, whose
answer the coordinator passes back to the caller; it fails if the target
disconnects (error `-32004`) or takes longer than `timeout_ms` (error
`-32003`; 30 s by default, 30 minutes at most). Instances answer `ping`,
`info`, `ask` and `task` — `call` is the extension point for richer
instance-to-instance requests (`Client::handle` in `src/coord/client.rs`).

The lock table is one of the two things the coordinator keeps, and it
keeps it in memory. Whenever it changes — an acquire, a release, a holder
disconnecting — every instance is sent the whole table in a `locks`
notification and keeps that copy, so looking a path up (which every edit
does) costs no request. An instance's `hello` lists the locks it holds
(`locks: [{path, note, since}]`), which is how the table survives a
coordinator restart, and the answer to `hello` carries the table as it then
stands (`src/coord/locks.rs`).

### Session claims

The other table is who owns the session of each project directory
(`src/coord/claims.rs`; what is built on it is in
[sessions](sessions.md#one-session-one-owner)). Unlike the locks it is not
advice: the coordinator is what guarantees that a directory's session has
one owner.

A run that works on a project's saved session opens a connection for it —
separate from its instance's, without `hello`: it has no name, is not
listed and receives no message — and sends `claim` with the project's real
path as `dir` (`atelier acp` does so for every session an editor opens, each
from the process that runs it: see [acp](acp.md#sessions-and-directories)),
what it says of itself as `record` (kept as given), and its
`version` and `proto`, checked as `hello`'s are. The answer is one of:

| Answer | |
|---|---|
| `{claimed: true}` | The session is this connection's, until the connection ends. Claiming it again replaces the record |
| `{claimed: false, owner, heir, waiting}` | It is somebody else's: `owner` is that one's record. `heir` is the record of the connection waiting behind it, if any, and `waiting` whether that is the caller |
| `{claimed: false, settling_ms}` | Nobody holds it, but this coordinator has just replaced another and the owner it may have had is given `settling_ms` more to come back. Ask again |

Requests are handled one at a time, so two claims on a free directory
cannot both succeed. A claim ends when its connection does, and only then;
nothing is stored, so nothing is ever stale.

`wait: true` is a **take-over**: when the directory is somebody's and
nobody waits behind it yet, the caller takes that place (`waiting: true`).
When the owner's connection ends, the claim becomes the waiting
connection's — under the same lock, with no moment in which the directory
is free — and it is sent a `claimed {dir}` notification. Closing the
connection, or `unclaim`, gives the place up. A coordinator that is ending
hands nothing over: the connections it closes are of owners still running.

`again: true` is an owner **claiming again** after the coordinator was
replaced. A coordinator that finds its predecessor's socket still there —
one that was killed, or that ended with sessions claimed, which leaves the
socket for this — refuses, for its first second, to give an unclaimed
directory to anyone who does not say `again`, so that a newcomer cannot get
in before the owner is back. If two owners claim the same directory again
(they could not see each other when they started), the first has it and the
second is answered `claimed: false` like anybody else; it is for that
atelier to say so to its user, and it keeps running.

**What a claim costs a start.** It is made before the session is opened,
so a start waits for it. Measured on 2026-11-22 (x86_64 Linux, release
build, median of 25 `--print` runs each against a local mock model; the
ignored test `the_claim_adds_little_to_a_start` in `tests/own.rs` runs it
again):

| Start | Median |
|---|---|
| no coordinator at all (`ATELIER_NO_COORDINATOR=1`) | 10.1 ms |
| coordinator running, a run that claims nothing (`--print`) | 10.1 ms |
| coordinator running, a run that claims (`--print --continue`) | 10.1 ms |
| no coordinator running yet, a run that claims — and so starts one | 41.6 ms |

With a coordinator up — every start but the first on the machine, or the
first after it went idle — the claim is one request on a local socket and
its cost is below what these runs can tell apart (well under a
millisecond). The first start pays about 30 ms for launching the
coordinator and waiting for its socket; an instance that does not claim
launches it too, but without waiting. Neither is worth making faster.
The exception is a start in the second after a coordinator was killed: a
directory nobody has claimed again yet is kept from it for that long (see
`again` above).

The answer to a `call` need not be ready when its handler returns. Handlers
run on the thread that reads the connection and must not hold it up, so one
registered with `Client::handle_deferred` is given a `Responder` it may keep
and answer through later, from any thread; the coordinator simply holds the
call open until the answer or the deadline. A responder is bound to the
connection the call arrived on (a new coordinator numbers its calls afresh),
and one dropped without an answer fails the call at once.

`ask` is built on that: `call {to, method: "ask", params: {question,
timeout_ms, wake_depth}}`. The target's handler files the question under an id (`q1`,
`q2`, …) with its responder and puts it in the inbox; `peer_answer` looks
the id up and answers `{answered: true, answer}`. A question closed without
an answer is answered `{answered: false, reason}`. The target forgets a
question after the same `timeout_ms`, so an agent is never asked to answer
one nobody waits for (`src/coord/ask.rs`).

`task` is what a [subagent that runs as a process](agents.md#as-a-process)
says to the instance that started it: `call {to: <parent's id>, method:
"task", params: {task, op, …}}`, where `task` is an id only the two know and
`op` is `take` (the child is up; the answer is its task), `say` (a line of
progress), `change` (a file it wrote), `approve` (a tool call it wants to
make; the answer is `{refusal}` — none, or the refused call's result) or
`report` (how it ended). The child always asks and the parent answers, so
no call outlasts a step, and a coordinator that restarts costs a retry. An
instance says whose subagent it is in `hello` (`parent`), which `list`
passes on (`src/agent/subagent/process.rs`).

The code is in `src/coord/`: `daemon.rs` (the coordinator), `client.rs`
(an instance's connection, reconnection and launch of the coordinator),
`ask.rs` (questions awaiting this instance's agent), `locks.rs` (the
advisory locks), `claims.rs` (the session claims), `tools.rs` (the agent's
tools).

### The local model's server

The coordinator runs [oxinfer](local.md) for every atelier of the user
(`src/coord/infer.rs`). An atelier that uses the local model opens a
connection for it — separate from its instance's, without `hello`, as for a
session claim — and sends `local` with the oxinfer to start (`bin`, an
absolute path), whether that program may be updated first (`update`), and
its `version` and `proto`, checked as `hello`'s are. The answer is one of:

| Answer | |
|---|---|
| `{state: "up", port, key, pid, version, bin, leases}` | The server: `http://127.0.0.1:<port>`, and the key it wants |
| `{state: "starting"}` | It is being started (or looked for, see below). Ask again in a moment |
| `{state: "down"}` | There is none, and none was to be started (`start: false`) |
| an error | The start failed — the message says why — or the caller's protocol is not this coordinator's |

- **Starting.** With no server running, the coordinator has oxinfer update
  itself if it is due or too old ([how](local.md#keeping-oxinfer-up-to-date)),
  then starts `oxinfer serve --listen 127.0.0.1:<port> --max-body-mb 64
  --no-update-check` on a free port, with a key it makes in the server's
  environment (`OXINFER_API_KEY`), detached, its output in `oxinfer.log`.
  No model is named: each atelier has the server load its own through
  oxinfer's management API. The request does not wait for this — it is
  answered `starting` — so the coordinator goes on serving meanwhile. A
  start that failed is told to those that ask for the next three seconds;
  after that the next request tries again.
- **A lease is its connection's.** The request makes the connection a user
  of the server (unless `lease: false`), and it stops being one when it
  ends, however the process went. Nothing is written for it. `status`
  counts the leases, and `atelier coordinator status` prints them: *local
  model server: oxinfer 0.0.16 (pid …) on 127.0.0.1:…, used by 2 ateliers*.
- **Stopping.** The server is stopped (`SIGTERM`, then `SIGKILL` after five
  seconds) once no lease has been held for a minute (`ATELIER_LOCAL_IDLE`
  seconds), by `local-stop`, and by a coordinator that ends while no lease
  is held. The coordinator does not leave for idleness while the server
  runs or a lease is held.
- **Adoption.** The server outlives the coordinator. What it takes to find
  it again — port, key, pid, and when that process started — is in
  `oxinfer-server.json` beside the socket (mode 0600), written when the
  server is spawned. A coordinator that starts and finds the file looks for
  the server on a thread of its own: the process must be there, started
  when the file says (a pid alone could be another process by now), and
  answer to its key, for which it gets ten seconds — the coordinator before
  may have only just started it. Then it is this coordinator's, models and
  all; otherwise the file is removed, and a process that is the server but
  never answered is ended. Meanwhile `local` answers `starting`. The
  holders make their leases again as they reconnect (`start: false`: a
  lease only), and the idle minute counts from the adoption, which is the
  time they have to. A server whose users all died with the coordinator is
  thus stopped a minute after the next coordinator starts — whichever
  atelier starts it.

The key reaches whoever can connect to the socket: the user's own
processes. `status`, which is what `atelier coordinator status` and
`atelier doctor` ask, does not carry it.
