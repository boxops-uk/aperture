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
pub mod serve;

use std::{path::Path, sync::Arc};

use aperture_client::{ClientError, Connection};
use aperture_store::{
    catalog::{Catalog, RootLock},
    error::StoreError,
};

use crate::{CliError, code_index};

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
pub fn route(root: &Path, socket: &Path) -> Result<Route, CliError> {
    match connect(socket)? {
        Some(server) => Ok(Route::Server(server)),
        None => {
            let (catalog, lock) = exclusive(root, socket)?;
            Ok(Route::Local(catalog, lock))
        }
    }
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
