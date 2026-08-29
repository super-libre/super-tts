# `GET /speak/stream` (WebSocket)

Speak text **as it is produced**. The client sends deltas as they arrive; the
daemon synthesizes each sentence as it completes, so speech starts long before
generation finishes.

This is the LLM case. With [`POST /speak`](../speak.md) you must have the whole
reply before the first word is heard; here the user hears the opening sentence
while the model is still writing the third.

## Why a WebSocket and not a series of POSTs

The back-channel. When the user interrupts a half-generated reply, the daemon
has to cancel in-flight synthesis, drop the queue, flush the audio ring **and**
tell the client to stop generating. A one-way append API makes that last part
awkward — the client would have to poll to discover that its own utterance was
pre-empted. Here it is a frame.

## Auth

- **Required scope:** `speak`.
- `Authorization: Bearer <session_token>` on the upgrade request.
- Tokens without the `speak` scope get `403 scope_denied` before the upgrade.

## Session shape

```
client                                daemon
  │  GET /speak/stream  (upgrade)       │
  │ ──────────────────────────────────► │
  │  {"type":"start", …}                │
  │ ──────────────────────────────────► │
  │            {"type":"utterance","id":"utt_…"}
  │ ◄────────────────────────────────── │
  │  {"type":"text","delta":"The kettle"}
  │ ──────────────────────────────────► │
  │  {"type":"text","delta":" is boiling."}
  │ ──────────────────────────────────► │   ← sentence complete: synthesis starts
  │            {"type":"progress","spoken_ms":0,"queued_ms":1180}
  │ ◄────────────────────────────────── │
  │  {"type":"end"}                     │
  │ ──────────────────────────────────► │
  │            {"type":"done","id":"utt_…","chunks":1}
  │ ◄────────────────────────────────── │
```

The first frame **must** be `start`: every other frame needs the options it
carries, and guessing them would produce a wrong voice rather than an error.

## Client frames

All frames are JSON text, discriminated on `type`.

| `type`   | Fields                                              | Meaning                                              |
|----------|-----------------------------------------------------|------------------------------------------------------|
| `start`  | `voice?`, `language?`, `speed?`, `instructions?`    | Open the utterance. Same fields as `POST /speak`.    |
| `text`   | `delta` (string)                                    | More text. Repeatable, any size, including partial words. |
| `end`    | —                                                   | No more text is coming; play out what remains.       |
| `cancel` | —                                                   | Stop now and drop queued audio.                      |

Deltas are appended to a streaming normalizer, so a delta may split a markup
construct in half — `**bo` then `ld**` — without the asterisks being spoken. The
daemon holds an ambiguous tail until it can resolve it.

## Server frames

| `type`      | Fields                                       | Meaning                                                       |
|-------------|----------------------------------------------|---------------------------------------------------------------|
| `utterance` | `id`                                         | Sent once, right after `start`. The utterance this session produces. |
| `mark`      | `start_ms?`, `end_ms?`, `start_char?`, `end_char?` | The backend aligned a span of audio to a span of the text. Only from backends that emit marks. |
| `progress`  | `spoken_ms`, `queued_ms`                     | How far playback has got. Positions within the utterance, derived from the device format — not wall-clock, so they stay correct while the stream is idle. |
| `done`      | `id`, `chunks`                               | The utterance is complete. **The stream closes after this.**   |
| `error`     | `message`                                    | Fatal. **The stream closes after this.**                       |

## Limits

| Limit                       | Value | On breach                                                     |
|-----------------------------|-------|---------------------------------------------------------------|
| Concurrent sessions         | 4     | `503 speak_sessions_busy` before the upgrade completes.       |
| Idle timeout (no frame)     | 120 s | The session is aborted; an `error` frame is sent if possible. |

The idle timeout exists because a session holds the utterance slot and the
output device. A client that goes away without closing — a half-open TCP
connection, a crashed process — must not hold them forever.

The session cap is on *sessions*, not on speech: only one utterance can be
playing at a time regardless, since the engine is newest-wins. The cap is there
because each session holds a task and a buffer, and an authorized client should
not be able to open them without limit.

## Errors

| HTTP / frame                | Meaning                                                       |
|-----------------------------|---------------------------------------------------------------|
| `401 invalid_session`       | Token unknown / expired / `exe_changed` — re-auth and retry.  |
| `403 scope_denied`          | Token lacks the `speak` scope.                                |
| `503 speak_sessions_busy`   | Four sessions are already open.                               |
| `{"type":"error", …}`       | Anything that goes wrong after the upgrade: no model loaded, the backend failed, the audio device could not be opened. |
