# `POST /speak`

Synthesize `text` with the active model and play it on the daemon's output
device.

The request returns as soon as the utterance is **queued**, not when it finishes
playing — the daemon keeps speaking after the connection closes. To follow an
utterance to its end, subscribe to
[`speaking_state` and `speech_progress`](./events.md) with the
[`playback_events`](../../scopes/playback_events.md) scope; those outlive any
one request, which is what a status widget actually needs.

**One utterance at a time, newest wins.** A `/speak` while something is already
playing interrupts it — that is what "speak this instead" means, and it is why
this endpoint is *not* guarded by `speech_in_progress`. Only swapping the model
out from under live synthesis is refused (see
[`/active_model`](./active_model.md)).

To stream text in as it is generated — an LLM reply spoken as it arrives —
use [`GET /speak/stream`](./speak/stream.md) instead. This endpoint takes
complete text.

## Auth

- **Required scope:** `speak`.
- `Authorization: Bearer <session_token>` is required.
- Tokens without the `speak` scope get `403 scope_denied`.

## Request

```http
POST /speak HTTP/1.1
Host: tts.local
Authorization: Bearer tts_…64hex…
Content-Type: application/json

{
  "text": "The kettle is boiling.",
  "voice": "alloy",
  "language": "en",
  "speed": 1.0,
  "instructions": "Read this calmly."
}
```

| Field          | Type     | Required | Meaning                                                                          |
|----------------|----------|----------|----------------------------------------------------------------------------------|
| `text`         | string   | yes      | What to speak. Empty or whitespace-only is `400 invalid_value`.                  |
| `voice`        | string   | no       | A voice id the active model declares. Omitted → the model's `default_voice`. A cloned voice is named `voice:<uuid>` — see [`/voices`](./voices.md#speaking-in-a-cloned-voice). |
| `language`     | string   | no       | BCP-47 override. Omitted → the configured language (see [`/language`](./language.md)). |
| `speed`        | number   | no       | Rate multiplier, roughly 0.5–2.0. Backends that cannot vary rate ignore it.      |
| `instructions` | string   | no       | Free-text delivery guidance, for models that accept it. Ignored otherwise.       |

`language` is also accepted at the top level of the request body, the way every
other endpoint takes it; both spellings mean the same thing.

The daemon normalizes `text` before synthesis — markup that would otherwise be
read out literally (emphasis markers, code fences, link targets, table pipes) is
stripped, and long text is split on sentence boundaries so playback can start
before the whole thing is synthesized. Numbers, dates, and currency are passed
through untouched: the model reads them, because a wrong number-to-words is
worse than none.

`text` is capped at **50 000 characters**. A model may declare a lower per-request
cap in its manifest (`max_input_chars`), which the daemon applies on top.

## Response (202)

```http
HTTP/1.1 202 Accepted
Content-Type: application/json

{
  "status": "success",
  "utterance_id": "utt_9f2c1e40"
}
```

`202`, not `200`: the audio has been accepted, and the device is still playing
it when you read this.

`utterance_id` is what correlates this call with the playback events, and what
[`/speak/stop`](./speak/stop.md) reports having cancelled. It is returned from
the first version of this endpoint even though the current policy is
one-at-a-time — adding an id later would break every client written against it.

## Errors

| HTTP | `error_code`        | Meaning                                                                                     |
|------|---------------------|---------------------------------------------------------------------------------------------|
| 400  | `invalid_value`     | `text` missing, empty, or over the cap; a `voice` the model does not declare or whose shape it did not opt into (see [`voice_kinds`](../../backend/config.md#voices)); or a malformed field. |
| 401  | `invalid_session`   | Token unknown / expired / `exe_changed` — re-auth and retry.                                |
| 403  | `scope_denied`      | Token lacks the `speak` scope.                                                              |
| 409  | `model_not_loaded`  | No model is loaded. Load one via [`POST /active_model`](./active_model.md) and retry.        |
| 429  | `rate_limited`      | Too many requests; back off.                                                                |
| 500  | —                   | The backend failed, or the output device could not be opened. `message` carries the reason.  |

A failure the caller did not cause — no model, a dead audio device, a backend
that refused — also raises a desktop notification, because a `speak` is as
likely to come from a keyboard shortcut as from an app with a UI to show the
error in. See [`/notification_method`](./notification_method.md).
