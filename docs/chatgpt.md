# Sign in with ChatGPT

"Sign in with ChatGPT" (SIWC) lets atelier run requests on your own **ChatGPT
plan** instead of an API key. It's OpenAI's own sanctioned, open-source OAuth
flow for this (authorization-code + PKCE, no client secret) — not a
reverse-engineered client — so it's a deliberate, separate backend alongside
atelier's default "bring your own API" mode (any OpenAI-compatible
chat/completions endpoint via `ATELIER_BASE_URL`/`ATELIER_API_KEY`).

## Using it

```
› /login
opening your browser to sign in with ChatGPT…
signed in — requests now use your ChatGPT plan (Responses API)
```

`/login` opens your browser to OpenAI's sign-in page; after you approve, a
loopback callback on `127.0.0.1` catches the authorization code and exchanges
it for tokens. From then on, every turn in this session is sent through the
**Responses API** (`https://api.openai.com/v1/responses`) instead of the
configured chat/completions endpoint.

`/logout` signs out — it deletes the stored tokens and switches subsequent
requests back to the configured endpoint:

```
› /logout
signed out — using the configured endpoint
```

## Why it's a separate backend

A ChatGPT-plan access token is only valid against the Responses API — it is
**not** a bearer token for arbitrary `chat/completions` endpoints. So signing
in with ChatGPT doesn't change `ATELIER_BASE_URL`/`ATELIER_MODEL`; it adds a
second, parallel backend (`src/provider/responses.rs`) that the agent loop
routes to instead, whenever you're signed in. `/config` (and `/model` with no
argument) show which backend is active as `chatgpt:<model>` versus the
configured model id — see [Configuration](configuration.md#config).

## Choosing a model

Once signed in, `/model <name>` sets the model requested over the Responses
API (it targets whichever backend is currently active), defaulting to
`gpt-5` right after `/login`:

```
› /model
model: chatgpt:gpt-5
› /model gpt-5-mini
model set to gpt-5-mini
```

Which models you can actually use depends on your ChatGPT plan — `/models`
doesn't query this; it only lists what the *configured* chat/completions
endpoint reports, so check OpenAI's own docs/ChatGPT UI for what your plan
offers and set it by name with `/model`.

## Token storage

Tokens are written to `$XDG_CONFIG_HOME/atelier/chatgpt-auth.json` (falling
back to `$HOME/.config/atelier/chatgpt-auth.json`), mode `0600`. They are
never sent anywhere but OpenAI. A successful `/login` is restored
automatically on your next launch — atelier loads this file at startup and
reactivates the ChatGPT backend before you type anything, so you don't need
to `/login` again each session; `/logout` (or deleting the file) is what
stops that.

## Caveats

- This is the Responses API, not chat/completions — features or quirks that
  differ between the two (e.g. how tool calls or multimodal content are
  framed) are implemented separately in `src/provider/responses.rs`.
- Available models are whatever your ChatGPT plan grants; there's no
  discovery endpoint for it in atelier, so `/model` just takes you at your
  word.
- Signing out clears local tokens only; it doesn't revoke them on OpenAI's
  side.
