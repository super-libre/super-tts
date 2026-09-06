# voices scope

> Scope: **voices** (add, list, play back, rename, and delete the recordings a
> cloned voice is built from).

The `voices` scope is the cloned-voice library. A `voices` token can upload a
reference recording, list what is stored, play a clip back, rename one, and
delete one. That is the whole surface: the library holds recordings and the
metadata describing them, and nothing else.

Speaking *in* a cloned voice needs [`speak`](./speak.md), not this scope — a
client passes the `voice:<uuid>` id on `POST /speak` and never touches the
audio. The two are separate deliberately: an app that reads text aloud in a
voice the user already cloned has no reason to hold the recordings.

## Why it is not part of `settings`

Every other stored preference is a value the user typed. A voice sample is a
**recording of a person**, usually the user, and it is the one asset in the
daemon that identifies someone. The [`settings`](./settings.md) scope is broad
by design — a settings app holds it to manage models, devices, and backends —
and folding voice recordings into it would hand every such app the ability to
read them back as audio.

So the library gets its own scope, the consent prompt names it in those terms
("list, play back, and delete your saved voice recordings"), and a client that
wants both asks for both. Scopes never imply one another.

Clips are stored under `$XDG_DATA_HOME/super-tts/voices`, owner-readable only
(`0600`), and never leave the machine except to the loaded backend — which is
local unless the user selected an online model.

## Endpoint reference

| Endpoint                                                          | Methods                | Notes                                                     |
|-------------------------------------------------------------------|------------------------|-----------------------------------------------------------|
| [`/voices`](../endpoints/v1/voices.md#get-voices)                 | GET, POST              | List the library; add a voice from a WAV upload.          |
| [`/voices/{id}`](../endpoints/v1/voices.md#get-voicesid)          | GET, PATCH, DELETE     | Read, rename, or delete one voice.                        |
| [`/voices/{id}/audio`](../endpoints/v1/voices.md#get-voicesidaudio) | GET                  | The stored clip, as `audio/wav`.                          |

See [voices.md](../endpoints/v1/voices.md) for the full request/response shapes
and error model. Transport and framing are in [transport.md](../transport.md);
how scopes compose and how a token is obtained are in [auth.md](../auth.md).
