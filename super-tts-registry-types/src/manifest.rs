// SPDX-License-Identifier: GPL-3.0-only
//! The canonical `backend.toml` manifest. This is the single source of truth
//! for the manifest contract (`docs/protocol/backend/config.md`): the daemon
//! parses it for discovery, the registry indexer parses it for release
//! validation, and the published JSON Schema is generated from these types.
//!
//! Parsing is deliberately lenient where the runtime allows it (unknown
//! fields ignored, `[assets]` optional); consumer-specific policy lives in
//! each consumer's `validate` step.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The option name that carries a backend's configurable endpoint.
///
/// It is the one option whose *value* changes what the daemon permits: the host
/// it names is authorized for egress with the SSRF guard relaxed. That is sound
/// only while the value is the user's, so consumers treat a manifest-supplied
/// one as no value — the indexer refuses such a release, the catalog synthesis
/// in [`IndexBackend::from_manifest`](crate::index::IndexBackend::from_manifest)
/// drops it, and the daemon drops it at discovery. Named here so those checks
/// cannot drift apart over a string literal.
pub const BASE_URL_OPTION: &str = "base_url";

/// A backend's `backend.toml`: identity, packaging, network policy,
/// secrets/options, and the models it provides.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Manifest {
    /// Backend identity and packaging.
    pub backend: BackendMeta,
    /// Outbound network the backend is permitted to reach.
    #[serde(default)]
    pub network: Network,
    /// Optional feature flags that unlock transport extensions.
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Binary artifacts a release publishes. Optional for locally installed
    /// backends; required (per `kind`) for registry publication.
    #[serde(default)]
    pub assets: Assets,
    /// Encrypted credentials the backend needs at runtime (e.g. API keys).
    #[serde(default)]
    pub secrets: Vec<Secret>,
    /// Non-secret configuration the user can set through the settings UI.
    #[serde(default)]
    pub options: Vec<Opt>,
    /// One entry per model the backend provides.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

/// `[backend]` — identity and packaging.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BackendMeta {
    /// Globally unique reverse-DNS identifier, e.g. `app.super-tts.piper`.
    /// Names the directory this backend installs into. Optional on disk so a
    /// backend installed before the field existed keeps loading; required for
    /// registry listing, which the indexer enforces.
    #[serde(default)]
    pub id: Option<String>,
    /// Canonical repository id, e.g. `github.com/<owner>/<repo>`. Becomes the
    /// `source` of every model this backend provides and must be unique
    /// across installed backends. For a monorepo, namespace it under the repo
    /// (e.g. `github.com/<owner>/<repo>/openai`).
    pub source: String,
    /// Human-readable display name.
    pub name: String,
    /// Backend version (semver). Must match the release tag's version when
    /// published through the registry.
    pub version: String,
    /// Selects the transport.
    pub kind: Kind,
    /// Path, relative to the backend directory, to the executable
    /// (`subprocess`) or the `.wasm` component (`wasm`). May be a nested
    /// relative path such as `bin/launcher` for multi-file bundles.
    /// Must not escape the backend directory: no absolute paths, no `..`
    /// components, no backslashes, no embedded NUL.
    pub entrypoint: String,
    /// The backend-protocol contract version implemented.
    pub contract: Contract,
    /// License of the backend: a current SPDX identifier that is OSI-approved
    /// or FSF Free/Libre (e.g. `Apache-2.0`, `MIT`, `GPL-3.0-only`), or the
    /// literal `other` for a license outside that set. Optional for local
    /// installs; required for registry publication, where the indexer rejects
    /// a manifest that omits the field or declares an unrecognized value.
    #[serde(default)]
    pub license: Option<String>,
    /// One-line, human-readable summary shown in the registry/Browse listing.
    /// Required for every backend.
    pub description: String,
}

/// Transport a backend uses: a wasm32 component or a native executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A `wasm32` component loaded in the daemon's WASM host.
    Wasm,
    /// A native executable run in the daemon's subprocess sandbox.
    Subprocess,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wasm => write!(f, "wasm"),
            Self::Subprocess => write!(f, "subprocess"),
        }
    }
}

/// Backend-protocol contract version: the one thing a manifest declares about
/// what it implements.
///
/// A contract generation names a set of manifest fields and backend routes.
/// Each generation is additive over the one before, so a backend declares the
/// *lowest* generation whose fields it uses, and a daemon supports every
/// generation up to the one it was built with.
///
/// This is a closed enum on purpose. A daemon that predates a generation
/// cannot parse a manifest declaring it, which is what stops such a daemon
/// from installing a backend it cannot drive: the refusal needs no field the
/// old daemon would have to know about, because it is the *absence* of
/// knowledge that refuses. Every generation added here therefore also gates
/// itself against every daemon released before it.
///
/// Variant order is generation order; the derived `Ord` relies on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Contract {
    /// The v1 contract (`docs/protocol/backend/contract.md`).
    V1,
    /// Adds the per-architecture selector on `[[models.files]]`: `accel`,
    /// `cuda_major`, `cuda_sm`, `gfx`, `vulkan_api` and `optional`. One
    /// `destination` may be published as several host-specific variants, and
    /// the daemon downloads the one this machine can use — the same question
    /// `[[assets.subprocess]]` already answers for the executable, asked of
    /// the data beside it.
    ///
    /// It is a generation rather than an optional extra because a daemon that
    /// does not know the selector reads the variants as ordinary files and
    /// downloads every one of them onto the same path. There is no spelling of
    /// this that degrades safely, so the refusal has to come from the
    /// generation.
    V2,
}

impl Contract {
    /// The newest generation this crate understands. A manifest may not
    /// declare anything above it, because the closed enum refuses to parse it.
    pub const LATEST: Self = Self::V2;

    /// Every generation, oldest first.
    pub const ALL: &'static [Self] = &[Self::V1, Self::V2];

    /// The generation immediately before this one; `None` for the first.
    #[must_use]
    pub fn previous(self) -> Option<Self> {
        Self::ALL
            .iter()
            .rev()
            .copied()
            .find(|candidate| *candidate < self)
    }

    /// The Super TTS release that first understood this generation — the
    /// floor below which a daemon cannot install a backend declaring it.
    ///
    /// The indexer stamps this onto each index entry as `min_client`, so a
    /// daemon that does not know the generation can still tell the user what
    /// to update to. A backend author never writes a Super TTS version: they
    /// declare the contract, and this table is what it means.
    ///
    /// Reported, never compared: the daemon decides compatibility by whether
    /// it can parse the generation at all, so a prerelease of the version
    /// named here (`0.2.0-beta.1`, which semver orders *below* `0.2.0`) is not
    /// wrongly locked out. It is a string for the user, not a gate.
    ///
    /// A new row is a forecast until its release ships, and nothing can check
    /// it: the version that introduces a generation is by definition not yet
    /// tagged when the row is written. Renumbering that release means
    /// renumbering here.
    #[must_use]
    pub const fn min_client(self) -> &'static str {
        match self {
            // The manifest and its `[backend].contract` field both date from
            // the first Super TTS release; there is no earlier daemon to gate.
            Self::V1 => "0.1.0",
            Self::V2 => "0.2.0",
        }
    }
}

impl fmt::Display for Contract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V1 => write!(f, "v1"),
            Self::V2 => write!(f, "v2"),
        }
    }
}

impl std::str::FromStr for Contract {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|c| c.to_string() == s)
            .ok_or_else(|| {
                let known: Vec<String> = Self::ALL.iter().map(ToString::to_string).collect();
                format!(
                    "unknown contract `{s}`; this build knows {}",
                    known.join(", ")
                )
            })
    }
}

/// Routed through `FromStr` so one table — `ALL` plus `Display` — is the only
/// place a generation is spelled, and so the error names what this build does
/// know. That message is what a user sees when a daemon meets a backend from a
/// newer generation, so it is worth more than serde's "unknown variant".
impl<'de> Deserialize<'de> for Contract {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// What a generation does to a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldRule {
    /// The field does not exist below `since`. Declaring it under an older
    /// generation is an error, because that generation's schema has no such
    /// key — an author validating against it would be told the field is
    /// unknown while the daemon quietly honored it.
    Added,
    /// The field exists in every generation, but from `since` on a manifest
    /// must declare it. Used to close an optionality that only survived for
    /// backward compatibility: the new generation is a clean break, so it can
    /// demand what older ones could only recommend.
    RequiredFrom,
}

/// A manifest field and what a contract generation does to it.
///
/// The one table behind both enforcement points: [`Manifest::parse`] holds a
/// manifest to the rules of the contract it declares, and the generated schema
/// encodes the same rules, so an editor flags a violation before anything is
/// published. Adding a field to a new generation means adding a row here, and
/// nothing else has to be taught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractField {
    /// The generation the rule takes effect in.
    pub since: Contract,
    /// Which rule this row expresses.
    pub rule: FieldRule,
    /// The table the field lives in, as a dotted path: `backend` for
    /// `[backend]`, `models` for each `[[models]]` entry, `models.files` for
    /// each `[[models.files]]` entry under one, and so on. Every segment must
    /// appear in [`ARRAY_TABLES`] if it is an array of tables, because the
    /// raw-document walk and the schema builder both decide at each step
    /// whether to descend into an array or a plain table.
    pub table: &'static str,
    /// The field's key within that table.
    pub key: &'static str,
}

impl ContractField {
    /// How the field is spelled in a manifest and in error messages, e.g.
    /// `[[models]].role`.
    #[must_use]
    pub fn path(&self) -> String {
        if self.is_array_table() {
            format!("[[{}]].{}", self.table, self.key)
        } else {
            format!("[{}].{}", self.table, self.key)
        }
    }

    /// Whether the field's table is an array of tables (`[[models]]`) rather
    /// than a plain one (`[backend]`).
    ///
    /// The single answer for every consumer: the manifest spelling above, the
    /// raw-document audit, and the schema rule (which must attach to `items`
    /// for an array). Adding a row for a table not listed in [`ARRAY_TABLES`]
    /// fails the schema's own test rather than silently generating a rule that
    /// matches nothing.
    #[must_use]
    pub fn is_array_table(&self) -> bool {
        ARRAY_TABLES.contains(&self.table)
    }

    /// The path's segments, outermost first, each paired with whether that
    /// segment is an array of tables.
    ///
    /// `models.files` walks `("models", true)` then `("files", true)`: the
    /// consumer descends into every `[[models]]` entry, then into every
    /// `[[models.files]]` entry under it. A single-segment path yields one
    /// pair and behaves exactly as it did before nesting existed.
    #[must_use]
    pub fn segments(&self) -> Vec<(&'static str, bool)> {
        let mut out = Vec::new();
        let mut end = 0;
        for segment in self.table.split('.') {
            // `end` walks the original string so each level can be looked up
            // whole: `models`, then `models.files`. The `+ 1` is the separator,
            // which every segment but the first is preceded by.
            end += segment.len() + usize::from(end != 0);
            out.push((segment, ARRAY_TABLES.contains(&&self.table[..end])));
        }
        out
    }

    /// The name of this table's type in the generated JSON schema, or `None`
    /// for a table the schema builder has no mapping for.
    #[must_use]
    pub fn schema_definition(&self) -> Option<&'static str> {
        match self.table {
            "backend" => Some("BackendMeta"),
            "models" => Some("ModelEntry"),
            "models.files" => Some("FileSpec"),
            "secrets" => Some("Secret"),
            "options" => Some("Opt"),
            _ => None,
        }
    }
}

/// Every manifest table spelled as an array of tables (`[[models]]`), by its
/// dotted path.
///
/// A nested table lists each of its own prefixes, not just its full path:
/// walking `models.files` has to know that `models` is an array before it can
/// look inside one, and the schema builder has to emit an `items` level for it.
const ARRAY_TABLES: &[&str] = &["models", "models.files", "secrets", "options"];

/// Every field rule a generation after v1 introduces.
///
/// Adding a field to a new generation means adding a row here, and both
/// [`Manifest::parse`] and the published JSON Schema learn the rule from it
/// without being taught separately.
///
/// Field **names** only. A generation that widens an existing field's value
/// set instead — a new `VoiceKind`, a new `Device` — cannot be expressed here,
/// and a manifest using such a value under an older `contract` is caught by
/// that field's own `FromStr` rather than by this table.
pub const CONTRACT_FIELDS: &[ContractField] = &[
    // v2's per-architecture file selector. One row per key: `contract = "v1"`
    // has no `[[models.files]]` selector at all, so writing any of them under
    // it is refused rather than honored by a daemon whose peers would ignore
    // it.
    ContractField {
        since: Contract::V2,
        rule: FieldRule::Added,
        table: "models.files",
        key: "accel",
    },
    ContractField {
        since: Contract::V2,
        rule: FieldRule::Added,
        table: "models.files",
        key: "cuda_major",
    },
    ContractField {
        since: Contract::V2,
        rule: FieldRule::Added,
        table: "models.files",
        key: "cuda_sm",
    },
    ContractField {
        since: Contract::V2,
        rule: FieldRule::Added,
        table: "models.files",
        key: "gfx",
    },
    ContractField {
        since: Contract::V2,
        rule: FieldRule::Added,
        table: "models.files",
        key: "vulkan_api",
    },
    ContractField {
        since: Contract::V2,
        rule: FieldRule::Added,
        table: "models.files",
        key: "optional",
    },
];

/// `[network]` — outbound network policy.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Network {
    /// Host or `host:port` egress allowlist. Empty or absent means no
    /// network. Honored for `wasm` backends; must be empty for `subprocess`
    /// backends (the transport provides no network).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

/// `[capabilities]` — transport extensions beyond the base `/v1` contract.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Capabilities {
    /// Opt into the realtime WebSocket import/export. wasm-only — a
    /// `subprocess` backend declaring this is rejected at discovery.
    /// Required for any model with `realtime = true`. Default `false`.
    #[serde(default)]
    pub websocket: bool,
    /// Reserved. Opts a backend into an incremental-text path, so it can hold
    /// an upstream provider's prosodic context across deltas instead of
    /// receiving one chunk per request.
    ///
    /// Not specified yet: the daemon implements no such path, and a manifest
    /// declaring it is rejected at parse rather than silently ignored — a
    /// backend author who sets this is expecting behavior that does not exist,
    /// and the loud failure is the honest answer.
    #[serde(default)]
    pub streaming_input: bool,
}

/// `[assets]` — binary artifacts a release publishes, so the registry indexer
/// and the daemon's installer can find them without guessing.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Assets {
    /// Filename of the wasm component on the GitHub release. Required for
    /// registry publication when `kind = "wasm"`.
    #[serde(default)]
    pub wasm: Option<String>,
    /// One entry per built subprocess variant. Required (non-empty) for
    /// registry publication when `kind = "subprocess"`.
    #[serde(default)]
    pub subprocess: Vec<SubprocessAsset>,
}

/// One `[[assets.subprocess]]` build variant.
///
/// The variant's `.tar.gz` is named by `file`, or — when it would exceed the
/// 2 GiB GitHub release-asset limit — by `parts`, whose byte-for-byte
/// concatenation in order is the archive. Exactly one of the two is set
/// (enforced by [`Manifest::parse`]).
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SubprocessAsset {
    /// Single-file archive: the filename on the GitHub release (`.tar.gz`).
    /// Mutually exclusive with `parts`. The archive must contain
    /// `bin/<entrypoint>`.
    #[serde(default)]
    pub file: Option<String>,
    /// Multi-part archive: ordered release filenames whose byte-for-byte
    /// concatenation is the `.tar.gz`. Mutually exclusive with `file`; use when
    /// the archive exceeds the 2 GiB release-asset limit. The indexer pins each
    /// part independently.
    #[serde(default)]
    pub parts: Vec<String>,
    /// Rust target triple, e.g. `x86_64-unknown-linux-gnu`. Tier-1/2 only;
    /// the indexer rejects unknown triples.
    pub target: String,
    /// Acceleration backends this build carries. A single string is accepted
    /// and read as a one-element list, which is what every published manifest
    /// uses; an array declares a binary carrying several runtimes, and the
    /// daemon tells it at load time which one to use. Must be non-empty.
    #[serde(deserialize_with = "one_or_many")]
    pub accel: Vec<Accel>,
    /// CUDA major version this build targets. Required when `accel` contains
    /// `cuda`, forbidden otherwise.
    #[serde(default)]
    pub cuda_major: Option<u32>,
    /// Compute capability (e.g. `75`, `86`, `90`, `120`). Omit to match any
    /// compute capability — use for multi-architecture framework builds
    /// (e.g. a `PyTorch` wheel). An exact-SM asset is preferred over a
    /// wildcard when both match. Forbidden when `accel` lacks `cuda`.
    #[serde(default)]
    pub cuda_sm: Option<u32>,
    /// Whether this build links cuDNN. Allowed only when `accel` contains
    /// `cuda`. Default `false`.
    #[serde(default)]
    pub cudnn: bool,
    /// AMD architecture targets this build carries, in `--offload-arch`
    /// spelling. Required when `accel` contains `rocm`, forbidden otherwise.
    ///
    /// There is no wildcard, deliberately breaking symmetry with `cuda_sm`:
    /// PTX gives CUDA a JIT path that makes "any compute capability" a true
    /// claim, while HIP code objects are architecture-specific AMDGCN ISA with
    /// no equivalent. A wildcard would install a binary that fails at model
    /// load instead of falling back to CPU. Fat builds list every target they
    /// were compiled for.
    #[serde(default)]
    pub gfx: Vec<crate::arch::GfxSpec>,
    /// Minimum Vulkan API version this build requires. Allowed only when
    /// `accel` contains `vulkan`. There is no architecture field: SPIR-V is
    /// portable and driver-compiled.
    #[serde(default)]
    pub vulkan_api: Option<crate::arch::VulkanApi>,
}

/// Accept a bare value as well as a list of them: `accel = "cuda"` alongside
/// `accel = ["cuda", "rocm"]`, `cuda_sm = 90` alongside `cuda_sm = [86, 90]`.
///
/// Every manifest published so far uses the scalar form for `accel`, and
/// `backend.toml` is a pinned release asset the daemon re-reads on every scan,
/// so the scalar has to keep parsing indefinitely. A field written this way can
/// also *become* a list after the fact without a new generation, which is what
/// makes the one-value spelling safe to offer at all.
fn one_or_many<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(a) => vec![a],
        OneOrMany::Many(v) => v,
    })
}

impl SubprocessAsset {
    /// The release filenames composing this variant's archive: the single
    /// `file`, or the ordered `parts`. Exactly one source is populated once the
    /// manifest has passed [`Manifest::parse`].
    #[must_use]
    pub fn release_files(&self) -> Vec<&str> {
        match &self.file {
            Some(f) => vec![f.as_str()],
            None => self.parts.iter().map(String::as_str).collect(),
        }
    }

    /// Whether the archive is delivered as multiple concatenated parts.
    #[must_use]
    pub fn is_multipart(&self) -> bool {
        self.file.is_none()
    }

    /// A short label for diagnostics (the `file`, else the first part).
    #[must_use]
    pub fn label(&self) -> String {
        self.file
            .clone()
            .or_else(|| self.parts.first().cloned())
            .unwrap_or_else(|| "<unnamed subprocess asset>".into())
    }
}

/// Acceleration backend of a subprocess build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Accel {
    Cpu,
    Cuda,
    Metal,
    Rocm,
    Vulkan,
}

impl fmt::Display for Accel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cpu => write!(f, "cpu"),
            Self::Cuda => write!(f, "cuda"),
            Self::Metal => write!(f, "metal"),
            Self::Rocm => write!(f, "rocm"),
            Self::Vulkan => write!(f, "vulkan"),
        }
    }
}

/// One `[[secrets]]` declaration — an encrypted credential the backend reads
/// as an `x-tts-secret-<name>` request header.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Secret {
    /// `snake_case` identifier the backend reads the value by. Unique within
    /// the table.
    pub name: String,
    /// Human-readable label shown in the settings UI. Falls back to `name`
    /// when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// Help text shown beside the input in the settings UI.
    pub description: String,
    /// Whether a value must be set before the backend can load. Default
    /// `false`.
    #[serde(default)]
    pub required: bool,
}

/// One `[[options]]` declaration — non-secret configuration the backend reads
/// as an `x-tts-option-<name>` request header.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Opt {
    /// `snake_case` identifier the backend reads the value by. Unique within
    /// the table.
    pub name: String,
    /// Human-readable label shown in the settings UI. Falls back to `name`
    /// when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// Help text shown beside the input in the settings UI.
    pub description: String,
    /// Drives the input the UI renders. Default `string`.
    #[serde(default)]
    pub r#type: Option<OptionType>,
    /// Value used when the user sets none. Should match `type`.
    #[serde(default)]
    pub default: Option<OptionDefault>,
    /// The values this option accepts, when it accepts a closed set. Empty
    /// means any value of the declared `type`.
    ///
    /// Declaring them is what turns a free-text field into a dropdown, and
    /// what lets the daemon refuse a value the backend would not understand.
    /// An option whose allowed values were only ever written into its
    /// `description` accepted anything the user typed, and shipped it.
    #[serde(default)]
    pub choices: Vec<OptionDefault>,
    /// Whether a value must be set before the backend can load. Default
    /// `false`.
    #[serde(default)]
    pub required: bool,
}

impl Opt {
    /// Whether `value`, in the string form the daemon stores and injects, is
    /// one this option accepts. An option declaring no choices accepts
    /// anything, so this is the check itself, not a precondition for it.
    #[must_use]
    pub fn accepts(&self, value: &str) -> bool {
        self.choices.is_empty() || self.choices.iter().any(|c| c.to_string() == value)
    }
}

/// The input type of an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OptionType {
    String,
    Integer,
    Bool,
}

impl OptionType {
    /// The canonical lowercase string form (e.g. for JSON responses).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Bool => "bool",
        }
    }
}

impl fmt::Display for OptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An option's default value; matches the option's declared `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum OptionDefault {
    String(String),
    Integer(i64),
    Bool(bool),
}

impl fmt::Display for OptionDefault {
    /// The string form injected via `x-tts-option-*` headers and shown in the
    /// settings catalog: strings pass through unquoted; integers and bools
    /// use their plain display form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => write!(f, "{s}"),
            Self::Integer(i) => write!(f, "{i}"),
            Self::Bool(b) => write!(f, "{b}"),
        }
    }
}

/// One `[[models]]` entry. Each model is identified on the wire by
/// `(name, source)`, where `source` is `[backend].source`.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelEntry {
    /// Wire model name.
    pub name: String,
    /// Whether the model accepts more than one language. Default `true`.
    /// When `false`, `supported_languages` must be exactly
    /// `[primary_language]`.
    #[serde(default = "default_true")]
    pub multilingual: bool,
    /// Default language code (e.g. `en`); used when `language` is omitted.
    /// Must appear in `supported_languages`.
    pub primary_language: String,
    /// Language codes the model accepts; must include `primary_language`.
    pub supported_languages: Vec<String>,
    /// Devices the model can be loaded onto. The sentinel `none` (remote /
    /// online model with no local compute) must be the only entry when
    /// present. Non-empty.
    pub supported_devices: Vec<Device>,
    /// Conservative GPU memory estimate in bytes. Default `0`; use `0` for
    /// cloud models.
    #[serde(default)]
    pub estimated_vram_bytes: u64,
    /// Suggested minimum interval between streaming passes, in milliseconds.
    #[serde(default)]
    pub processing_interval_ms: Option<u64>,
    /// When `true`, the model is driven over the realtime WebSocket transport
    /// rather than batch `POST /v1/synthesize`. Requires
    /// `[capabilities] websocket = true`. Default `false`.
    #[serde(default)]
    pub realtime: bool,
    /// Files the model needs, each provisioned to its own `destination`
    /// before `POST /v1/load`. Cloud models declare none.
    #[serde(default)]
    pub files: Vec<FileSpec>,
    /// Longest `text` the model accepts in one `POST /v1/synthesize`. Absent
    /// means unbounded: the daemon sends whole utterances and never splits for
    /// length. When set, the daemon chunks on sentence boundaries to stay
    /// under it.
    #[serde(default)]
    pub max_input_chars: Option<u32>,
    /// Native output sample rate in Hz, e.g. `24000`.
    ///
    /// Advisory. The authoritative rate is the `x-tts-sample-rate` response
    /// header on each synthesis, which is what the daemon resamples from — a
    /// manifest cannot know what a cloud provider will actually return. This
    /// value exists so the settings UI can show an output rate and the daemon
    /// can pre-size buffers before the first response arrives.
    #[serde(default)]
    pub output_sample_rate: Option<u32>,
    /// Voice used when a synthesis request omits `voice`. Required when
    /// `voices` is non-empty; must name one of them.
    #[serde(default)]
    pub default_voice: Option<String>,
    /// Which kinds of `voice` id this model accepts. Default `["preset"]`.
    /// The daemon refuses an id shape the model did not opt into, so a backend
    /// never sees one it cannot resolve.
    #[serde(default = "default_voice_kinds")]
    pub voice_kinds: Vec<VoiceKind>,
    /// Longest reference audio accepted for a cloned voice, in seconds.
    /// Required when `voice_kinds` contains `cloned`, forbidden otherwise.
    #[serde(default)]
    pub clone_ref_seconds: Option<f32>,
    /// Whether registering a cloned voice requires the reference clip's
    /// transcript as well as its audio. Default `false`.
    ///
    /// In-context cloning conditions on the reference audio *and* the tokens
    /// of what it says, so a backend taking that path cannot build a voice
    /// from audio alone; speaker-embedding cloning needs no transcript and
    /// leaves this unset. The daemon refuses to register a transcript-less
    /// clip against a model that declares it, which turns a synthesis failure
    /// on every later request into one refusal at the point the voice is
    /// added. Only meaningful alongside `cloned` in `voice_kinds`.
    #[serde(default)]
    pub clone_needs_transcript: bool,
    /// Preset voices the model provides. Empty for models whose voices are
    /// entirely cloned or described.
    #[serde(default)]
    pub voices: Vec<VoiceEntry>,
    /// Compatibility shim; not part of model identity, which is
    /// `(name, source)`.
    ///
    /// `provider` used to be the third component of that key, and backends
    /// released against the earlier contract compare it against their own
    /// fixed value on `POST /v1/load` — answering `400 invalid_model` when it
    /// does not match. Dropping the field from the parser would make every
    /// such backend unloadable, so the value is kept solely to be echoed back
    /// on load; the daemon reads no meaning from it (`is_online` comes from
    /// `supported_devices`).
    ///
    /// It also has to stay in the generated schema: every published manifest
    /// declares the key, and a closed `ModelEntry` without it flags all of
    /// them as invalid in an editor bound to the schema.
    ///
    /// Delete the field once no supported backend validates the key.
    #[serde(default)]
    pub provider: Option<String>,
}

impl ModelEntry {
    /// Whether the model is served by a remote API with no local compute —
    /// encoded by the `none` sentinel in `supported_devices` (which validation
    /// requires to be the sole entry when present). This is the single source
    /// of the online/local distinction; the `provider` string is free-form and
    /// carries no such meaning.
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.supported_devices.contains(&Device::None)
    }
}

/// A device a model can be loaded onto.
///
/// Only two local answers exist, because `registry::compat` has already chosen
/// exactly one asset by the time this matters and that asset names its own
/// runtimes: run on the CPU, or run on the accelerator the installed build
/// targets. Which accelerator that is — CUDA, `ROCm`, Metal, Vulkan — is a
/// property of the asset, reported by `Accel`, not a choice made here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Device {
    Cpu,
    Gpu,
    /// Sentinel for remote/online models with no local compute; must be the
    /// only entry when present.
    None,
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cpu => write!(f, "cpu"),
            Self::Gpu => write!(f, "gpu"),
            Self::None => write!(f, "none"),
        }
    }
}

impl std::str::FromStr for Device {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cpu" => Ok(Self::Cpu),
            // `cuda` and `metal` are deprecated input spellings. They are
            // accepted because `backend.toml` is a pinned release asset and
            // published `index.json` files carry them, so a manifest written
            // before this vocabulary must keep loading. `Display` never emits
            // them, so nothing new can come to depend on them.
            "gpu" | "cuda" | "metal" => Ok(Self::Gpu),
            "none" => Ok(Self::None),
            _ => Err(format!("Unknown device: {s}")),
        }
    }
}

/// Routed through `FromStr` so the deprecated spellings are accepted wherever
/// a device is deserialized — TOML manifests and JSON index entries alike.
impl<'de> Deserialize<'de> for Device {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A kind of `voice` id a model accepts on `POST /v1/synthesize`.
///
/// The kind is carried by the id's shape, so the daemon can route without
/// asking the backend: a bare id is a preset, `voice:<uuid>` is a cloned voice
/// the daemon holds reference audio for, and `desc:<text>` is a free-text
/// description for models that can synthesize a voice from one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VoiceKind {
    /// A voice declared in `[[models.voices]]`, named by a bare id.
    Preset,
    /// A user-cloned voice, `voice:<uuid>`. The daemon pushes reference audio
    /// on the request; requires `clone_ref_seconds`.
    Cloned,
    /// A free-text voice description, `desc:<text>`.
    Described,
}

impl fmt::Display for VoiceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preset => write!(f, "preset"),
            Self::Cloned => write!(f, "cloned"),
            Self::Described => write!(f, "described"),
        }
    }
}

/// One `[[models.voices]]` entry: a preset voice the backend provides.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VoiceEntry {
    /// The `voice` id sent on `POST /v1/synthesize`. Unique within the model.
    pub id: String,
    /// Display name for the voice picker. Falls back to `id` when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// Primary language of the voice. Must be one of the model's
    /// `supported_languages` when set.
    #[serde(default)]
    pub language: Option<String>,
    /// Free-form tags for filtering in the picker, e.g. `["warm", "female"]`.
    /// Not interpreted by the daemon.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl VoiceEntry {
    /// Display name: `label` when set, otherwise the bare `id`.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

/// One `[[models.files]]` entry: a single file to download and where to put it.
///
/// Source-agnostic — a file is just a URL, fetched the same way regardless of
/// host. Hugging Face is reached by writing its plain resolve URL, with no
/// special treatment.
///
/// From [`Contract::V2`] an entry may also carry a **host selector** —
/// `accel`, `cuda_major`, `cuda_sm`, `gfx`, `vulkan_api` — written in the same
/// vocabulary [`SubprocessAsset`] uses for a build. Entries sharing a
/// `destination` are then variants of one file: the daemon scores each against
/// the host exactly as it scores build variants, downloads the best match, and
/// leaves the rest alone. That is what lets a model publish per-architecture
/// weights, or a kernel cache compiled for one GPU, without a separate build of
/// the backend to carry them.
///
/// An entry with no selector matches every host — which is what every entry in
/// every v1 manifest is, and why the selector could be added without changing
/// what any of them mean.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileSpec {
    /// Full download URL for this file, e.g.
    /// `https://huggingface.co/openai/kokoro-tiny/resolve/main/config.json`.
    pub url: String,
    /// Relative file path (including filename) under the backend directory to
    /// write the download to, e.g. `models/kokoro-tiny/config.json`.
    /// Validated as a safe relative path so it cannot escape the backend dir.
    ///
    /// Also the variant key: every entry writing to one `destination` is a
    /// candidate for it, and exactly one of them is downloaded. The backend
    /// therefore reads a fixed path and never learns which variant it got.
    pub destination: String,
    /// Expected SHA-256 of the file, hex-encoded, for integrity verification.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Acceleration families this variant is for. Empty — the default — is an
    /// unconditional file that matches every host.
    ///
    /// Every field of a file's selector is optional, which is the one place
    /// this vocabulary is looser than an asset's. An asset has to *run* on the
    /// host, so `[[assets.subprocess]]` requires `cuda_major` alongside `cuda`
    /// and `gfx` alongside `rocm`; a file is data whose meaning belongs to the
    /// backend, so `accel = "cuda"` on its own is a legitimate "for any CUDA
    /// host". Naming a narrower variant as well is how an author gets both: a
    /// variant that matches the host's compute capability exactly outranks one
    /// that only matches its family.
    #[serde(default, deserialize_with = "one_or_many")]
    pub accel: Vec<Accel>,
    /// Highest CUDA major version this variant needs — it matches a host whose
    /// installed CUDA runtime is at least this. Allowed only with `cuda` in
    /// `accel`; omit to match any CUDA runtime.
    #[serde(default)]
    pub cuda_major: Option<u32>,
    /// Compute capabilities this variant covers (e.g. `90`, or `[86, 90]`).
    /// Allowed only with `cuda` in `accel`; empty matches any. A variant that
    /// names the host's capability is preferred over one that does not.
    ///
    /// A list where an asset's `cuda_sm` is a single value, because the two are
    /// answering different questions. A build that omits it is a fat binary
    /// with PTX behind it, so "any capability" is a true claim and enumerating
    /// is rarely useful. A file has no JIT to fall back on — kernels compiled
    /// for `sm_90` are inert on `sm_86` — but one file may still carry entries
    /// for several devices, which is exactly what a pre-warmed kernel cache is.
    /// Saying so once beats declaring the same URL and hash under each.
    #[serde(default, deserialize_with = "one_or_many")]
    pub cuda_sm: Vec<u32>,
    /// AMD architecture targets this variant is built for, in `--offload-arch`
    /// spelling. Allowed only with `rocm` in `accel`; omit to match any AMD
    /// host.
    #[serde(default)]
    pub gfx: Vec<crate::arch::GfxSpec>,
    /// Minimum Vulkan API version a host needs to use this variant. Allowed
    /// only with `vulkan` in `accel`.
    #[serde(default)]
    pub vulkan_api: Option<crate::arch::VulkanApi>,
    /// Whether the model can load without this file. Default `false`.
    ///
    /// A destination none of whose variants match the host is a hole, and the
    /// two kinds of hole want opposite handling. Weights are load-bearing: the
    /// load fails, naming what the host offered and what the variants wanted,
    /// which beats a backend erroring on a file it was never given. A
    /// pre-warmed kernel cache is not: the backend compiles its own when the
    /// file is absent, so an unlisted GPU should still load, just slower.
    /// `optional = true` is that second case.
    ///
    /// It describes the destination rather than the entry, so the variants of
    /// one destination must agree on it.
    #[serde(default)]
    pub optional: bool,
}

impl FileSpec {
    /// Whether this entry names any host requirement at all. An entry that
    /// does not is the v1 shape: it matches every host, and is the fallback
    /// when it shares a destination with variants that do.
    #[must_use]
    pub fn is_conditional(&self) -> bool {
        !self.accel.is_empty()
    }
}

fn default_true() -> bool {
    true
}

/// A model that says nothing about voices accepts preset ids only — the
/// conservative reading, since cloning and description both need capability
/// the backend has to have opted into.
fn default_voice_kinds() -> Vec<VoiceKind> {
    vec![VoiceKind::Preset]
}

/// Errors from reading/parsing a `backend.toml`.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// An I/O error reading the file.
    #[error("read {path}")]
    Io {
        /// Path that failed to read.
        path: String,
        /// Underlying I/O error.
        #[source]
        err: std::io::Error,
    },
    /// A parse error, annotated with the file path.
    #[error("parse {path}")]
    Parse {
        /// Path of the file that failed to parse.
        path: String,
        #[source]
        err: Box<ManifestError>,
    },
    /// A TOML parse error.
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
    /// The `entrypoint` field is not a safe relative path.
    #[error("backend.toml entrypoint {0:?} is not a safe relative path")]
    UnsafeEntrypoint(String),
    /// `[backend].id` is present but not a well-formed reverse-DNS id.
    #[error("`[backend].id` is not a valid reverse-DNS id: {0}")]
    InvalidId(String),
    /// A `[[models.files]]` `destination` is not a safe relative path.
    #[error("backend.toml file destination {0:?} is not a safe relative path")]
    UnsafeDestination(String),
    /// A `[[assets.subprocess]]` entry set neither or both of `file`/`parts`.
    #[error(
        "backend.toml subprocess asset for target {0:?} must set exactly one of \
         `file` or `parts`"
    )]
    AssetFileXorParts(String),
    /// A `[[assets.subprocess]]` entry declared an empty `accel` list.
    #[error("asset `{file}` declares an empty `accel` list")]
    AccelEmpty {
        /// The asset's label (its `file`, or its first `parts` entry).
        file: String,
    },
    /// A `[[assets.subprocess]]` entry declared `accel = "rocm"` with no `gfx`.
    #[error("asset `{file}` declares `accel = rocm` but no `gfx` targets")]
    RocmMissingGfx {
        /// The asset's label (its `file`, or its first `parts` entry).
        file: String,
    },
    /// A `[[assets.subprocess]]` entry declared `gfx` without `rocm` in `accel`.
    #[error("asset `{file}` declares `gfx` without `accel = rocm`")]
    GfxRequiresRocm {
        /// The asset's label (its `file`, or its first `parts` entry).
        file: String,
    },
    /// A `[[assets.subprocess]]` entry declared `vulkan_api` without `vulkan`
    /// in `accel`.
    #[error("asset `{file}` declares `vulkan_api` without `accel = vulkan`")]
    VulkanApiRequiresVulkan {
        /// The asset's label (its `file`, or its first `parts` entry).
        file: String,
    },
    /// A `[[assets.subprocess]]` entry declared `accel` containing `cuda` but
    /// no `cuda_major`.
    #[error("asset `{file}` declares `accel` containing `cuda` but no `cuda_major`")]
    CudaMissingMajor {
        /// The asset's label (its `file`, or its first `parts` entry).
        file: String,
    },
    /// A `[[assets.subprocess]]` entry declared `cuda_major`/`cuda_sm` without
    /// `cuda` in `accel`.
    #[error("asset `{file}` declares `cuda_major`/`cuda_sm` without `accel` containing `cuda`")]
    CudaForbiddenFields {
        /// The asset's label (its `file`, or its first `parts` entry).
        file: String,
    },
    /// A `[[assets.subprocess]]` entry declared `cudnn = true` without `cuda`
    /// in `accel`.
    #[error("asset `{file}` declares `cudnn = true` without `accel` containing `cuda`")]
    CudnnRequiresCuda {
        /// The asset's label (its `file`, or its first `parts` entry).
        file: String,
    },
    /// A `[[models.files]]` entry declared `cuda_major`/`cuda_sm` without
    /// `cuda` in `accel`.
    #[error(
        "model `{model}` file `{destination}` declares `cuda_major`/`cuda_sm` \
         without `accel` containing `cuda`"
    )]
    FileCudaForbiddenFields {
        /// The model declaring the file.
        model: String,
        /// The file's `destination`.
        destination: String,
    },
    /// A `[[models.files]]` entry declared `gfx` without `rocm` in `accel`.
    #[error("model `{model}` file `{destination}` declares `gfx` without `accel = rocm`")]
    FileGfxRequiresRocm {
        /// The model declaring the file.
        model: String,
        /// The file's `destination`.
        destination: String,
    },
    /// A `[[models.files]]` entry declared `vulkan_api` without `vulkan` in
    /// `accel`.
    #[error("model `{model}` file `{destination}` declares `vulkan_api` without `accel = vulkan`")]
    FileVulkanApiRequiresVulkan {
        /// The model declaring the file.
        model: String,
        /// The file's `destination`.
        destination: String,
    },
    /// Variants of one `destination` disagreed on `optional`.
    ///
    /// `optional` answers "may this destination end up empty?", which is a
    /// property of the destination and not of whichever variant happened to
    /// win. Two answers to one question would make the outcome depend on the
    /// host, so the manifest has to settle it.
    #[error("model `{model}` declares `{destination}` with variants that disagree on `optional`")]
    FileVariantsDisagreeOnOptional {
        /// The model declaring the file.
        model: String,
        /// The destination whose variants disagree.
        destination: String,
    },
    /// Two `[[models.voices]]` under one model share an `id`.
    #[error("model `{model}` declares voice id `{id}` more than once")]
    DuplicateVoiceId {
        /// The model declaring the duplicate.
        model: String,
        /// The repeated voice id.
        id: String,
    },
    /// A model declared `[[models.voices]]` but no `default_voice`.
    #[error("model `{model}` declares voices but no `default_voice`")]
    MissingDefaultVoice {
        /// The model missing the default.
        model: String,
    },
    /// `default_voice` does not name any declared voice.
    #[error("model `{model}` sets `default_voice = {voice:?}`, which is not a declared voice")]
    UnknownDefaultVoice {
        /// The model with the dangling default.
        model: String,
        /// The value that matched no voice.
        voice: String,
    },
    /// A voice's `language` is not in the model's `supported_languages`.
    #[error(
        "model `{model}` voice `{id}` declares language `{language}`, \
         which is not in `supported_languages`"
    )]
    VoiceLanguageUnsupported {
        /// The model declaring the voice.
        model: String,
        /// The voice id.
        id: String,
        /// The unsupported language tag.
        language: String,
    },
    /// `voice_kinds` was declared empty; a model must accept at least one kind.
    #[error("model `{model}` declares an empty `voice_kinds`")]
    VoiceKindsEmpty {
        /// The model with the empty list.
        model: String,
    },
    /// `voice_kinds` contains `cloned` but `clone_ref_seconds` is absent.
    #[error("model `{model}` accepts cloned voices but declares no `clone_ref_seconds`")]
    CloneMissingRefSeconds {
        /// The model missing the bound.
        model: String,
    },
    /// `clone_ref_seconds` was declared without `cloned` in `voice_kinds`.
    #[error("model `{model}` declares `clone_ref_seconds` without `cloned` in `voice_kinds`")]
    CloneRefSecondsRequiresCloned {
        /// The model with the stray bound.
        model: String,
    },
    /// `clone_ref_seconds` is not a positive, finite number of seconds.
    #[error("model `{model}` declares a non-positive `clone_ref_seconds`")]
    CloneRefSecondsInvalid {
        /// The model with the bad bound.
        model: String,
    },
    /// `clone_needs_transcript` was declared without `cloned` in `voice_kinds`.
    #[error("model `{model}` declares `clone_needs_transcript` without `cloned` in `voice_kinds`")]
    CloneTranscriptRequiresCloned {
        /// The model with the stray flag.
        model: String,
    },
    /// `max_input_chars` was declared as zero, which would admit no text.
    #[error("model `{model}` declares `max_input_chars = 0`")]
    MaxInputCharsZero {
        /// The model with the unusable bound.
        model: String,
    },
    /// `output_sample_rate` is outside the range any audio device serves.
    #[error("model `{model}` declares `output_sample_rate = {rate}`, outside 8000..=192000")]
    OutputSampleRateOutOfRange {
        /// The model with the implausible rate.
        model: String,
        /// The declared rate.
        rate: u32,
    },
    /// `[capabilities] streaming_input` is reserved and not yet implemented.
    #[error("`[capabilities] streaming_input` is reserved and not yet supported")]
    StreamingInputReserved,
    /// A field was declared that the manifest's `contract` does not include.
    ///
    /// Both fixes are named because they are not equivalent: raising the
    /// contract is right for a manifest that means to use the field, but it
    /// also raises the release floor for every client. An author who wrote the
    /// field's default value by hand wants the other one.
    #[error(
        "`{field}` requires `contract = \"{since}\"`, but this manifest declares \
         `contract = \"{declared}\"` — raise the contract to use the field, or \
         remove the field to stay on `{declared}`"
    )]
    FieldRequiresContract {
        /// The field, spelled as in the manifest (e.g. `[[models]].role`).
        field: String,
        /// The generation that introduced it.
        since: Contract,
        /// The generation the manifest declares.
        declared: Contract,
    },
    /// A field the manifest's `contract` requires was not declared.
    #[error(
        "`contract = \"{declared}\"` requires `{field}`, which this manifest does not \
         declare — add it, or drop to `contract = \"{previous}\"` where it is optional"
    )]
    FieldRequiredByContract {
        /// The field, spelled as in the manifest (e.g. `[backend].id`).
        field: String,
        /// The generation the manifest declares.
        declared: Contract,
        /// The newest generation that does not require it.
        previous: Contract,
    },
}

/// Whether [`Manifest::parse_inner`] holds the document to its declared
/// contract's field rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContractFields {
    /// A manifest being admitted: the rules apply.
    Enforced,
    /// A manifest already installed here: they do not. See
    /// [`Manifest::parse_installed`].
    Ignored,
}

/// Whether any [`CONTRACT_FIELDS`] rule can bite at `declared`. When none can
/// — which for a manifest declaring the latest generation with no required
/// fields is the common case — the raw document never has to be parsed.
fn applies_to(declared: Contract) -> bool {
    CONTRACT_FIELDS.iter().any(|field| match field.rule {
        FieldRule::Added => field.since > declared,
        FieldRule::RequiredFrom => declared >= field.since,
    })
}

/// Whether the raw document declares a field, anywhere its table allows it.
///
/// Reads the raw document rather than the typed struct so an explicitly
/// written default still counts as declared, and an absent field is absent
/// rather than defaulted. A table that is an array (`[[models]]`) counts a
/// field declared if *any* entry declares it; a plain table (`[backend]`) is
/// checked once.
fn declares(raw: &toml::Table, field: &ContractField) -> bool {
    /// Descend `path` from `table`, then report whether the table it lands in
    /// declares `key`. An array segment is satisfied by *any* of its entries,
    /// which is what makes one `[[models.files]]` entry anywhere in the
    /// document count as a declaration.
    fn walk(table: &toml::Table, path: &[(&str, bool)], key: &str) -> bool {
        let Some(((segment, is_array), rest)) = path.split_first() else {
            return table.contains_key(key);
        };
        match table.get(*segment) {
            Some(toml::Value::Array(entries)) if *is_array => entries
                .iter()
                .filter_map(toml::Value::as_table)
                .any(|entry| walk(entry, rest, key)),
            Some(toml::Value::Table(inner)) if !*is_array => walk(inner, rest, key),
            _ => false,
        }
    }
    walk(raw, &field.segments(), field.key)
}

/// The first [`CONTRACT_FIELDS`] rule the document breaks, in table order, as
/// the error it should be reported as. `None` when the manifest stays within
/// its contract.
fn contract_violation(raw: &toml::Table, declared: Contract) -> Option<ManifestError> {
    CONTRACT_FIELDS.iter().find_map(|field| match field.rule {
        // A field from a later generation, spelled under an earlier one.
        FieldRule::Added if field.since > declared && declares(raw, field) => {
            Some(ManifestError::FieldRequiresContract {
                field: field.path(),
                since: field.since,
                declared,
            })
        }
        // A field this generation requires, left out.
        FieldRule::RequiredFrom if declared >= field.since && !declares(raw, field) => {
            Some(ManifestError::FieldRequiredByContract {
                field: field.path(),
                declared,
                previous: field.since.previous().unwrap_or(field.since),
            })
        }
        _ => None,
    })
}

impl Manifest {
    /// Parse a `backend.toml` from its text.
    ///
    /// The entrypoint is joined onto the backend dir to spawn/load the
    /// backend; an absolute or traversing value would escape it. The guard
    /// lives in the single canonical parser so every consumer inherits it.
    ///
    /// # Errors
    /// Returns a [`ManifestError`] on TOML errors, an unsafe entrypoint or
    /// file destination, a malformed `[backend].id`, a malformed
    /// `[[assets.subprocess]]` entry, or a field the declared
    /// [`contract`](Contract) does not include
    /// ([`FieldRequiresContract`](ManifestError::FieldRequiresContract)).
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        Self::parse_inner(text, ContractFields::Enforced)
    }

    /// Parse a manifest that is **already installed** on this machine.
    ///
    /// Identical to [`parse`](Self::parse) except that the contract-field rule
    /// does not apply. That rule's job is to stop a manifest getting in, and
    /// this one is already in: enforcing it at discovery would make a backend
    /// that installed cleanly under an earlier build disappear from the
    /// catalog — taking its downloaded models out of reach — over a manifest
    /// the user cannot edit and did not write.
    ///
    /// Only discovery of the installed backends directory uses this. Every
    /// path that admits a *new* manifest — registry install, custom repo,
    /// import-from-directory, and the indexer — goes through
    /// [`parse`](Self::parse).
    ///
    /// # Errors
    /// As [`parse`](Self::parse), less `FieldRequiresContract`.
    pub fn parse_installed(text: &str) -> Result<Self, ManifestError> {
        Self::parse_inner(text, ContractFields::Ignored)
    }

    fn parse_inner(text: &str, fields: ContractFields) -> Result<Self, ManifestError> {
        let mut m: Self = toml::from_str(text)?;
        // Every field defaults when absent, so the typed struct cannot tell
        // "declared the field" from "left it out". The raw document can, and
        // the rule is about what was written: an older manifest may not spell
        // a newer generation's field at all, not even with its default value,
        // because that generation's schema does not have it. Parsed from the
        // text a second time rather than converted from one `Table`, so the
        // typed parse above keeps its line/column spans in error messages —
        // and only when some field could actually be in breach, which for a
        // manifest declaring the latest generation is never.
        if fields == ContractFields::Enforced && applies_to(m.backend.contract) {
            let raw: toml::Table = toml::from_str(text)?;
            if let Some(violation) = contract_violation(&raw, m.backend.contract) {
                return Err(violation);
            }
        }
        if !crate::is_safe_relative_path(&m.backend.entrypoint) {
            return Err(ManifestError::UnsafeEntrypoint(m.backend.entrypoint));
        }
        // Validated in the canonical parser so the daemon (which joins it onto
        // the backends dir) and the indexer (which pins it against
        // registry.toml) inherit one definition.
        if let Some(id) = &m.backend.id
            && !crate::backend_id::is_valid(id)
        {
            return Err(ManifestError::InvalidId(id.clone()));
        }
        // Reserved capability: refuse rather than ignore, so an author who
        // declares it learns the path does not exist instead of shipping a
        // backend that silently never receives incremental text.
        if m.capabilities.streaming_input {
            return Err(ManifestError::StreamingInputReserved);
        }
        for model in &m.models {
            Self::validate_files(model)?;
            Self::validate_voices(model)?;
        }
        // A subprocess build variant names its archive with exactly one of
        // `file` (single) or `parts` (split across release assets, concatenated
        // in order). The guard lives in the canonical parser so the daemon and
        // the indexer agree on the contract.
        for a in &mut m.assets.subprocess {
            // Normalize an empty `file` to `None` so the XOR check and the
            // downstream `release_files()` / `is_multipart()` all agree that
            // `parts` is the source. Without this, `file = ""` plus valid `parts`
            // passed parse but `release_files()` then returned `[""]` and
            // `is_multipart()` was false (Tier 1 #25).
            if a.file.as_deref().is_some_and(str::is_empty) {
                a.file = None;
            }
            let has_file = a.file.is_some();
            let has_parts = !a.parts.is_empty() && a.parts.iter().all(|p| !p.is_empty());
            if has_file == has_parts {
                return Err(ManifestError::AssetFileXorParts(a.target.clone()));
            }
            if a.accel.is_empty() {
                return Err(ManifestError::AccelEmpty { file: a.label() });
            }
            let has = |k: Accel| a.accel.contains(&k);
            if has(Accel::Cuda) {
                // `cuda_sm` stays optional: omitted means the build matches any
                // compute capability (multi-architecture framework builds).
                if a.cuda_major.is_none() {
                    return Err(ManifestError::CudaMissingMajor { file: a.label() });
                }
            } else {
                if a.cuda_major.is_some() || a.cuda_sm.is_some() {
                    return Err(ManifestError::CudaForbiddenFields { file: a.label() });
                }
                if a.cudnn {
                    return Err(ManifestError::CudnnRequiresCuda { file: a.label() });
                }
            }
            if has(Accel::Rocm) {
                if a.gfx.is_empty() {
                    return Err(ManifestError::RocmMissingGfx { file: a.label() });
                }
            } else if !a.gfx.is_empty() {
                return Err(ManifestError::GfxRequiresRocm { file: a.label() });
            }
            if !has(Accel::Vulkan) && a.vulkan_api.is_some() {
                return Err(ManifestError::VulkanApiRequiresVulkan { file: a.label() });
            }
        }
        Ok(m)
    }

    /// Path safety and selector coherence for one model's `[[models.files]]`.
    ///
    /// The cross-field rules mirror `[[assets.subprocess]]`, minus the two
    /// requirements a data file has no basis for: a file may declare `cuda`
    /// without a `cuda_major` and `rocm` without a `gfx`, because "any CUDA
    /// host" and "any AMD host" are things a file can honestly mean and a
    /// binary cannot. What is refused is the same in both places — a
    /// discriminator naming a family the entry never declared, which is a typo
    /// that would otherwise select nothing and say nothing.
    fn validate_files(model: &ModelEntry) -> Result<(), ManifestError> {
        let name = || model.name.clone();
        // Destination -> the `optional` its first variant declared.
        let mut optional_by_destination: std::collections::HashMap<&str, bool> =
            std::collections::HashMap::new();

        for file in &model.files {
            // Joined onto the backend dir before the daemon writes the
            // download; reject any value that would escape it. The guard lives
            // in the canonical parser so every consumer inherits it.
            if !crate::is_safe_relative_path(&file.destination) {
                return Err(ManifestError::UnsafeDestination(file.destination.clone()));
            }
            let destination = || file.destination.clone();
            let declares = |k: Accel| file.accel.contains(&k);
            if !declares(Accel::Cuda) && (file.cuda_major.is_some() || !file.cuda_sm.is_empty()) {
                return Err(ManifestError::FileCudaForbiddenFields {
                    model: name(),
                    destination: destination(),
                });
            }
            if !declares(Accel::Rocm) && !file.gfx.is_empty() {
                return Err(ManifestError::FileGfxRequiresRocm {
                    model: name(),
                    destination: destination(),
                });
            }
            if !declares(Accel::Vulkan) && file.vulkan_api.is_some() {
                return Err(ManifestError::FileVulkanApiRequiresVulkan {
                    model: name(),
                    destination: destination(),
                });
            }
            match optional_by_destination.insert(file.destination.as_str(), file.optional) {
                Some(first) if first != file.optional => {
                    return Err(ManifestError::FileVariantsDisagreeOnOptional {
                        model: name(),
                        destination: destination(),
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Voice and synthesis-bound coherence for one `[[models]]` entry.
    ///
    /// Kept in the canonical parser alongside the path and asset guards, so
    /// the daemon and the indexer reject the same manifests — a voice the
    /// picker can offer but the backend cannot resolve is a runtime failure
    /// the user sees as a dead entry in a dropdown.
    fn validate_voices(model: &ModelEntry) -> Result<(), ManifestError> {
        let name = || model.name.clone();

        if model.voice_kinds.is_empty() {
            return Err(ManifestError::VoiceKindsEmpty { model: name() });
        }

        let mut seen = std::collections::BTreeSet::new();
        for voice in &model.voices {
            if !seen.insert(voice.id.as_str()) {
                return Err(ManifestError::DuplicateVoiceId {
                    model: name(),
                    id: voice.id.clone(),
                });
            }
            // A voice pinned to a language the model does not accept can never
            // be selected successfully, so it is a manifest bug, not a runtime
            // one.
            if let Some(lang) = &voice.language
                && !model.supported_languages.contains(lang)
            {
                return Err(ManifestError::VoiceLanguageUnsupported {
                    model: name(),
                    id: voice.id.clone(),
                    language: lang.clone(),
                });
            }
        }

        match &model.default_voice {
            None if !model.voices.is_empty() => {
                return Err(ManifestError::MissingDefaultVoice { model: name() });
            }
            // A default naming no declared voice is only detectable here: the
            // backend would answer `unsupported_voice` for every request that
            // omitted a voice, which reads as "the model is broken".
            Some(v) if !seen.contains(v.as_str()) => {
                return Err(ManifestError::UnknownDefaultVoice {
                    model: name(),
                    voice: v.clone(),
                });
            }
            _ => {}
        }

        let clones = model.voice_kinds.contains(&VoiceKind::Cloned);
        match model.clone_ref_seconds {
            None if clones => {
                return Err(ManifestError::CloneMissingRefSeconds { model: name() });
            }
            Some(_) if !clones => {
                return Err(ManifestError::CloneRefSecondsRequiresCloned { model: name() });
            }
            Some(s) if !(s.is_finite() && s > 0.0) => {
                return Err(ManifestError::CloneRefSecondsInvalid { model: name() });
            }
            _ => {}
        }
        if model.clone_needs_transcript && !clones {
            return Err(ManifestError::CloneTranscriptRequiresCloned { model: name() });
        }

        if model.max_input_chars == Some(0) {
            return Err(ManifestError::MaxInputCharsZero { model: name() });
        }

        if let Some(rate) = model.output_sample_rate
            && !(8000..=192_000).contains(&rate)
        {
            return Err(ManifestError::OutputSampleRateOutOfRange {
                model: name(),
                rate,
            });
        }

        Ok(())
    }

    /// Read and parse `<dir>/backend.toml`.
    ///
    /// # Errors
    /// Returns a [`ManifestError`] if the file is missing, unreadable, or
    /// fails [`Manifest::parse`].
    pub fn load(dir: &Path) -> Result<Self, ManifestError> {
        Self::load_with(dir, Self::parse)
    }

    /// [`load`](Self::load) for a backend already installed here — see
    /// [`parse_installed`](Self::parse_installed) for why the contract-field
    /// rule is not applied.
    ///
    /// # Errors
    /// As [`load`](Self::load), less `FieldRequiresContract`.
    pub fn load_installed(dir: &Path) -> Result<Self, ManifestError> {
        Self::load_with(dir, Self::parse_installed)
    }

    /// Read `<dir>/backend.toml` and hand its text to `parse`.
    fn load_with(
        dir: &Path,
        parse: fn(&str) -> Result<Self, ManifestError>,
    ) -> Result<Self, ManifestError> {
        let path = dir.join("backend.toml");
        let text = std::fs::read_to_string(&path).map_err(|err| ManifestError::Io {
            path: path.display().to_string(),
            err,
        })?;
        parse(&text).map_err(|err| ManifestError::Parse {
            path: path.display().to_string(),
            err: Box::new(err),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal manifest with no `[backend].id`, for tests that only care
    /// about behavior around the field.
    const VALID: &str = r#"
        [backend]
        source = "github.com/x/y"
        name = "Y"
        version = "1.0.0"
        kind = "wasm"
        entrypoint = "y.wasm"
        contract = "v1"
        description = "Test backend."
        "#;

    /// Generations are ordered, and each names the release that first
    /// understood it.
    ///
    /// The order is what every `CONTRACT_FIELDS` rule is evaluated against, and
    /// it comes from variant declaration order alone — a generation inserted in
    /// the wrong place would silently invert every rule that mentions it.
    /// `min_client` is a forecast written before the release it names is
    /// tagged, so the only thing that can be checked here is that every
    /// generation has one and that it parses as a version.
    #[test]
    fn contracts_order_by_generation_and_each_names_its_floor() {
        assert_eq!(
            Contract::ALL.last().copied(),
            Some(Contract::LATEST),
            "LATEST must be the newest generation in ALL"
        );
        assert!(
            Contract::ALL.windows(2).all(|pair| pair[0] < pair[1]),
            "ALL must be in generation order; the derived Ord follows it"
        );
        assert_eq!(Contract::V1.previous(), None, "v1 is the first generation");
        for contract in Contract::ALL {
            assert!(
                !contract.min_client().is_empty(),
                "{contract} names no client floor"
            );
        }
    }

    /// A manifest from a generation this build does not know is refused, and
    /// the refusal says what this build *does* know.
    ///
    /// This is the whole gating mechanism: a daemon predating a generation
    /// cannot parse a manifest declaring it, so it cannot install a backend it
    /// could not drive. Serde's own "unknown variant" message would name Rust
    /// variants (`V1`), not the spelling a manifest author wrote, which is why
    /// the deserializer is routed through `FromStr`.
    #[test]
    fn a_contract_this_build_does_not_know_is_refused_by_name() {
        let text = VALID.replace(r#"contract = "v1""#, r#"contract = "v99""#);
        let err = Manifest::parse(&text).expect_err("an unknown generation must not parse");
        let message = err.to_string();
        assert!(
            message.contains("v99") && message.contains("v1"),
            "the error must name both the unknown generation and the known ones: {message}"
        );
    }

    /// The contract-field rule applies to a manifest being admitted and not to
    /// one already installed.
    ///
    /// Both parsers exist so an installed backend never disappears from the
    /// catalog over a rule tightened after it was installed. The divergence
    /// itself is exercised by `a_file_selector_requires_contract_v2`; what
    /// this pins is that `parse_installed` is a real second entry point which
    /// still runs every *other* guard, rather than a way around all of them.
    #[test]
    fn an_installed_manifest_skips_only_the_contract_field_rule() {
        Manifest::parse(VALID).expect("a well-formed manifest parses");
        Manifest::parse_installed(VALID).expect("and so does an installed one");

        // Every non-contract guard still runs on the installed path.
        let unsafe_entrypoint =
            VALID.replace(r#"entrypoint = "y.wasm""#, r#"entrypoint = "/y.wasm""#);
        assert!(
            Manifest::parse_installed(&unsafe_entrypoint).is_err(),
            "parse_installed must still refuse an entrypoint that escapes the backend dir"
        );
    }

    /// `VALID` plus one `[[models]]` entry, so the voice rules have something
    /// to attach to. `body` is appended inside the model table.
    fn with_model(body: &str) -> String {
        format!(
            "{VALID}
            [[models]]
            name = \"m\"
            primary_language = \"en\"
            supported_languages = [\"en\", \"es\"]
            supported_devices = [\"cpu\"]
            {body}
            "
        )
    }

    #[test]
    fn a_model_declaring_no_voice_fields_accepts_presets_only() {
        let m = Manifest::parse(&with_model("")).expect("voice fields are all optional");
        let model = &m.models[0];
        assert_eq!(model.voice_kinds, vec![VoiceKind::Preset]);
        assert!(model.voices.is_empty());
        assert!(model.default_voice.is_none());
        assert!(model.max_input_chars.is_none());
        assert!(model.output_sample_rate.is_none());
    }

    #[test]
    fn parses_preset_voices_with_a_default() {
        let m = Manifest::parse(&with_model(
            r#"
            default_voice = "bella"
            output_sample_rate = 24000
            max_input_chars = 500
            voices = [
                { id = "bella", label = "Bella", language = "en", tags = ["warm"] },
                { id = "nova" },
            ]
            "#,
        ))
        .expect("a well-formed voice table parses");
        let model = &m.models[0];
        assert_eq!(model.voices.len(), 2);
        assert_eq!(model.voices[0].display_name(), "Bella");
        // `label` absent falls back to the bare id.
        assert_eq!(model.voices[1].display_name(), "nova");
        assert_eq!(model.default_voice.as_deref(), Some("bella"));
        assert_eq!(model.max_input_chars, Some(500));
        assert_eq!(model.output_sample_rate, Some(24000));
    }

    #[test]
    fn rejects_duplicate_voice_ids() {
        let err = Manifest::parse(&with_model(
            r#"
            default_voice = "bella"
            voices = [{ id = "bella" }, { id = "bella" }]
            "#,
        ))
        .unwrap_err();
        assert!(matches!(err, ManifestError::DuplicateVoiceId { .. }));
    }

    #[test]
    fn rejects_voices_without_a_default() {
        let err = Manifest::parse(&with_model(r#"voices = [{ id = "bella" }]"#)).unwrap_err();
        assert!(matches!(err, ManifestError::MissingDefaultVoice { .. }));
    }

    #[test]
    fn rejects_a_default_voice_that_names_nothing() {
        let err = Manifest::parse(&with_model(
            r#"
            default_voice = "ghost"
            voices = [{ id = "bella" }]
            "#,
        ))
        .unwrap_err();
        assert!(matches!(err, ManifestError::UnknownDefaultVoice { .. }));
    }

    #[test]
    fn rejects_a_voice_language_the_model_does_not_support() {
        let err = Manifest::parse(&with_model(
            r#"
            default_voice = "bella"
            voices = [{ id = "bella", language = "ja" }]
            "#,
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ManifestError::VoiceLanguageUnsupported { .. }
        ));
    }

    #[test]
    fn rejects_an_empty_voice_kinds_list() {
        let err = Manifest::parse(&with_model("voice_kinds = []")).unwrap_err();
        assert!(matches!(err, ManifestError::VoiceKindsEmpty { .. }));
    }

    #[test]
    fn cloning_requires_a_reference_length_bound() {
        let err = Manifest::parse(&with_model(r#"voice_kinds = ["cloned"]"#)).unwrap_err();
        assert!(matches!(err, ManifestError::CloneMissingRefSeconds { .. }));

        let m = Manifest::parse(&with_model(
            r#"
            voice_kinds = ["preset", "cloned"]
            clone_ref_seconds = 12.0
            "#,
        ))
        .expect("cloned + a bound parses");
        assert_eq!(m.models[0].clone_ref_seconds, Some(12.0));
    }

    #[test]
    fn rejects_a_reference_bound_without_cloning() {
        let err = Manifest::parse(&with_model("clone_ref_seconds = 12.0")).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::CloneRefSecondsRequiresCloned { .. }
        ));
    }

    #[test]
    fn rejects_a_non_positive_reference_bound() {
        for bad in ["0.0", "-3.0", "nan"] {
            let err = Manifest::parse(&with_model(&format!(
                "voice_kinds = [\"cloned\"]\nclone_ref_seconds = {bad}"
            )))
            .unwrap_err();
            assert!(
                matches!(err, ManifestError::CloneRefSecondsInvalid { .. }),
                "clone_ref_seconds = {bad} must be rejected, got {err:?}"
            );
        }
    }

    /// The transcript requirement rides on cloning: on its own it would
    /// promise a rule the daemon has no cloned voice to apply it to.
    #[test]
    fn a_transcript_requirement_needs_cloning() {
        let err = Manifest::parse(&with_model("clone_needs_transcript = true")).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::CloneTranscriptRequiresCloned { .. }
        ));

        let m = Manifest::parse(&with_model(
            r#"
            voice_kinds = ["cloned"]
            clone_ref_seconds = 20.0
            clone_needs_transcript = true
            "#,
        ))
        .expect("cloning may require a transcript");
        assert!(m.models[0].clone_needs_transcript);
        assert!(
            !Manifest::parse(&with_model(""))
                .expect("defaults parse")
                .models[0]
                .clone_needs_transcript,
            "a model that says nothing needs no transcript"
        );
    }

    #[test]
    fn rejects_a_zero_max_input_chars() {
        let err = Manifest::parse(&with_model("max_input_chars = 0")).unwrap_err();
        assert!(matches!(err, ManifestError::MaxInputCharsZero { .. }));
    }

    #[test]
    fn rejects_an_implausible_output_sample_rate() {
        for bad in [7999, 192_001] {
            let err =
                Manifest::parse(&with_model(&format!("output_sample_rate = {bad}"))).unwrap_err();
            assert!(matches!(
                err,
                ManifestError::OutputSampleRateOutOfRange { .. }
            ));
        }
        assert!(Manifest::parse(&with_model("output_sample_rate = 48000")).is_ok());
    }

    #[test]
    fn rejects_the_reserved_streaming_input_capability() {
        let t = format!("{VALID}\n[capabilities]\nstreaming_input = true\n");
        let err = Manifest::parse(&t).unwrap_err();
        assert!(matches!(err, ManifestError::StreamingInputReserved));
        // The neighbouring capability is unaffected.
        let ok = format!("{VALID}\n[capabilities]\nwebsocket = true\n");
        assert!(Manifest::parse(&ok).is_ok());
    }

    #[test]
    fn parses_a_manifest_declaring_a_backend_id() {
        let t = VALID.replace("[backend]", "[backend]\n    id = \"app.super-tts.piper\"");
        let m = Manifest::parse(&t).expect("a manifest with a valid id parses");
        assert_eq!(m.backend.id.as_deref(), Some("app.super-tts.piper"));
    }

    #[test]
    fn a_manifest_without_an_id_still_parses() {
        let m = Manifest::parse(VALID).expect("id is optional on disk");
        assert!(m.backend.id.is_none());
    }

    #[test]
    fn rejects_a_malformed_backend_id() {
        let t = VALID.replace("[backend]", "[backend]\n    id = \"piper\"");
        let err = Manifest::parse(&t).unwrap_err();
        assert!(matches!(err, ManifestError::InvalidId(_)));
    }

    #[test]
    fn parses_wasm_manifest_with_secrets_and_options() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "wasm"
            entrypoint = "y.wasm"
            contract = "v1"
            description = "Test backend."

            [assets]
            wasm = "y.wasm"

            [[secrets]]
            name = "y_api_key"
            description = "Key."

            [[options]]
            name = "region"
            description = "Override."
            type = "string"
            default = "https://api.y.com"

            [[options]]
            name = "timeout"
            description = "Seconds."
            type = "integer"
            default = 30
            "#,
        )
        .unwrap();
        assert_eq!(m.backend.kind, Kind::Wasm);
        assert_eq!(m.backend.contract, Contract::V1);
        assert!(m.secrets[0].label.is_none());
        assert!(!m.secrets[0].required);
        assert_eq!(m.options[0].r#type, Some(OptionType::String));
        assert_eq!(
            m.options[0].default,
            Some(OptionDefault::String("https://api.y.com".into()))
        );
        assert_eq!(m.options[1].default, Some(OptionDefault::Integer(30)));
        assert_eq!(m.options[1].default.as_ref().unwrap().to_string(), "30");
    }

    /// `base_url` names the endpoint whose host is authorized for egress with
    /// the SSRF guard relaxed, so a value for it must come from the user. The
    /// format stays lenient about that — the parser keeps whatever the manifest
    /// wrote, and the consumers enforce the rule: the indexer refuses to publish
    /// such a release, and the daemon drops the value and loads the backend
    /// anyway (`super-tts-indexer::manifest::validate`,
    /// `super_tts_daemon::tts_models::backends`).
    #[test]
    fn parse_keeps_a_base_url_default_for_consumers_to_judge() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "wasm"
            entrypoint = "y.wasm"
            contract = "v1"
            description = "Test backend."

            [[options]]
            name = "base_url"
            description = "Endpoint."
            type = "string"
            default = "https://api.y.com"
            "#,
        )
        .expect("parse stays lenient; policy lives in each consumer");
        assert_eq!(m.options[0].name, "base_url");
        assert_eq!(
            m.options[0].default,
            Some(OptionDefault::String("https://api.y.com".into()))
        );
    }

    #[test]
    fn rejects_secret_without_description() {
        let err = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "wasm"
            entrypoint = "y.wasm"
            contract = "v1"
            description = "Test backend."

            [[secrets]]
            name = "y_api_key"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("description"), "got: {err}");
    }

    #[test]
    fn rejects_backend_without_description() {
        let err = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "wasm"
            entrypoint = "y.wasm"
            contract = "v1"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("description"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_kind_at_parse() {
        let err = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "container"
            entrypoint = "y.wasm"
            contract = "v1"
            description = "Test backend."
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown variant"), "got: {err}");
    }

    #[test]
    fn rejects_unsafe_entrypoint() {
        // "a/b" is a *valid* relative path ("bin/launcher" style) — only
        // absolute and traversing values are rejected.
        for bad in ["../escape", "/usr/bin/python3", ".."] {
            let text = format!(
                r#"
                [backend]
                source = "github.com/x/y"
                name = "Y"
                version = "1.0.0"
                kind = "subprocess"
                entrypoint = "{bad}"
                contract = "v1"
                description = "Test backend."
                "#
            );
            let err = Manifest::parse(&text).unwrap_err();
            assert!(
                matches!(err, ManifestError::UnsafeEntrypoint(_)),
                "entrypoint {bad:?} should be rejected, got {err}"
            );
        }
    }

    /// A `[[models]]` table may carry keys this crate does not read, and
    /// `provider` is the one published backends actually ship. The parser must
    /// keep ignoring it: a manifest is fetched from a backend's release at
    /// index time, so rejecting an unread key would drop every already-released
    /// backend out of the index rather than fail some local build.
    ///
    /// Concretely, this is the test that fails if `deny_unknown_fields` is ever
    /// added to `ModelEntry`.
    #[test]
    fn a_model_carrying_an_unread_provider_key_still_parses() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            entrypoint = "y"
            contract = "v1"
            description = "Test backend."

            [[models]]
            name = "m1"
            provider = "local_kokoro"
            primary_language = "en"
            supported_languages = ["en"]
            supported_devices = ["cpu"]
            "#,
        )
        .expect("a manifest declaring `provider` must still parse");
        assert_eq!(m.models.len(), 1);
        assert_eq!(m.models[0].name, "m1");
        assert_eq!(m.models[0].supported_devices, vec![Device::Cpu]);
    }

    #[test]
    fn file_spec_parses_inline_and_block_forms() {
        // The inline-table array and the `[[models.files]]` block form are the
        // same TOML structure; exercise both (on separate models — TOML forbids
        // mixing the two for one key).
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            entrypoint = "y"
            contract = "v1"
            description = "Test backend."

            [[models]]
            name = "m1"
            primary_language = "en"
            supported_languages = ["en"]
            supported_devices = ["cpu"]
            files = [
                { url = "https://example.com/config.json", destination = "models/m1/config.json" },
            ]

            [[models]]
            name = "m2"
            primary_language = "en"
            supported_languages = ["en"]
            supported_devices = ["cpu"]

            [[models.files]]
            url = "https://huggingface.co/openai/kokoro-tiny/resolve/main/model.safetensors"
            destination = "models/m2/model.safetensors"
            sha256 = "abc123"
            "#,
        )
        .unwrap();
        let inline = &m.models[0].files[0];
        assert_eq!(inline.url, "https://example.com/config.json");
        assert_eq!(inline.destination, "models/m1/config.json");
        assert!(inline.sha256.is_none());
        let block = &m.models[1].files[0];
        assert_eq!(block.destination, "models/m2/model.safetensors");
        assert_eq!(block.sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn rejects_unsafe_destination() {
        let manifest = |dest: &str| {
            format!(
                r#"
                [backend]
                source = "github.com/x/y"
                name = "Y"
                version = "1.0.0"
                kind = "subprocess"
                entrypoint = "y"
                contract = "v1"
                description = "Test backend."

                [[models]]
                name = "m"
                primary_language = "en"
                supported_languages = ["en"]
                supported_devices = ["cpu"]
                files = [{{ url = "https://example.com/x", destination = "{dest}" }}]
                "#
            )
        };
        for bad in ["../escape", "/abs/path", "a/../b", "models/"] {
            let err = Manifest::parse(&manifest(bad)).unwrap_err();
            assert!(
                matches!(err, ManifestError::UnsafeDestination(_)),
                "destination {bad:?} should be rejected, got {err}"
            );
        }
        // A nested relative path is accepted.
        Manifest::parse(&manifest("models/m/model.safetensors")).expect("safe nested path");
    }

    #[test]
    fn load_errors_carry_the_file_path() {
        let dir = std::env::temp_dir().join("super-tts-manifest-err-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("backend.toml"), "not [ valid toml").unwrap();
        let err = Manifest::load(&dir).unwrap_err();
        let chain = format!(
            "{err}: {}",
            std::error::Error::source(&err)
                .map(ToString::to_string)
                .unwrap_or_default()
        );
        assert!(chain.contains("backend.toml"), "got: {chain}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Untagged `OptionDefault` must bind TOML primitives by their actual type —
    /// these pins guard against serde/toml upgrades changing untagged behavior.
    #[test]
    fn option_default_binds_by_toml_type() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "wasm"
            entrypoint = "y.wasm"
            contract = "v1"
            description = "Test backend."

            [[options]]
            name = "a"
            description = "A."
            default = true

            [[options]]
            name = "b"
            description = "B."
            default = "30"
            "#,
        )
        .unwrap();
        assert_eq!(m.options[0].default, Some(OptionDefault::Bool(true)));
        assert_eq!(
            m.options[1].default,
            Some(OptionDefault::String("30".into()))
        );
    }

    /// Unknown fields and tables are ignored — older daemons must tolerate
    /// manifests written for newer contract revisions.
    #[test]
    fn unknown_fields_and_tables_are_ignored() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "wasm"
            entrypoint = "y.wasm"
            contract = "v1"
            description = "Test backend."
            future_field = "ignored"

            [future_table]
            x = 1
            "#,
        )
        .unwrap();
        assert_eq!(m.backend.name, "Y");
    }

    #[test]
    fn cuda_sm_is_optional_wildcard() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            entrypoint = "y"
            contract = "v1"
            description = "Test backend."

            [[assets.subprocess]]
            file = "y-cuda13.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = "cuda"
            cuda_major = 13
            "#,
        )
        .unwrap();
        let a = &m.assets.subprocess[0];
        assert_eq!(a.accel, vec![Accel::Cuda]);
        assert_eq!(a.cuda_major, Some(13));
        assert_eq!(a.cuda_sm, None);
        assert!(!a.cudnn);
    }

    /// The v2 file selector is refused under `contract = "v1"`, by name.
    ///
    /// This is the whole reason the selector needed a generation: a v1 daemon
    /// reads the variants of one destination as ordinary files and downloads
    /// every one of them onto the same path.
    #[test]
    fn a_file_selector_requires_contract_v2() {
        let text = with_model(
            r#"
            files = [
                { url = "https://h/x.bin", destination = "m/x.bin", accel = "cuda" },
            ]"#,
        );
        let message = Manifest::parse(&text)
            .expect_err("a v1 manifest may not select a file by host")
            .to_string();
        assert!(
            message.contains("[[models.files]].accel") && message.contains("v2"),
            "the refusal must name the field and the generation that has it: {message}"
        );
        // The installed path skips the contract rule and nothing else, so a
        // backend already on disk keeps loading whatever it declares.
        Manifest::parse_installed(&text).expect("an installed manifest skips the contract rule");
        // Raising the generation is the other documented fix.
        let raised = text.replace(r#"contract = "v1""#, r#"contract = "v2""#);
        Manifest::parse(&raised).expect("the same manifest parses under v2");
    }

    /// Variants of one destination parse, keep manifest order, and may leave
    /// every discriminator off — the looseness a data file is allowed and a
    /// build is not.
    #[test]
    fn parses_per_architecture_file_variants() {
        let text = with_model(
            r#"
            files = [
                { url = "https://h/k-sm90.bin", destination = "c/k.bin", accel = "cuda",
                  cuda_sm = 90, optional = true },
                { url = "https://h/k-ada.bin", destination = "c/k.bin", accel = "cuda",
                  cuda_sm = [86, 89], optional = true },
                { url = "https://h/k-cuda.bin", destination = "c/k.bin", accel = "cuda",
                  optional = true },
                { url = "https://h/k-rocm.bin", destination = "c/k.bin", accel = ["rocm"],
                  gfx = ["gfx1100"], optional = true },
                { url = "https://h/w.bin", destination = "m/w.bin" },
            ]"#,
        )
        .replace(r#"contract = "v1""#, r#"contract = "v2""#);
        let m = Manifest::parse(&text).expect("a v2 selector parses");
        let files = &m.models[0].files;
        assert_eq!(files.len(), 5);
        // The bare number and the list are the same field, one spelling apart.
        assert_eq!(files[0].cuda_sm, vec![90]);
        assert_eq!(files[0].accel, vec![Accel::Cuda]);
        assert_eq!(files[1].cuda_sm, vec![86, 89]);
        // A CUDA variant naming no runtime major is "any CUDA host"; an asset
        // may not say that, a file may.
        assert_eq!(files[2].cuda_major, None);
        assert!(files[2].cuda_sm.is_empty());
        assert!(files[2].is_conditional());
        assert_eq!(files[3].gfx, vec![crate::arch::GfxSpec::new(11, 0, 0)]);
        // The unconditional entry is the v1 shape and stays that way.
        assert!(!files[4].is_conditional());
        assert!(!files[4].optional);
    }

    /// A discriminator naming a family the entry never declared is a typo that
    /// would otherwise select nothing and say nothing, so it is refused — the
    /// same rule `[[assets.subprocess]]` gets.
    #[test]
    fn a_file_discriminator_requires_its_family() {
        let cases = [
            ("cuda_sm = 90", "cuda"),
            (r#"gfx = ["gfx1100"]"#, "gfx"),
            (r#"vulkan_api = "1.3""#, "vulkan"),
        ];
        for (field, expected) in cases {
            let text = with_model(&format!(
                r#"
            files = [
                {{ url = "https://h/x.bin", destination = "m/x.bin", accel = "cpu", {field} }},
            ]"#
            ))
            .replace(r#"contract = "v1""#, r#"contract = "v2""#);
            let message = Manifest::parse(&text)
                .expect_err("a discriminator without its family must not parse")
                .to_string();
            assert!(
                message.contains("m/x.bin") && message.contains(expected),
                "the refusal must name the destination and the missing family: {message}"
            );
        }
    }

    /// `optional` answers "may this destination end up empty?", which is a
    /// property of the destination. Two answers would make the outcome depend
    /// on which variant the host happened to pick.
    #[test]
    fn variants_of_one_destination_must_agree_on_optional() {
        let text = with_model(
            r#"
            files = [
                { url = "https://h/a.bin", destination = "c/k.bin", accel = "cuda",
                  optional = true },
                { url = "https://h/b.bin", destination = "c/k.bin", accel = "rocm" },
            ]"#,
        )
        .replace(r#"contract = "v1""#, r#"contract = "v2""#);
        let message = Manifest::parse(&text)
            .expect_err("variants may not disagree on `optional`")
            .to_string();
        assert!(
            message.contains("c/k.bin") && message.contains("optional"),
            "the refusal must name the destination: {message}"
        );
    }

    #[test]
    fn parses_multipart_subprocess_asset() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            entrypoint = "y"
            contract = "v1"
            description = "Test backend."

            [[assets.subprocess]]
            parts = ["y-cuda13.tar.gz.part00", "y-cuda13.tar.gz.part01"]
            target = "x86_64-unknown-linux-gnu"
            accel = "cuda"
            cuda_major = 13
            "#,
        )
        .unwrap();
        let a = &m.assets.subprocess[0];
        assert!(a.is_multipart());
        assert_eq!(a.file, None);
        assert_eq!(
            a.release_files(),
            vec!["y-cuda13.tar.gz.part00", "y-cuda13.tar.gz.part01"]
        );
    }

    #[test]
    fn empty_file_string_normalizes_to_parts() {
        // Regression (Tier 1 #25): `file = ""` plus valid `parts` used to pass
        // parse but leave `file = Some("")`, so `release_files()` returned `[""]`
        // and `is_multipart()` was false. Parse must normalize empty -> None.
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            entrypoint = "y"
            contract = "v1"
            description = "Test backend."

            [[assets.subprocess]]
            file = ""
            parts = ["y.tar.gz.part00", "y.tar.gz.part01"]
            target = "x86_64-unknown-linux-gnu"
            accel = "cpu"
            "#,
        )
        .unwrap();
        let a = &m.assets.subprocess[0];
        assert_eq!(a.file, None);
        assert!(a.is_multipart());
        assert_eq!(
            a.release_files(),
            vec!["y.tar.gz.part00", "y.tar.gz.part01"]
        );
    }

    #[test]
    fn rejects_subprocess_asset_with_both_file_and_parts() {
        let err = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            entrypoint = "y"
            contract = "v1"
            description = "Test backend."

            [[assets.subprocess]]
            file = "y.tar.gz"
            parts = ["y.tar.gz.part00"]
            target = "x86_64-unknown-linux-gnu"
            accel = "cpu"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::AssetFileXorParts(_)));
    }

    #[test]
    fn rejects_subprocess_asset_with_neither_file_nor_parts() {
        let err = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            entrypoint = "y"
            contract = "v1"
            description = "Test backend."

            [[assets.subprocess]]
            target = "x86_64-unknown-linux-gnu"
            accel = "cpu"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ManifestError::AssetFileXorParts(_)));
    }

    #[test]
    fn device_from_str_round_trips_canonical_forms() {
        for device in [Device::Cpu, Device::Gpu, Device::None] {
            let s = device.to_string();
            let parsed: Device = s.parse().unwrap();
            assert_eq!(device, parsed, "round-trip failed for {s}");
        }
    }

    /// `cuda` and `metal` are the spelling every shipped manifest and published
    /// index uses. They are accepted as input and mapped onto the one device that
    /// means "an accelerator"; nothing ever writes them back.
    #[test]
    fn deprecated_device_spellings_parse_as_gpu() {
        assert_eq!("cuda".parse(), Ok(Device::Gpu));
        assert_eq!("metal".parse(), Ok(Device::Gpu));
        assert_eq!("gpu".parse(), Ok(Device::Gpu));
        assert_eq!("cpu".parse(), Ok(Device::Cpu));
        assert_eq!("none".parse(), Ok(Device::None));
        assert!(
            "rocm".parse::<Device>().is_err(),
            "rocm is an accel, not a device"
        );
        assert!("nonsense".parse::<Device>().is_err());
    }

    #[test]
    fn device_never_emits_a_deprecated_spelling() {
        for device in [Device::Cpu, Device::Gpu, Device::None] {
            let text = device.to_string();
            assert!(
                !matches!(text.as_str(), "cuda" | "metal"),
                "Display emitted a deprecated spelling: {text}"
            );
            assert_eq!(text.parse(), Ok(device), "round trip for {text}");
        }
        assert_eq!(Device::Gpu.to_string(), "gpu");
    }

    #[test]
    fn a_manifest_declaring_cuda_yields_gpu() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            contract = "v1"
            entrypoint = "y"
            license = "Apache-2.0"
            description = "Test backend."

            [[assets.subprocess]]
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = "cuda"
            cuda_major = 12

            [[models]]
            name = "m"
            supported_devices = ["cpu", "cuda"]
            primary_language = "en"
            supported_languages = ["en"]
        "#,
        )
        .expect("shipped manifests must keep parsing");
        assert_eq!(
            m.models[0].supported_devices,
            vec![Device::Cpu, Device::Gpu]
        );
    }

    #[test]
    fn device_from_str_rejects_non_canonical_strings() {
        // `rocm` is an `Accel` build axis, never a model `Device`; non-snake_case
        // and unknown strings must error so callers don't accept stale forms.
        for bad in ["rocm", "Cpu", "CUDA", "metal_gpu", ""] {
            assert!(
                bad.parse::<Device>().is_err(),
                "{bad:?} should fail to parse as a Device"
            );
        }
    }

    /// Build a minimal valid manifest around one `[[assets.subprocess]]` body, so
    /// asset-level validation tests carry only the lines under test.
    fn manifest_with_asset(asset_body: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(&format!(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            contract = "v1"
            entrypoint = "y"
            license = "Apache-2.0"
            description = "Test backend."

            [[assets.subprocess]]
            {asset_body}

            [[models]]
            name = "m"
            supported_devices = ["cpu"]
            primary_language = "en"
            supported_languages = ["en"]
        "#
        ))
    }

    /// A scalar `accel` is the spelling every shipped manifest uses, and
    /// `backend.toml` is a pinned release asset — rejecting it would break
    /// already-installed backends on users' machines.
    #[test]
    fn a_scalar_accel_parses_as_a_one_element_list() {
        let m = Manifest::parse(
            r#"
            [backend]
            source = "github.com/x/y"
            name = "Y"
            version = "1.0.0"
            kind = "subprocess"
            contract = "v1"
            entrypoint = "y"
            license = "Apache-2.0"
            description = "Test backend."

            [[assets.subprocess]]
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = "cuda"
            cuda_major = 12

            [[models]]
            name = "m"
            supported_devices = ["cpu"]
            primary_language = "en"
            supported_languages = ["en"]
        "#,
        )
        .expect("a scalar accel must parse");
        assert_eq!(m.assets.subprocess[0].accel, vec![Accel::Cuda]);
    }

    #[test]
    fn a_list_accel_parses() {
        let m = manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = ["cuda", "rocm"]
            cuda_major = 12
            gfx = ["gfx1030"]
        "#,
        )
        .expect("a dual-runtime asset must parse");
        assert_eq!(m.assets.subprocess[0].accel, vec![Accel::Cuda, Accel::Rocm]);
        assert_eq!(
            m.assets.subprocess[0].gfx,
            vec![crate::arch::GfxSpec::new(10, 3, 0)]
        );
    }

    #[test]
    fn an_empty_accel_list_is_rejected() {
        let err = manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = []
        "#,
        )
        .expect_err("an asset must declare at least one accel");
        assert!(format!("{err}").contains("accel"), "{err}");
    }

    #[test]
    fn rocm_requires_gfx_and_forbids_it_elsewhere() {
        let err = manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = ["rocm"]
        "#,
        )
        .expect_err("a rocm asset must list its gfx targets");
        assert!(format!("{err}").contains("gfx"), "{err}");

        let err = manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = ["cpu"]
            gfx = ["gfx1030"]
        "#,
        )
        .expect_err("gfx is meaningless without rocm");
        assert!(format!("{err}").contains("gfx"), "{err}");
    }

    #[test]
    fn vulkan_api_is_allowed_only_with_vulkan() {
        manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = ["vulkan"]
            vulkan_api = "1.2"
        "#,
        )
        .expect("a vulkan asset may declare an api floor");

        let err = manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = ["cpu"]
            vulkan_api = "1.2"
        "#,
        )
        .expect_err("vulkan_api without vulkan is a contradiction");
        assert!(format!("{err}").contains("vulkan"), "{err}");
    }

    #[test]
    fn cuda_fields_are_gated_on_accel_containing_cuda() {
        manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = ["cuda", "rocm"]
            cuda_major = 12
            cuda_sm = 86
            gfx = ["gfx1030"]
        "#,
        )
        .expect("a dual asset may carry both vendors' discriminators");

        let err = manifest_with_asset(
            r#"
            file = "y.tar.gz"
            target = "x86_64-unknown-linux-gnu"
            accel = ["rocm"]
            gfx = ["gfx1030"]
            cuda_sm = 86
        "#,
        )
        .expect_err("cuda_sm without cuda is a contradiction");
        assert!(format!("{err}").contains("cuda"), "{err}");
    }
}
