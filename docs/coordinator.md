# The coordinator

Every atelier running on your machine is connected to the others through
a small background process, the **coordinator**. It lets the instances see
each other and talk — you to an agent in another terminal, or the agents
among themselves.

There is nothing to install or start. The first atelier you launch starts
the coordinator; the others find it. It holds no state worth keeping, so it
can end at any time and be replaced:

- **It ended** (crashed, was killed): every instance notices, the first one
  to retry starts a new coordinator, and they all rejoin it under the same
  names, with the same topic subscriptions and locks. This takes a fraction of a
  second and needs nothing from you.
- **atelier was updated**: the coordinator ends by itself when its
  executable is replaced on disk (a self-update, a rebuild), and when a
  newer atelier connects to it. The next one runs the new version.
- **Nobody is left**: it ends a minute after the last instance exits.

While the coordinator is away, `/peers`, `/send` and the agent's tools fail
with a message saying so; nothing else in atelier depends on it.

## Using it

| Command | |
|---|---|
| `/peers` | The connection's state and the instances connected: name, idle or working, model, directory, topics |
| `/send <name> <message>` | Message one instance |
| `/send * <message>` | Message every other instance |
| `/send #<topic> <message>` | Message the instances subscribed to a topic |
| `/subscribe <topic>`, `/unsubscribe <topic>` | Start or stop receiving a topic |

An instance is named after its project directory (`atelier`, and
`atelier-2` for a second one in a directory of the same name).

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
conversation is checked against the table.

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
atelier coordinator status   # is one running, and who is connected
atelier coordinator stop     # end it (instances will start another)
```

## Settings

| Variable | |
|---|---|
| `ATELIER_NO_COORDINATOR` | Any non-empty value other than `0`: this atelier runs alone — it neither joins nor starts a coordinator, and has none of the `peer*` tools |
| `ATELIER_COORDINATOR_WAKE` | `never`, `questions` or `always`: what [wakes](#waking-an-idle-instance) this instance when idle, overriding `[coordinator] wake`. Any other value means `never` |
| `ATELIER_COORDINATOR_DIR` | Directory for the coordinator's socket, lock and log. Default: `$XDG_RUNTIME_DIR/atelier`, else `~/.local/state/atelier`. Instances only meet if they use the same directory |

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

Deliveries arrive as a `message` notification (`from`, `body`, `topic` or
`broadcast`, and the sender's `wake_depth`, which the coordinator passes on
untouched — see [waking](#waking-an-idle-instance)). A `call` arrives at its target as a `call` request, whose
answer the coordinator passes back to the caller; it fails if the target
disconnects (error `-32004`) or takes longer than `timeout_ms` (error
`-32003`; 30 s by default, 30 minutes at most). Instances answer `ping`,
`info` and `ask` — `call` is the extension point for richer
instance-to-instance requests (`Client::handle` in `src/coord/client.rs`).

The lock table is the one thing the coordinator keeps, and it keeps it in
memory. Whenever it changes — an acquire, a release, a holder
disconnecting — every instance is sent the whole table in a `locks`
notification and keeps that copy, so looking a path up (which every edit
does) costs no request. An instance's `hello` lists the locks it holds
(`locks: [{path, note, since}]`), which is how the table survives a
coordinator restart, and the answer to `hello` carries the table as it then
stands (`src/coord/locks.rs`).

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

The code is in `src/coord/`: `daemon.rs` (the coordinator), `client.rs`
(an instance's connection, reconnection and launch of the coordinator),
`ask.rs` (questions awaiting this instance's agent), `locks.rs` (the
advisory locks), `tools.rs` (the agent's tools).
