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
  names, with the same topic subscriptions. This takes a fraction of a
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
start working because a message arrived** — it waits for you.

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
worth. An idle instance does not answer by itself: the question waits for
its user's next prompt, like any message.

From a shell:

```sh
atelier coordinator status   # is one running, and who is connected
atelier coordinator stop     # end it (instances will start another)
```

## Settings

| Variable | |
|---|---|
| `ATELIER_NO_COORDINATOR` | Any non-empty value other than `0`: this atelier runs alone — it neither joins nor starts a coordinator, and has none of the `peer*` tools |
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
directory, model, protocol revision, topics); the coordinator answers with
the name it gets. After that:

| Request | Does |
|---|---|
| `list` | The connected instances |
| `update {status, model}` | Change what `list` says about the caller |
| `send {to, body}` | Deliver `body` to the instance named `to` (or with that id), or to all others with `to: "*"` |
| `subscribe {topic}`, `unsubscribe {topic}` | Topic membership |
| `publish {topic, body}` | Deliver `body` to the topic's subscribers |
| `call {to, method, params, timeout_ms}` | Ask another instance to run `method` and return its answer |
| `status`, `shutdown` | About the coordinator itself; allowed without `hello` |

Deliveries arrive as a `message` notification (`from`, `body`, and `topic`
or `broadcast`). A `call` arrives at its target as a `call` request, whose
answer the coordinator passes back to the caller; it fails if the target
disconnects (error `-32004`) or takes longer than `timeout_ms` (error
`-32003`; 30 s by default, 30 minutes at most). Instances answer `ping`,
`info` and `ask` — `call` is the extension point for richer
instance-to-instance requests (`Client::handle` in `src/coord/client.rs`).

The answer to a `call` need not be ready when its handler returns. Handlers
run on the thread that reads the connection and must not hold it up, so one
registered with `Client::handle_deferred` is given a `Responder` it may keep
and answer through later, from any thread; the coordinator simply holds the
call open until the answer or the deadline. A responder is bound to the
connection the call arrived on (a new coordinator numbers its calls afresh),
and one dropped without an answer fails the call at once.

`ask` is built on that: `call {to, method: "ask", params: {question,
timeout_ms}}`. The target's handler files the question under an id (`q1`,
`q2`, …) with its responder and puts it in the inbox; `peer_answer` looks
the id up and answers `{answered: true, answer}`. A question closed without
an answer is answered `{answered: false, reason}`. The target forgets a
question after the same `timeout_ms`, so an agent is never asked to answer
one nobody waits for (`src/coord/ask.rs`).

The code is in `src/coord/`: `daemon.rs` (the coordinator), `client.rs`
(an instance's connection, reconnection and launch of the coordinator),
`ask.rs` (questions awaiting this instance's agent), `tools.rs` (the
agent's tools).
