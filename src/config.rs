//! Where things live.
//!
//! [Operations §3](../docs/aperture-cli-design.md) specifies figment layering —
//! defaults → config file → `APERTURE_` env → flags, every field `Option<T>` so an
//! unset flag does not clobber a lower layer. **The file layer is not here yet**; this
//! is defaults → env → flags, which is the same shape with one layer missing and the
//! same `Option<T>` discipline, so adding the file is an insertion rather than a
//! rewrite.

use std::path::PathBuf;

/// The socket's name inside the store root ([operations §9](../docs/aperture-cli-design.md)).
pub const SOCKET_FILE: &str = "aperture.sock";

/// The store root.
///
/// A flag beats `APERTURE_DATA_DIR` beats `$XDG_DATA_HOME/aperture` beats
/// `$HOME/.local/share/aperture`. The last fallback is the working directory, which is
/// deliberately visible rather than a hidden temp: a tool that silently put databases
/// somewhere unfindable would be worse than one that put them underfoot.
#[must_use]
pub fn data_dir(flag: Option<PathBuf>) -> PathBuf {
    if let Some(path) = flag {
        return path;
    }

    if let Some(path) = std::env::var_os("APERTURE_DATA_DIR") {
        return PathBuf::from(path);
    }

    if let Some(base) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(base).join("aperture");
    }

    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/aperture");
    }

    PathBuf::from("aperture-data")
}

/// The socket to listen on or connect to.
///
/// Derived from the store root rather than chosen, which is what makes the socket the
/// server-detection mechanism (§2): a client that knows the data directory knows where
/// to look, with nothing to configure and nothing to get out of step.
#[must_use]
pub fn socket_path(data_dir: &std::path::Path, flag: Option<PathBuf>) -> PathBuf {
    flag.unwrap_or_else(|| data_dir.join(SOCKET_FILE))
}
