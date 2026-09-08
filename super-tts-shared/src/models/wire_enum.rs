// SPDX-License-Identifier: GPL-3.0-only
//! One table → one wire form for fieldless settings enums.
//!
//! `RecordingStopMode`, `WriteMethod`, and `AudioTheme` each cross the wire and
//! the config file as a stable `snake_case` token. Previously each carried three
//! hand-maintained forms (`PascalCase` serde, kebab `Display`, aliased `FromStr`)
//! that had drifted. [`wire_enum_strings!`] generates `Display`, `FromStr`, and
//! serde `Serialize`/`Deserialize` from a single table so they can't.
//!
//! **Unknown-value policy.** `FromStr` and `Deserialize` reject an unrecognized
//! token (so REST endpoints answer `400`, not a silent default). Config-load
//! resilience — an unknown stored value degrading to the field default instead
//! of failing the whole parse — is a separate concern handled by
//! `deserialize_or_default` on the individual config field. There are no legacy
//! aliases: exactly one token maps to each variant.

/// Generate the wire-string plumbing (`as_wire_str`, `Display`, `FromStr`,
/// serde `Serialize`/`Deserialize`) for a fieldless enum from a single table of
/// `Variant => "snake_case_token"` pairs. The enum keeps its own derives
/// (`Debug`, `Clone`, `Copy`, `Default`, …) and any domain methods; it must
/// **not** also derive `Serialize`/`Deserialize` (this macro provides them).
macro_rules! wire_enum_strings {
    ($ty:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl $ty {
            /// The stable `snake_case` wire/config token for this variant.
            #[must_use]
            pub fn as_wire_str(self) -> &'static str {
                match self { $( Self::$variant => $wire, )+ }
            }

            /// Every accepted wire/config token, in declaration order. Lets a
            /// CLI build its `value_parser` (or help text) from the single
            /// table instead of re-listing the strings.
            pub const WIRE_VARIANTS: &'static [&'static str] = &[ $( $wire, )+ ];
        }

        impl ::std::fmt::Display for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_wire_str())
            }
        }

        impl ::std::str::FromStr for $ty {
            type Err = String;
            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                match s {
                    $( $wire => ::std::result::Result::Ok(Self::$variant), )+
                    other => ::std::result::Result::Err(
                        ::std::format!(concat!("unknown ", stringify!($ty), ": {}"), other)
                    ),
                }
            }
        }

        impl ::serde::Serialize for $ty {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> ::std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_wire_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> ::std::result::Result<Self, D::Error> {
                let s = <::std::string::String as ::serde::Deserialize>::deserialize(d)?;
                s.parse().map_err(::serde::de::Error::custom)
            }
        }

        // The OpenAPI schema comes off the same table as `Serialize` and
        // `FromStr`, for the same reason those do. `#[derive(ToSchema)]` reads
        // the *Rust* variant names, so it would publish `SciFi` as an accepted
        // value of a field that only ever accepts `sci_fi` — a spec that
        // disagrees with the endpoint it documents, and silently, since nothing
        // type-checks a schema against a hand-written `Serialize`.
        #[cfg(feature = "openapi")]
        impl ::utoipa::PartialSchema for $ty {
            fn schema() -> ::utoipa::openapi::RefOr<::utoipa::openapi::schema::Schema> {
                ::utoipa::openapi::ObjectBuilder::new()
                    .schema_type(::utoipa::openapi::Type::String)
                    .enum_values(::std::option::Option::Some(
                        Self::WIRE_VARIANTS.iter().copied(),
                    ))
                    .into()
            }
        }

        #[cfg(feature = "openapi")]
        impl ::utoipa::ToSchema for $ty {
            fn name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(::std::stringify!($ty))
            }
        }
    };
}

pub(crate) use wire_enum_strings;
