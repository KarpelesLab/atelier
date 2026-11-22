# A model on this machine

`atelier --local` runs the model on your own computer: no endpoint to
configure, no API key, no subscription. It is what to use when you have none
of those, or want nothing to leave the machine.

```sh
atelier --local           # or ATELIER_BACKEND=local, or /local in a session
```

The model is served by [oxinfer](https://github.com/KarpelesLab/oxinfer), a
separate program atelier fetches and runs for you. The model is
**Qwen3.8 27B**, which reads images as well as text and calls tools, at the
precision your machine can hold ([below](#which-model-it-chooses)).

## What happens the first time

1. **oxinfer is installed**: the newest release with a build for your
   platform is downloaded (about 16 MB), checked against the size the
   release lists (and its SHA-256, where GitHub lists one), and unpacked
   into `$XDG_DATA_HOME/atelier/oxinfer/` (default
   `~/.local/share/atelier/oxinfer/`). Linux x86_64 and arm64, and macOS.
   This is the one time atelier installs it; from then on [oxinfer updates
   itself](#keeping-oxinfer-up-to-date).
2. **The server is started**, by the [coordinator](coordinator.md): `oxinfer
   serve` on a loopback port with a key of its own, and no model yet. It
   takes a second.
3. **The model is chosen for the machine**, and atelier says which and why
   — say, *Qwen3.8 27B at 4 bits, chosen for this machine: it takes about
   19 GB, and the GPU (NVIDIA GeForce RTX 4090) has 24 GB*.
4. **You are asked before the model is downloaded.** The 8- and 4-bit
   models are one **56 GB download** (oxinfer quantizes as it loads), the
   3-bit one 14 GB. atelier shows what is left to download (the server
   looks at the repository and at what its cache already has, so a download
   that was interrupted is asked about for the rest only), the disk space
   free, the memory needed and available, where it would run and how fast
   — and asks *Download … (56 GB) now?*. Nothing that size is fetched
   unasked:
   - at a terminal, `atelier --local` and `/local` ask;
   - where nobody can be asked (`--print`, a script), it stops with an
     error unless `ATELIER_LOCAL_ACCEPT=1` is set;
   - `atelier local download` does it from a shell, with oxinfer's own
     progress display — the comfortable way for a long download.

   In a session the server does the fetching, and atelier says how far it
   is now and then (*downloaded 12 of 56 GB (22%), 45 MB/s, about 16 min
   left*). The download belongs to the server, not to the session: it goes
   on if you quit, for as long as the server runs, and what was fetched is
   kept either way. The weights go to oxinfer's cache
   (`~/.cache/oxinfer/hub`), where other uses of oxinfer find them too.

   Where the repository cannot be asked what it holds (no network), a model
   that is already in the cache is used as it is. A model atelier cannot
   size at all — a repository that does not exist, or one that needs a
   Hugging Face token you have not set — is an error rather than a download.
5. **The server is told to load the model.** It is read into memory in the
   background, and the first request waits for it — which makes that one
   the slow one.

After that, `atelier --local` starts at once.

## Speed, and why not a smaller model

How fast it answers depends on where the model fits. On Apple silicon,
whose memory is shared with the GPU, and on a GPU with 20 GB or more, the
27B model runs well. On a CPU alone it answers at a few tokens per second,
and an agent's requests are long (the tools alone are several thousand
tokens): it works, it is not quick. `atelier local status` shows oxinfer's
estimate for your machine.

A smaller model is faster and is not the answer: in our use, anything below
Qwen 3.8 does poorly at this work — it loses the thread of a task and gets
tool calls wrong — and the 27B is the smallest Qwen 3.8 there is. That is
why it is the model despite its size; what changes with the machine is only
how finely its weights are kept.

## Which model it chooses

With no model named, atelier takes the largest of these that fits your
machine with room to spare:

| Model | Memory | Download | Chosen when |
|---|---|---|---|
| Qwen3.8 27B at 8 bits (`Qwen/Qwen3.8-27B,quant=q8`) | 33 GB | 56 GB | it fits on the GPU |
| Qwen3.8 27B at 4 bits (`Qwen/Qwen3.8-27B,quant=q4`) | 19 GB | 56 GB | it fits on the GPU, or in RAM |
| Qwen3.8 27B at 3 bits, GGUF (`unsloth/Qwen3.8-27B-GGUF:UD-Q3_K_XL`, with its image reader) | 15 GB | 14 GB | it fits on Apple's GPU, or in RAM; or nothing fits |

- **On the GPU** means whole in its memory, in 92% of it less 1 GB
  (oxinfer's own rule for decoding on the GPU). oxinfer runs GGUF weights on
  Apple's GPU, not on an NVIDIA one, so the 3-bit model counts as fitting
  there only on a Mac. On Apple silicon the GPU's memory is the RAM, and the
  model must fit both ways.
- **In RAM** means in 60% of it — beyond that oxinfer streams the weights
  from storage, and the rest is what the system and your other programs
  need. On the CPU the 8 bits are not offered: it reads every weight for
  every token, so twice the bytes is half the speed.
- **Nothing fits** (16 GB and less, without a GPU that holds 15 GB): the
  3-bit model, and atelier says to expect it to be slow.
- bf16 is not among them: twice the memory of 8 bits and half its speed, for
  no difference we could see.

What the machine has comes from oxinfer (`oxinfer --models --json`: the
RAM, and the CUDA, Vulkan or Metal GPU it can use, with its memory), and
from `/proc/meminfo` or `sysctl hw.memsize` when oxinfer can't say. The
memory each model takes is oxinfer's estimate where its table has the model.
The choice goes by the machine's total memory, not what happens to be free
right now, so it is the same each time; the prompt before the download still
says when less is available than the model needs.

`atelier local status` shows the choice and the reason without starting
anything. A model atelier chose that the server already has is used,
whatever the choice would be today.

## Choosing another model

Any chat model oxinfer runs, written the way `oxinfer serve --model` takes
it — a Hugging Face repository, a folder or a file, then oxinfer's options:

```sh
ATELIER_LOCAL_MODEL="Qwen/Qwen3.8-27B,quant=q8" atelier --local
```

or, to keep it, in your user-global `~/.config/atelier/atelier.toml`:

```toml
[local]
model = "Qwen/Qwen3.8-27B,quant=q8"
```

Naming a model takes it over the choice, whatever the machine. To keep the
4 bits on a machine that would get the 8, or to take the 3 bits on one that
would get the 4 (it is a 14 GB download rather than 56):

```toml
[local]
model = "unsloth/Qwen3.8-27B-GGUF:UD-Q3_K_XL,mmproj=unsloth/Qwen3.8-27B-GGUF/mmproj-F16.gguf"
```

The `mmproj` part is what lets it read images (0.9 GB of the 14, fetched
when the model is first loaded). The 3-bit model calls tools and reads images
as the others do; how much is lost to the coarser weights on long tasks we
have not measured.

`oxinfer --models` lists what your machine can run, with estimated speeds.
A project's `atelier.toml` cannot set this: what gets downloaded and loaded
on your machine is yours to choose.

## One server for all your sessions

There is one oxinfer server per user, and it belongs to the
[coordinator](coordinator.md) — the background process that already
connects your ateliers. The first session that wants the local model has
the coordinator start it; the others are given its address and key.

- **Each session has its own model.** The server starts with none; a
  session asks it to load the one it wants, under a name made from what the
  model is (`Qwen3.8-27B-q4-1a2b3c` — it is what `/model` shows after
  `local/`). Two sessions that want the same model share it: it is loaded
  once. Two that want different ones are both served, side by side.
  oxinfer keeps what it has loaded within a memory budget (80% of the RAM)
  and unloads the model used least recently when another needs the room —
  it is loaded again by the next request that names it, which waits. So two
  large models on a machine that holds one work, slowly, each request
  paying for a load.
- **It stops by itself a minute after the last session using it exits**,
  freeing its memory. A session that crashes counts as gone: what tells the
  coordinator a session uses the server is a connection, which ends with
  the process however it ends. `ATELIER_LOCAL_IDLE` sets another number of
  seconds.
- **It outlives the coordinator.** When the coordinator is replaced (an
  atelier update, a crash), the server and the models in it stay: the next
  coordinator finds the server again — the process it was, answering to its
  key — and the sessions say again that they use it. If nobody does within
  the minute, it is stopped. A coordinator that is told to stop
  (`atelier coordinator stop`) while no session uses the server stops the
  server too.
- **It needs the coordinator.** With `ATELIER_NO_COORDINATOR` set, `--local`
  and `/local` fail and say so: there is nobody to run the server.

```sh
atelier local status     # the model and why, what it takes, the server and the models it has
atelier local download   # install oxinfer and fetch the model, from a shell
atelier local update     # have oxinfer update itself now
atelier local stop       # end the server now
atelier coordinator status   # among the rest: the server, and how many ateliers use it
```

If oxinfer itself dies, the coordinator notices and forgets it; the
sessions that used it get errors from their requests, and `/local` in each
starts over on a new server.

## Keeping oxinfer up to date

oxinfer updates itself: `oxinfer --update` fetches its latest release for
your platform, writes it beside the program and renames it into place.
atelier installs oxinfer once and after that only asks it to do this:

- **when a server is about to be started**, at most once a day. That is
  the moment a new version takes effect, and nothing is waiting on the old
  one. If the check fails (no network) the installed version is used;
- **at once when the installed oxinfer is too old** — atelier needs 0.0.16
  or newer, the first whose server takes models after it has started. An
  older one is brought up to date before the server starts, and atelier
  says so (*this oxinfer is too old for atelier: updating it…*). One so old
  that it cannot update itself is replaced the way oxinfer was first
  installed;
- **when you run `atelier local update`**, with oxinfer's own output.

A server running at the time is never touched: it keeps running the version
it started with (the system holds on to a program in use), and the next
server started — after the current one's sessions are done — is the new
version. For the same reason the server is started with
`--no-update-check`: oxinfer's web page could otherwise update and restart
it in place, which forgets the models your sessions had it load.

Not updated: an oxinfer of your own (`ATELIER_OXINFER_BIN`), and anything
when `ATELIER_NO_UPDATE=1` is set, which turns atelier's own update off
too. If such a program is too old, starting the local model fails and says
which version is needed.

## Settings

| Variable | |
|---|---|
| `ATELIER_BACKEND=local` | The same as `--local` |
| `ATELIER_LOCAL_MODEL` | The model to run, as `SOURCE[,KEY=VALUE...]`; over `[local] model`, and over the one chosen for the machine |
| `ATELIER_LOCAL_ACCEPT` | `1` agrees to the model download where nobody can be asked |
| `ATELIER_LOCAL_IDLE` | How many seconds the server is kept with no session using it (default 60). Read by the coordinator when it starts |
| `ATELIER_OXINFER_BIN` | An oxinfer executable to use instead of a downloaded one (never updated; 0.0.16 or newer) |
| `ATELIER_NO_UPDATE` | `1`: oxinfer is not asked to update itself (and atelier does not look for a newer atelier) |
| `ATELIER_NO_COORDINATOR` | Switches the [coordinator](coordinator.md) off, and the local model with it |

In a session the local model is the provider `local`
([`/providers`](configuration.md#provider--named-endpoints)), and the model
`local/<name>`, the name being the one the server knows it by; `/model` can
still switch to any other configured model, and `/local` back.

If nothing is configured at all and the built-in endpoint can't be reached,
the error says so and points here.

## Notes

- oxinfer's releases are not signed, and publish no checksums file of their
  own. What vouches for the download is GitHub over TLS, the size the
  release lists, and the digest GitHub gives for the archive.
- Not with the [Claude backend](claude.md), which runs claude's own models.
- The server's log is `oxinfer.log` in atelier's runtime directory
  (`$XDG_RUNTIME_DIR/atelier`), next to the coordinator's.
- The server listens on loopback only and wants its key for everything —
  the chat API and oxinfer's management API, which can load, fetch and
  delete models. The key is made anew for each server, given to it in its
  environment (not on its command line, which `ps` shows), kept in
  `oxinfer-server.json` in that directory (mode 0600) and handed to your
  ateliers over the coordinator's socket (mode 0600, in a directory of mode
  0700): it reaches your own processes and nobody else's.
- An atelier from before the coordinator ran the server starts its own,
  the old way (`atelier local-serve`), and the two do not share: until that
  atelier is quit there can be two servers. The old one stops by itself a
  minute after its last session.
