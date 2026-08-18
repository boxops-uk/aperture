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

/// Where a schema's imports are looked for
/// ([operations §7](../docs/aperture-cli-design.md)).
///
/// A list of roots, searched in order, first match wins — `lang.rust` is `lang/rust.aps`
/// under one of them. An entry file's *own* directory is always searched first and is
/// not configured, so a self-contained directory of schemas needs none of this.
///
/// `APERTURE_SCHEMA_PATH` is separated the way `PATH` is, because that is the one
/// convention nobody has to look up.
#[must_use]
pub fn schema_path(flag: Option<Vec<PathBuf>>) -> Vec<PathBuf> {
    if let Some(roots) = flag {
        return roots;
    }

    std::env::var_os("APERTURE_SCHEMA_PATH")
        .map(|raw| std::env::split_paths(&raw).collect())
        .unwrap_or_default()
}

/// Whether the store root was **chosen** rather than defaulted to.
///
/// What it decides is where the socket goes: a root somebody named gets its socket
/// beside it, and the default root gets the well-known one. See
/// [`socket_path`](socket_path).
#[must_use]
pub fn root_was_chosen(flag: Option<&PathBuf>) -> bool {
    flag.is_some() || std::env::var_os("APERTURE_DATA_DIR").is_some()
}

/// The socket to listen on or connect to.
///
/// **A named root keeps its socket beside it; the default root uses the well-known
/// one.** Two roots therefore mean two servers, which is what the test suite and a
/// second store on one machine both rely on — and a client that named no root has a
/// short, fixed path to reach, which is the whole of "you should not need to know the
/// data directory to connect".
///
/// The well-known path is `$XDG_RUNTIME_DIR/aperture.sock`, and it is short on purpose:
/// a Unix socket path has a hard length limit (`SUN_LEN`, 108 bytes on Linux), and
/// deriving one from a deep data directory produces a path the kernel refuses. Falling
/// back to the root when `XDG_RUNTIME_DIR` is unset keeps the old behaviour in the
/// environments — containers, cron — that do not set it.
#[must_use]
pub fn socket_path(data_dir: &std::path::Path, chosen: bool, flag: Option<PathBuf>) -> PathBuf {
    if let Some(path) = flag {
        return path;
    }

    if !chosen && let Some(run) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(run).join(SOCKET_FILE);
    }

    data_dir.join(SOCKET_FILE)
}

/// What a config file may say.
///
/// **Two scalars, both about *where*.** Not which database: a file that silently decided
/// that would be the ambient-state problem this design refuses one level down, where it
/// would be harder to see.
///
/// JSON, and read with the `serde_json` already in the build rather than by adding a
/// configuration crate for two fields. [Operations §3](../docs/aperture-cli-design.md)
/// specifies the figment *pattern* — defaults → file → env → flags, every field
/// `Option<T>` so an unset layer cannot clobber a lower one — and that is what this is;
/// the crate was never the point.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// Where a server is: a host, a `host:port`, or a socket path.
    pub target: Option<String>,
    /// The store root.
    pub data_dir: Option<PathBuf>,
}

/// The config file's name when one is not named.
pub const CONFIG_FILE: &str = "aperture.json";

/// Read `--config`, or `./aperture.json` if it happens to be there.
///
/// **The working directory only, and no walk upwards.** Cargo and git search parents;
/// this deliberately does not, because a connection target inherited from a directory
/// nobody was thinking about is the same invisible-state problem as a global registry,
/// just harder to notice. CI writes the file where it runs.
///
/// # Errors
///
/// [`CliError::Config`] if a file that was named cannot be read or parsed. A *missing*
/// `aperture.json` is not an error — nobody asked for one — but a missing `--config` is.
pub fn file(flag: Option<&std::path::Path>) -> Result<File, crate::CliError> {
    let path = match flag {
        Some(path) => path.to_path_buf(),
        None => {
            let local = PathBuf::from(CONFIG_FILE);
            if !local.is_file() {
                return Ok(File::default());
            }
            local
        }
    };

    let text = std::fs::read_to_string(&path).map_err(|source| crate::CliError::Config {
        path: path.clone(),
        detail: source.to_string(),
    })?;

    serde_json::from_str(&text).map_err(|source| crate::CliError::Config {
        path,
        detail: source.to_string(),
    })
}

/// Where a client connects when the address named no target.
///
/// `APERTURE_TARGET` beats the config file beats the local socket. The address itself is
/// the layer above all of them and is applied by the caller, since it is the argument.
///
/// # Errors
///
/// [`CliError::Client`] if the environment or the file holds something that is not a
/// target.
pub fn default_endpoint(
    socket: &std::path::Path,
    file: &File,
) -> Result<aperture_client::Endpoint, crate::CliError> {
    if let Some(text) = std::env::var_os("APERTURE_TARGET") {
        let text = text.to_string_lossy().into_owned();
        return Ok(aperture_client::Endpoint::parse(&text)?);
    }

    if let Some(text) = &file.target {
        return Ok(aperture_client::Endpoint::parse(text)?);
    }

    Ok(aperture_client::Endpoint::Unix(socket.to_path_buf()))
}
