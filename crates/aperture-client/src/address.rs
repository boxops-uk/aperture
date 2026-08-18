//! **Where a server is, and which database on it** — the whole of addressing.
//!
//! ```text
//! [where//]name[@instance]
//! ```
//!
//! | Address | Means |
//! |---|---|
//! | `code`, `code@01M0B3D` | the caller's default target |
//! | `//code` | the same thing, said explicitly |
//! | `box:7280//code` | TCP |
//! | `box//code` | TCP, [`DEFAULT_PORT`] |
//! | `/run/user/1000/aperture.sock//code` | a Unix socket |
//! | `./dev.sock//code` | a relative socket path |
//! | `box:7280//` | that server, no database — a control session |
//!
//! # Two rules carry it, and both are derived rather than invented
//!
//! **Split at the *last* `//`.** A database name may not contain `/` — the catalog
//! refuses one — so the last separator is always the right one. That is not merely
//! sufficient, it is what makes a socket path holding a doubled slash parse instead of
//! silently misreading, and it is why no "everything before the final slash" rule needs
//! learning.
//!
//! **A relative socket path needs `./`.** Otherwise `dev.sock//code` is indistinguishable
//! from a host called `dev.sock`, which is the one genuine ambiguity in the grammar. It
//! is the same rule a shell already imposes on `./script`, and the same one Go reached
//! for and then went further with by banning relative imports outright.
//!
//! # What is deliberately absent
//!
//! **No scheme.** `//` announces where the target is; nothing needs to announce that an
//! Aperture address is an Aperture address.
//!
//! **No names to look up.** A `where` is always literal — a host or a path — and never
//! an alias resolved through a registry somewhere on the machine. A named target whose
//! meaning lives in ambient config is how `kubectl delete` reaches the wrong cluster;
//! the default target is settable (by environment, by a config file the caller points
//! at), but *which database* is only ever what the argument says.
//!
//! **No credentials.** The handshake has no credential field, so `user@host` would have
//! been syntax with nothing behind it.
//!
//! # What this module does not parse
//!
//! The selector — `name@instance` — is passed through as a string. Resolving which
//! instance of a name is meant belongs to the store's catalog, and the server does it;
//! putting it here would make this crate depend on the storage layer to answer a
//! question it only has to forward.

use std::path::{Path, PathBuf};

use crate::error::ClientError;

/// The TCP port a `host//db` address means.
///
/// Arbitrary, as every such number is; picked to avoid the databases and management
/// consoles that already crowd this range.
pub const DEFAULT_PORT: u16 = 7280;

/// What separates where a server is from which database on it.
pub const SEPARATOR: &str = "//";

/// A server, located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix socket, by path.
    Unix(PathBuf),
    /// A TCP authority — `host:port`, with the port already filled in.
    Tcp(String),
}

/// An address: where, and which database.
///
/// The endpoint is optional because the everyday form names none, and what "none" means
/// is the caller's to decide — the CLI layers a flag over an environment variable over a
/// config file over the local socket. Keeping that out of here is what stops this module
/// from having opinions about a machine it knows nothing about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    endpoint: Option<Endpoint>,
    database: String,
}

impl Address {
    /// Parse `[where//]database`.
    ///
    /// # Errors
    ///
    /// [`ClientError::BadAddress`] for a `where` that is neither a host nor a path, or a
    /// database name carrying a path separator.
    pub fn parse(text: &str) -> Result<Address, ClientError> {
        let bad = |detail: String| Err(ClientError::BadAddress(format!("`{text}`: {detail}")));

        // The *last* separator, because a database name cannot contain `/` and a socket
        // path can. `/tmp//sockets//code` is a socket at `/tmp//sockets`.
        let (where_, database) = match text.rfind(SEPARATOR) {
            Some(at) => (&text[..at], &text[at + SEPARATOR.len()..]),
            None => ("", text),
        };

        if database.contains('/') {
            return bad("a database name cannot contain `/`".to_owned());
        }

        let endpoint = if where_.is_empty() {
            None
        } else if is_path(where_) {
            Some(Endpoint::Unix(expand_home(where_)))
        } else if where_.contains('/') {
            return bad(format!(
                "`{where_}` is neither a host nor a path — a relative socket path needs `./`"
            ));
        } else {
            Some(Endpoint::Tcp(authority(where_)))
        };

        Ok(Address {
            endpoint,
            database: database.to_owned(),
        })
    }

    /// An address for `database` on the caller's default target.
    #[must_use]
    pub fn local(database: impl Into<String>) -> Address {
        Address {
            endpoint: None,
            database: database.into(),
        }
    }

    /// Where the server is, or `None` for the caller's default.
    #[must_use]
    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref()
    }

    /// The selector — `name`, or `name@instance`, or empty for a control session.
    #[must_use]
    pub fn database(&self) -> &str {
        &self.database
    }

    /// This address with `endpoint` filled in wherever it named none.
    #[must_use]
    pub fn or_endpoint(self, endpoint: Endpoint) -> Address {
        Address {
            endpoint: Some(self.endpoint.unwrap_or(endpoint)),
            database: self.database,
        }
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.endpoint {
            Some(Endpoint::Unix(path)) => write!(f, "{}", path.display())?,
            Some(Endpoint::Tcp(authority)) => f.write_str(authority)?,
            // A bare name is the everyday spelling, so it is what a bare name prints —
            // except when there is no database either, where `//` is the only spelling
            // that reads as an address at all.
            None if !self.database.is_empty() => return f.write_str(&self.database),
            None => {}
        }

        write!(f, "{SEPARATOR}{}", self.database)
    }
}

/// Whether `where` is a filesystem path rather than a host.
///
/// Absolute, explicitly relative, or home-relative. A bare `dev.sock` is a host, which is
/// the ambiguity `./` exists to settle.
fn is_path(text: &str) -> bool {
    text.starts_with('/')
        || text.starts_with("./")
        || text.starts_with("../")
        || text == "~"
        || text.starts_with("~/")
}

/// `~` expanded from the environment.
///
/// Done here rather than left to fail at connect time: a quoted `'~/db/aperture.sock//code'`
/// reaches us with the tilde intact, and "no such file `~/db/aperture.sock`" is a worse
/// answer than the one somebody meant.
fn expand_home(text: &str) -> PathBuf {
    let Some(rest) = text.strip_prefix('~') else {
        return PathBuf::from(text);
    };

    match std::env::var_os("HOME") {
        Some(home) => Path::new(&home).join(rest.trim_start_matches('/')),
        None => PathBuf::from(text),
    }
}

/// A host with [`DEFAULT_PORT`] appended unless one is already there.
///
/// Bracketed IPv6 is why this is not a search for `:` — `[::1]` is all colons and no
/// port, and `[::1]:7280` is the same address with one.
fn authority(host: &str) -> String {
    let has_port = if host.starts_with('[') {
        host.rfind("]:").is_some()
    } else {
        host.contains(':')
    };

    if has_port {
        host.to_owned()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Address {
        Address::parse(text).expect("it parses")
    }

    #[test]
    fn a_bare_name_names_no_target() {
        let address = parse("code");
        assert_eq!(address.endpoint(), None);
        assert_eq!(address.database(), "code");
    }

    /// The selector passes through untouched — including the instance, which this module
    /// deliberately does not understand.
    #[test]
    fn an_instance_is_carried_not_parsed() {
        assert_eq!(parse("code@01M0B3D").database(), "code@01M0B3D");
        assert_eq!(parse("box//code@01M0B3D").database(), "code@01M0B3D");
    }

    #[test]
    fn an_empty_where_is_the_default_target_said_explicitly() {
        assert_eq!(parse("//code"), parse("code"));
    }

    #[test]
    fn a_host_gets_the_default_port() {
        assert_eq!(
            parse("box//code").endpoint(),
            Some(&Endpoint::Tcp(format!("box:{DEFAULT_PORT}")))
        );
        assert_eq!(
            parse("box:9999//code").endpoint(),
            Some(&Endpoint::Tcp("box:9999".to_owned()))
        );
    }

    /// Bracketed IPv6 is all colons and no port, so the port test cannot be a search
    /// for one.
    #[test]
    fn bracketed_ipv6_is_understood() {
        assert_eq!(
            parse("[::1]//code").endpoint(),
            Some(&Endpoint::Tcp(format!("[::1]:{DEFAULT_PORT}")))
        );
        assert_eq!(
            parse("[::1]:9999//code").endpoint(),
            Some(&Endpoint::Tcp("[::1]:9999".to_owned()))
        );
    }

    #[test]
    fn an_absolute_path_is_a_socket() {
        assert_eq!(
            parse("/run/user/1000/aperture.sock//code").endpoint(),
            Some(&Endpoint::Unix(PathBuf::from(
                "/run/user/1000/aperture.sock"
            )))
        );
    }

    /// The rule `./` exists for: without it this is a host called `dev.sock`.
    #[test]
    fn a_relative_socket_path_needs_a_leading_dot() {
        assert_eq!(
            parse("./dev.sock//code").endpoint(),
            Some(&Endpoint::Unix(PathBuf::from("./dev.sock")))
        );
        assert_eq!(
            parse("dev.sock//code").endpoint(),
            Some(&Endpoint::Tcp(format!("dev.sock:{DEFAULT_PORT}"))),
            "without `./` it is a host, which is the ambiguity the rule settles"
        );
    }

    /// Splitting at the *last* separator is what makes this parse rather than misread.
    #[test]
    fn a_socket_path_may_hold_a_doubled_slash() {
        assert_eq!(
            parse("/tmp//sockets//code").endpoint(),
            Some(&Endpoint::Unix(PathBuf::from("/tmp//sockets")))
        );
    }

    #[test]
    fn an_empty_database_is_a_control_session() {
        let address = parse("box:9999//");
        assert_eq!(address.database(), "");
        assert_eq!(
            address.endpoint(),
            Some(&Endpoint::Tcp("box:9999".to_owned()))
        );

        // And with no target either, which is what `shell` with no argument opens.
        assert_eq!(parse("//").database(), "");
        assert_eq!(parse("//").endpoint(), None);
        assert_eq!(parse(""), parse("//"));
    }

    #[test]
    fn what_is_neither_a_host_nor_a_path_is_refused() {
        assert!(matches!(
            Address::parse("box/nested//code"),
            Err(ClientError::BadAddress(_))
        ));

        // A `/` on the database side is a name the catalog would refuse anyway, caught
        // here where the message can say which half is wrong.
        assert!(matches!(
            Address::parse("box//a/b"),
            Err(ClientError::BadAddress(_))
        ));
    }

    /// Round-tripped by **value**: `code` and `//code` are the same address, and the
    /// bare form is the one that prints.
    #[test]
    fn an_address_round_trips_through_its_text_form() {
        for text in [
            "code",
            "code@01M0B3D",
            "box:7280//code",
            "box:9999//code@01M0B3D",
            "/run/user/1000/aperture.sock//code",
            "./dev.sock//",
            "//",
        ] {
            let address = parse(text);
            assert_eq!(
                Address::parse(&address.to_string()).expect("it re-parses"),
                address,
                "{text} printed as {address}"
            );
        }

        // The bare spellings print bare, and the explicit ones print explicitly.
        assert_eq!(parse("code").to_string(), "code");
        assert_eq!(parse("//code").to_string(), "code");
        assert_eq!(parse("box//code").to_string(), "box:7280//code");
        assert_eq!(parse("//").to_string(), "//");
    }

    #[test]
    fn a_default_target_fills_in_only_where_none_was_named() {
        let fallback = Endpoint::Unix(PathBuf::from("/run/aperture.sock"));

        assert_eq!(
            parse("code").or_endpoint(fallback.clone()).endpoint(),
            Some(&fallback)
        );

        // ...and does not override one that was.
        assert_eq!(
            parse("box:9999//code").or_endpoint(fallback).endpoint(),
            Some(&Endpoint::Tcp("box:9999".to_owned()))
        );
    }
}
