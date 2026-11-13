# A model on this machine

`atelier --local` runs the model on your own computer: no endpoint to
configure, no API key, no subscription. It is what to use when you have none
of those, or want nothing to leave the machine.

```sh
atelier --local           # or ATELIER_BACKEND=local, or /local in a session
```

The model is served by [oxinfer](https://github.com/KarpelesLab/oxinfer), a
separate program atelier fetches and runs for you. The default model is
**Qwen3.8 27B** at 4 bits: it reads images as well as text and calls tools.

## What happens the first time

1. **oxinfer is installed**: the newest release with a build for your
   platform is downloaded (about 13 MB), checked against the size the
   release lists (and its SHA-256, where GitHub lists one), and unpacked
   into `$XDG_DATA_HOME/atelier/oxinfer/` (default
   `~/.local/share/atelier/oxinfer/`). Linux x86_64 and arm64, and macOS.
2. **You are asked before the model is downloaded.** The default model is a
   **56 GB download** and takes about **19 GB of memory**. atelier shows
   what oxinfer estimates for your machine — the download, the disk space
   free, the memory needed and available, where it would run and how fast —
   and waits for a yes. Nothing that size is fetched unasked:
   - at a terminal, `atelier --local` and `/local` ask;
   - where nobody can be asked (`--print`, a script), it stops with an
     error unless `ATELIER_LOCAL_ACCEPT=1` is set;
   - `atelier local download` does it from a shell, with oxinfer's own
     progress display — the comfortable way for a long download.

   The weights go to oxinfer's cache (`~/.cache/oxinfer/hub`), where other
   uses of oxinfer find them too.
3. **The server is started**, on a loopback port with a key of its own. The
   model is loaded into memory on the first request, which is therefore the
   slow one.

After that, `atelier --local` starts at once.

## Speed, honestly

A 27B model on a CPU answers at a few tokens per second, and an agent's
requests are long (the tools alone are several thousand tokens). It works;
it is not quick. A GPU with enough memory changes that, and so does a
smaller model. `atelier local status` shows oxinfer's estimate for the
model you have chosen.

## Choosing another model

Any chat model oxinfer runs, written the way `oxinfer serve --model` takes
it — a Hugging Face repository, a folder or a file, then oxinfer's options:

```sh
ATELIER_LOCAL_MODEL="google/gemma-4-E4B-it,quant=q8" atelier --local
```

or, to keep it, in your user-global `~/.config/atelier/atelier.toml`:

```toml
[local]
model = "Qwen/Qwen3.5-9B,quant=q8"
```

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
atelier local status     # the model, what it takes, whether a server is up
atelier local download   # install oxinfer and fetch the model, from a shell
atelier local stop       # end the server now
```

## Settings

| Variable | |
|---|---|
| `ATELIER_BACKEND=local` | The same as `--local` |
| `ATELIER_LOCAL_MODEL` | The model to run, as `SOURCE[,KEY=VALUE...]`; over `[local] model` |
| `ATELIER_LOCAL_ACCEPT` | `1` agrees to the model download where nobody can be asked |
| `ATELIER_OXINFER_BIN` | An oxinfer executable to use instead of a downloaded one |

In a session the local model is the provider `local`
([`/providers`](configuration.md#provider--named-endpoints)); `/model` can
still switch to any other configured model, and `/local` back.

If nothing is configured at all and the built-in endpoint can't be reached,
the error says so and points here.

## Notes

- atelier does not update the installed oxinfer; delete
  `~/.local/share/atelier/oxinfer/oxinfer` to get the newest one, or run its
  own `oxinfer --update`.
- oxinfer's releases are not signed. What vouches for the download is GitHub
  over TLS, the size the release lists, and the digest where GitHub gives
  one.
- Not with the [Claude backend](claude.md), which runs claude's own models.
- The server's log is `oxinfer.log` in atelier's runtime directory
  (`$XDG_RUNTIME_DIR/atelier`).
