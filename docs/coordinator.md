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

The agent has two tools for this (see [tools](tools.md#peers-peer_send)):
`peers` lists the instances and manages its topic subscriptions, and
`peer_send` sends a message. `peer_send` asks for your approval like any
tool that acts outside the project; answer "always" to let the agent talk
to its peers freely.

From a shell:

```sh
atelier coordinator status   # is one running, and who is connected
atelier coordinator stop     # end it (instances will start another)
```

## Settings

| Variable | |
|---|---|
| `ATELIER_NO_COORDINATOR` | Any non-empty value other than `0`: this atelier runs alone — it neither joins nor starts a coordinator, and has no `peers`/`peer_send` tools |
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
disconnects or takes longer than `timeout_ms` (30 s by default). Instances
answer `ping` and `info` today — `call` is the extension point for richer
instance-to-instance requests (`Client::handle` in `src/coord/client.rs`).

The code is in `src/coord/`: `daemon.rs` (the coordinator), `client.rs`
(an instance's connection, reconnection and launch of the coordinator),
`tools.rs` (the agent's tools).
