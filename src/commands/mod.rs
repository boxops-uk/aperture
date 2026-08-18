//! One module per command, and one rule they all follow: **a command never opens a
//! store root a server holds**.
//!
//! `ops-I1` gives one process ownership of a root, and §2 is explicit that there is no
//! *silent* fallback from "connect" to "open directly". [`route`] is where that rule
//! is stated, once, for every command that changes a database's life.

pub mod create;
pub mod describe;
pub mod finish;
pub mod list;
pub mod query;
pub mod rm;
pub mod schema;
pub mod serve;
pub mod shell;

use std::{path::Path, sync::Arc};

use aperture_client::{Address, ClientError, Connection, Endpoint};
use aperture_store::{
    catalog::{Catalog, RootLock},
    error::StoreError,
};

use crate::{CliError, code_index};

/// Where a command is going, and which database it is about.
///
/// The resolved form of an [`Address`]: the target filled in from whatever layer
/// supplied it, and the selector left as the string the catalog will parse.
#[derive(Debug, Clone)]
pub struct Target {
    /// Where the server is.
    pub endpoint: Endpoint,
    /// `name`, or `name@instance`, or empty for a control session.
    pub database: String,
    /// Whether this process may do the work itself when no server answers.
    ///
    /// **True only when nobody named a target.** Having asked for a particular server,
    /// "it is not there" is an answer rather than an invitation to open some other
    /// root — so `box//code` and `APERTURE_TARGET=box` both take the offline path away,
    /// and only the plain local socket keeps it. That is the same reasoning as §2's
    /// no-silent-fallback rule, applied one level up: the rule forbids reaching past a
    /// server that might be holding the root, and reaching past a server somebody
    /// *named* is worse.
    pub offline: bool,
}

impl Target {
    /// Resolve `text` against a default target.
    ///
    /// # Errors
    ///
    /// [`CliError::Client`] if `text` is not an address.
    pub fn resolve(
        text: &str,
        default: &Endpoint,
        default_is_local: bool,
    ) -> Result<Target, CliError> {
        let address = Address::parse(text)?;
        let named = address.endpoint().is_some();

        Ok(Target {
            endpoint: address
                .endpoint()
                .cloned()
                .unwrap_or_else(|| default.clone()),
            database: address.database().to_owned(),
            offline: !named && default_is_local,
        })
    }

    /// A target at `socket`, for a caller that already holds one.
    ///
    /// Tests only: everything in the binary arrives here through
    /// [`resolve`](Target::resolve), which is where the layering lives.
    #[cfg(test)]
    #[must_use]
    pub fn at(socket: impl Into<std::path::PathBuf>, database: impl Into<String>) -> Target {
        Target {
            endpoint: Endpoint::Unix(socket.into()),
            database: database.into(),
            offline: false,
        }
    }

    /// The socket this target names, if it is one.
    ///
    /// Which is what the offline path needs: a root is a directory on *this* machine, so
    /// the question only arises for a socket.
    #[must_use]
    pub fn socket(&self) -> Option<&Path> {
        match &self.endpoint {
            Endpoint::Unix(path) => Some(path),
            Endpoint::Tcp(_) => None,
        }
    }
}

/// Where a lifecycle command's work is going to happen.
pub enum Route {
    /// Through a running server, over its socket.
    Server(Connection),
    /// In this process, holding the root (§2's `--embedded`, with the root as the
    /// path). The lock rides along and must be kept alive for the whole operation.
    Local(Catalog, RootLock),
}

/// §2 address resolution for a command that changes a database.
///
/// **The socket is the detection mechanism, and there is no other autodetect** — §2
/// says so in those words. If a server is listening, the command is its; if none is,
/// this process does the work under the root lock.
///
/// That ordering is what makes it a resolution rather than a fallback. The forbidden
/// thing is to try the server, fail, and open the directory *anyway* — because a
/// server might be holding it. Here nothing is opened until the socket has already
/// answered that none is. The lock is still taken, and is still the authority: a root
/// held by something that is not listening is refused by name rather than opened.
///
/// # Errors
///
/// [`CliError::RootHeld`] if no server is listening and something else holds the root.
pub fn route(root: &Path, target: &Target) -> Result<Route, CliError> {
    // A target somebody named has no offline half — see [`Target::offline`].
    let Some(socket) = target.socket().filter(|_| target.offline) else {
        return Ok(Route::Server(control(&target.endpoint)?));
    };

    match connect(socket)? {
        Some(server) => Ok(Route::Server(server)),
        None => {
            let (catalog, lock) = exclusive(root, socket)?;
            Ok(Route::Local(catalog, lock))
        }
    }
}

/// A control session at `endpoint`, or the failure to open one.
///
/// Unlike [`connect`], a refused socket is *reported*: this is the path taken when
/// somebody named where to go, and answering "no server" by quietly doing something else
/// is exactly what §2 forbids.
///
/// # Errors
///
/// [`CliError::NoServer`] if nothing is listening, which is the message that says what
/// to do about it.
fn control(endpoint: &Endpoint) -> Result<Connection, CliError> {
    let schema = Arc::new(code_index::schema());

    Connection::control_at(endpoint, schema).map_err(|error| match error {
        ClientError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            CliError::NoServer {
                target: endpoint.clone(),
            }
        }
        other => other.into(),
    })
}

/// Open a control session, or answer that no server is listening.
///
/// **A missing socket and a refused one are the same answer**: no server. The first is
/// a root nothing has served; the second is the file a killed server left behind, and
/// treating it as "a server is there" would refuse every command until someone deleted
/// a stale inode by hand. Anything else — a socket that exists and will not talk to us
/// — is reported rather than assumed away, because that is a server we are being kept
/// out of, not the absence of one.
///
/// The session asserts this build's schema fingerprint rather than accepting whatever
/// the server has. A tool whose built-in schema is not the server's would otherwise
/// create a database against a schema it does not have, and find out by writing facts
/// nobody can read back — which is precisely what that handshake field is for.
fn connect(socket: &Path) -> Result<Option<Connection>, CliError> {
    use std::io::ErrorKind;

    match Connection::control(socket, Arc::new(code_index::schema())) {
        Ok(server) => Ok(Some(server)),

        Err(ClientError::Io(error))
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }

        Err(error) => Err(error.into()),
    }
}

/// Open the store root and take exclusive ownership of it.
///
/// # Errors
///
/// [`CliError::RootHeld`] with an actionable message if another process holds it —
/// which is `ops-I1` refusing rather than a lock to wait on.
pub fn exclusive(root: &Path, socket: &Path) -> Result<(Catalog, RootLock), CliError> {
    let catalog = Catalog::open(root)?;

    match catalog.lock() {
        Ok(lock) => Ok((catalog, lock)),
        Err(StoreError::RootHeld { .. }) => Err(CliError::RootHeld {
            root: root.to_path_buf(),
            socket: socket.to_path_buf(),
        }),
        Err(other) => Err(other.into()),
    }
}

/// Open the store root for reading only.
///
/// Takes **no lock**: enumeration reads sidecars and never opens fjall (`ops-I7`), so
/// it works perfectly well while a server owns every database under the root. That is
/// the whole point of the filesystem being the catalog, and the reason `list` and
/// `describe` never needed a control message to work against a running server.
///
/// # Errors
///
/// [`CliError::Store`] if the root cannot be read.
pub fn readable(root: &Path) -> Result<Catalog, CliError> {
    Ok(Catalog::open(root)?)
}
