# Old-config compatibility fixtures

Each `vX.Y.Z/` directory holds the configuration file(s) that release persisted,
hand-derived from that tag's source. They are loaded by the applet config tests
(`just config-compat`) to prove the current code still loads configs written by
older releases — it must load, migrate, or reset, never crash.

**On every release, add a `vX.Y.Z/` directory** with the configs that version
wrote (`applet-full.toml`, and `daemon.toml` once one exists — see below). Do
not reformat existing files — they represent real on-disk user configs.

## No daemon corpus yet

The `v0.1.x` directories are inherited from Super STT, whose `daemon.toml`
described recording, typing, and transcription. Super TTS renamed that section
to `[synthesis]` and dropped those keys, so those files no longer describe any
config this daemon can be asked to load — keeping them would have tested a
migration path that no user can be on. They were removed rather than rewritten,
because a hand-edited fixture claiming to be "what v0.1.3 wrote" is worse than
no fixture at all.

The applet's config did not change shape in the conversion, so its corpus is
intact and still covers every release. Add `daemon.toml` back starting with
Super TTS's first release.
