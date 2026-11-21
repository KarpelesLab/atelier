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
2. **The model is chosen for the machine**, and atelier says which and why
   — say, *Qwen3.8 27B at 4 bits, chosen for this machine: it takes about
   19 GB, and the GPU (NVIDIA GeForce RTX 4090) has 24 GB*.
3. **You are asked before the model is downloaded.** The 8- and 4-bit
   models are one **56 GB download** (oxinfer quantizes as it loads), the
   3-bit one 14 GB. atelier shows what oxinfer estimates for your machine —
   the download, the disk space free, the memory needed and available,
   where it would run and how fast — and asks *Download … (56 GB) now?*.
   Nothing that size is fetched unasked:
   - at a terminal, `atelier --local` and `/local` ask;
   - where nobody can be asked (`--print`, a script), it stops with an
     error unless `ATELIER_LOCAL_ACCEPT=1` is set;
   - `atelier local download` does it from a shell, with oxinfer's own
     progress display — the comfortable way for a long download.

   The weights go to oxinfer's cache (`~/.cache/oxinfer/hub`), where other
   uses of oxinfer find them too.
4. **The server is started**, on a loopback port with a key of its own. The
   model is loaded into memory on the first request, which is therefore the
   slow one.

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
anything. A server already running a model atelier chose is joined whatever
the choice would be today.

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
when the server first starts). The 3-bit model calls tools and reads images
as the others do; how much is lost to the coarser weights on long tasks we
have not measured.

`oxinfer --models` lists what your machine can run, with estimated speeds.
A project's `atelier.toml` cannot set this: what gets downloaded and loaded
on your machine is yours to choose.

## One server for all your sessions

Every atelier you run with the local model shares one server — a model this
size is not something to load twice. It is started by the first session that
needs it and **stops by itself a minute after the last one exits**, freeing
its memory. A session that crashes counts as gone.

If you ask for a different model while a server is running one, it is
replaced when nobody else is using it, and refused otherwise.

```sh
atelier local status     # the model and why, what it takes, whether a server is up
atelier local download   # install oxinfer and fetch the model, from a shell
atelier local update     # install a newer oxinfer now, if there is one
atelier local stop       # end the server now
```

## Keeping oxinfer up to date

Once a day at most, when a session starts the local model, atelier asks
oxinfer's releases in the background whether there is a newer one than the
installed program says it is (`oxinfer --version`). A newer one is installed
the way the first was: the newest release with an archive for your platform,
checked against its size and its SHA-256 (GitHub lists one for each of
oxinfer's archives), test-run, then renamed over the old program — atomic,
so nothing ever sees half a program. A server running at the time is not
stopped: it keeps running the version it started with (the system holds on
to a program in use), and the next server started, once the current one's
sessions are done, is the new version. The session is told when an update
was installed.

`atelier local update` does it on demand. Not updated: an oxinfer of your
own (`ATELIER_OXINFER_BIN`), and anything when `ATELIER_NO_UPDATE=1` is set,
which turns atelier's own update off too.

## Settings

| Variable | |
|---|---|
| `ATELIER_BACKEND=local` | The same as `--local` |
| `ATELIER_LOCAL_MODEL` | The model to run, as `SOURCE[,KEY=VALUE...]`; over `[local] model`, and over the one chosen for the machine |
| `ATELIER_LOCAL_ACCEPT` | `1` agrees to the model download where nobody can be asked |
| `ATELIER_OXINFER_BIN` | An oxinfer executable to use instead of a downloaded one (never updated) |
| `ATELIER_NO_UPDATE` | `1` stops the daily check for a newer oxinfer (and for a newer atelier) |

In a session the local model is the provider `local`
([`/providers`](configuration.md#provider--named-endpoints)); `/model` can
still switch to any other configured model, and `/local` back.

If nothing is configured at all and the built-in endpoint can't be reached,
the error says so and points here.

## Notes

- oxinfer's releases are not signed, and publish no checksums file of their
  own. What vouches for the download is GitHub over TLS, the size the
  release lists, and the digest GitHub gives for the archive.
- Not with the [Claude backend](claude.md), which runs claude's own models.
- The server's log is `oxinfer.log` in atelier's runtime directory
  (`$XDG_RUNTIME_DIR/atelier`).
