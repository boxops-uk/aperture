//! `APERTURE_META` — what a database says about itself, **without being opened**.
//!
//! The sidecar is the fast enumeration path
//! ([`ops-I7`](../../../docs/aperture-cli-design.md)): `list` walks the store root
//! and reads these, never touching fjall — which is what lets it work while a server
//! holds every database in the root. The copy of the schema under `schema/` is the
//! durable fallback if a sidecar is lost; the sidecar is what is *read*.
//!
//! # Written atomically, or not at all
//!
//! Every write is temp → fsync → rename, and the rename is the moment the new
//! contents exist. [`ops-I3`](../../../docs/aperture-cli-design.md) needs that of the
//! status flip in particular: `finish` must never be observable as "metadata says
//! Complete while data is not durable", so the flip has to be a single atomic act and
//! the last one.
//!
//! # The field list is fixed, and two absences are deliberate
//!
//! - **No `externally_modified`.** `ops-I6` is explicit: a marker that downgraded
//!   identity would contradict `ops-I4`'s "identity is always the content hash".
//!   Manual writes are trusted, not tracked.
//! - **No provenance.** Operations §5 records it as a genuine gap and a later
//!   addition; it is descriptive-only under `ops-I4` either way.
//!
//! Both are *additions* the versioned format can take later rather than migrations,
//! which is what [`Meta::VERSION`] is for.
//!
//! # JSON, and epoch milliseconds
//!
//! JSON because a person will `cat` this file, and because the alternative — a packed
//! binary — buys nothing for a document read once per database per listing.
//!
//! `created_at_ms` is epoch milliseconds rather than a formatted timestamp, which is
//! the less friendly choice made deliberately: rendering a civil date needs either a
//! dependency or thirty lines of calendar arithmetic in the storage layer, and the
//! value is descriptive (`ops-I4`) so nothing but a human ever reads it. The CLI
//! formats it.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{error::StoreError, format::FormatVersion};

/// The sidecar's file name, inside a database's directory.
pub const META_FILE: &str = "APERTURE_META";

/// Where a database is in its life.
///
/// `Writable → Complete` is the whole lifecycle, plus `Broken` for one that failed in
/// a way that leaves it unusable. There is no `Finalizing`: the long work Glean needs
/// that state for happens in an operator-visible phase here instead
/// (`ops-I8`), which is what leaves `finish` with nothing to do but sync and flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Accepts writes. The only state in which facts can be added.
    Writable,
    /// Sealed and immutable. Every write-mode open is refused at establishment,
    /// forever (`ops-I2`).
    Complete,
    /// Something went wrong that leaves the database unusable. Readable if it can
    /// be; never writable.
    Broken,
}

impl Status {
    #[must_use]
    pub fn is_writable(self) -> bool {
        matches!(self, Status::Writable)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Status::Writable => "writable",
            Status::Complete => "complete",
            Status::Broken => "broken",
        };
        f.write_str(text)
    }
}

/// A database's metadata, as it sits on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// The sidecar format's own version — what makes a new field an addition.
    pub version: u32,

    pub name: String,

    /// The provisional instance id: a [ULID](crate::ulid), so instances of one name
    /// sort by creation time.
    ///
    /// Provisional because content-derived identity can only exist at `finish` — it
    /// hashes the base facts. The directory keeps this name afterwards; the content
    /// fingerprint goes in the field below rather than renaming a directory under a
    /// live server.
    pub instance: String,

    pub status: Status,

    /// The on-disk format the database was written with, mirroring the stamp inside
    /// it ([I15](../../../docs/invariants.md#i15)).
    ///
    /// Duplicated on purpose: the stamp is authoritative and requires opening fjall,
    /// and enumeration must not. A disagreement between the two means the sidecar is
    /// stale, which is worth being able to see.
    pub format_codec: u16,
    pub format_storage: u16,

    /// The schema this database was created against, and cannot change
    /// ([I13](../../../docs/invariants.md#i13) once Phase 8 lands).
    pub schema_fingerprint: u64,

    /// `hash(canonical schema, base facts)` — recorded at `finish`, absent before it.
    ///
    /// `ops-I4`: identity is *always* this, and there is no path by which a finished
    /// database carries a random id instead.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content_fingerprint: Option<u64>,

    /// Facts, counted at `finish`.
    ///
    /// Absent while Writable rather than zero, and the distinction is the honest one:
    /// counting requires a scan, `finish` does one anyway for the content
    /// fingerprint, and a number maintained per write would be both expensive and a
    /// second source of truth for something the data already knows.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub facts: Option<u64>,

    /// Bytes on disk, measured at `finish`. Absent before it, for the same reason.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bytes: Option<u64>,

    /// Descriptive only (`ops-I4`) — never an input to identity.
    pub created_at_ms: u64,
}

impl Meta {
    /// The sidecar format this build writes and reads.
    pub const VERSION: u32 = 1;

    /// A fresh sidecar for a database being created.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        instance: impl Into<String>,
        schema_fingerprint: u64,
    ) -> Meta {
        Meta {
            version: Meta::VERSION,
            name: name.into(),
            instance: instance.into(),
            status: Status::Writable,
            format_codec: FormatVersion::CURRENT.codec,
            format_storage: FormatVersion::CURRENT.storage,
            schema_fingerprint,
            content_fingerprint: None,
            facts: None,
            bytes: None,
            created_at_ms: now_ms(),
        }
    }

    /// Read the sidecar in `directory`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if it is missing, unreadable, malformed, or written by a
    /// sidecar format this build does not know.
    pub fn read(directory: &Path) -> Result<Meta, StoreError> {
        let path = directory.join(META_FILE);

        let text = fs::read_to_string(&path).map_err(|source| StoreError::Meta {
            path: path.clone(),
            detail: format!("cannot read: {source}"),
        })?;

        let meta: Meta = serde_json::from_str(&text).map_err(|source| StoreError::Meta {
            path: path.clone(),
            detail: format!("malformed: {source}"),
        })?;

        // Checked after parsing rather than before, so a future sidecar that this
        // build cannot parse says *why* rather than only that it failed.
        if meta.version != Meta::VERSION {
            return Err(StoreError::Meta {
                path,
                detail: format!(
                    "sidecar format version {}, this build writes and reads {}",
                    meta.version,
                    Meta::VERSION
                ),
            });
        }

        Ok(meta)
    }

    /// Write the sidecar into `directory`, atomically.
    ///
    /// Temp file → fsync → rename, then fsync the directory so the rename itself is
    /// durable. Both fsyncs matter: without the first the renamed file can contain
    /// nothing after a crash, and without the second the rename can be lost — which
    /// for the `finish` flip would be exactly the observable `ops-I3` forbids.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if any step fails.
    pub fn write(&self, directory: &Path) -> Result<(), StoreError> {
        let path = directory.join(META_FILE);
        let temp = directory.join(format!("{META_FILE}.tmp"));

        let fail = |detail: String| StoreError::Meta {
            path: path.clone(),
            detail,
        };

        let mut json = serde_json::to_string_pretty(self)
            .map_err(|source| fail(format!("cannot serialise: {source}")))?;
        json.push('\n');

        {
            let mut file = fs::File::create(&temp)
                .map_err(|source| fail(format!("cannot create {}: {source}", temp.display())))?;
            file.write_all(json.as_bytes())
                .map_err(|source| fail(format!("cannot write: {source}")))?;
            file.sync_all()
                .map_err(|source| fail(format!("cannot sync: {source}")))?;
        }

        fs::rename(&temp, &path)
            .map_err(|source| fail(format!("cannot rename into place: {source}")))?;

        sync_dir(directory).map_err(|source| fail(format!("cannot sync directory: {source}")))?;

        Ok(())
    }
}

/// Milliseconds since the epoch, or 0 if the clock is before it.
///
/// Saturating rather than erroring: a wrong `created_at` is a cosmetic fault in a
/// descriptive field, and failing a create over a misconfigured clock would not be.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// fsync a directory, so a rename inside it is durable.
///
/// Opening a directory read-only and syncing it is the portable-enough incantation;
/// on Linux it is what makes a rename survive a power loss.
pub(crate) fn sync_dir(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// A directory that is removed when dropped, unless it is disarmed.
///
/// What makes `create` all-or-nothing: the database is built under a temporary name
/// and only renamed into place once it is complete, so a failure anywhere leaves
/// nothing behind rather than a half-built database the catalog would list.
pub(crate) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub(crate) fn new(path: PathBuf) -> Scratch {
        Scratch { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    /// **Always removed**, and there is no longer a way to disarm it.
    ///
    /// There used to be: `create` renamed the scratch directory itself into place and
    /// had to stop the drop deleting it. Now the finished *instance* directory is
    /// renamed out from under the scratch, so what is left is always an empty directory
    /// this owns — on the failure paths and on the happy one alike.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta {
        Meta::new("code", "01J000000000000000000000", 0xDEAD_BEEF)
    }

    #[test]
    fn a_sidecar_round_trips() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let written = meta();

        written.write(dir.path()).expect("it writes");
        assert_eq!(Meta::read(dir.path()).expect("it reads"), written);
    }

    /// A new database has no content fingerprint, no fact count and no size, and the
    /// sidecar says so by **omitting** them rather than by writing zeros — a zero
    /// fact count on a Writable database would be a claim, and the truth is that
    /// nobody has counted.
    #[test]
    fn the_unknowns_are_absent_rather_than_zero() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        meta().write(dir.path()).expect("it writes");

        let text = fs::read_to_string(dir.path().join(META_FILE)).expect("it reads");

        for absent in ["content_fingerprint", "facts", "bytes"] {
            assert!(
                !text.contains(absent),
                "{absent} should be omitted:\n{text}"
            );
        }
    }

    /// The two fields `ops-I6` and §5 say must not exist. Asserted on the *written
    /// bytes*, because the point is what a reader of the file can find.
    #[test]
    fn the_deliberate_absences_stay_absent() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        meta().write(dir.path()).expect("it writes");

        let text = fs::read_to_string(dir.path().join(META_FILE)).expect("it reads");
        assert!(!text.contains("externally_modified"), "ops-I6 forbids it");
        assert!(
            !text.contains("provenance"),
            "§5 records it as a later addition"
        );
    }

    /// A sidecar from a future build is refused by name rather than parsed
    /// optimistically — the same discipline the on-disk format stamp follows.
    #[test]
    fn a_future_sidecar_is_refused() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let mut future = meta();
        future.version = Meta::VERSION + 1;
        future.write(dir.path()).expect("it writes");

        let error = Meta::read(dir.path()).expect_err("a future version");
        assert!(
            format!("{error}").contains("sidecar format version"),
            "{error}"
        );
    }

    #[test]
    fn a_malformed_sidecar_is_refused() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        fs::write(dir.path().join(META_FILE), b"{not json").expect("it writes");

        assert!(Meta::read(dir.path()).is_err());
    }

    /// **The temp file is never left behind**, which matters because the catalog
    /// scans directories: a stray `APERTURE_META.tmp` that a later read picked up
    /// would be a half-written sidecar presented as a database.
    #[test]
    fn writing_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        meta().write(dir.path()).expect("it writes");
        meta().write(dir.path()).expect("and again");

        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("a listing")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        assert_eq!(names, vec![META_FILE.to_owned()]);
    }

    /// A rewrite replaces the file rather than appending to or truncating it in
    /// place — the property the rename buys, checked by shrinking the document.
    #[test]
    fn a_rewrite_replaces_rather_than_overlays() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let mut long = meta();
        long.name = "a-very-long-database-name-indeed".to_owned();
        long.content_fingerprint = Some(u64::MAX);
        long.write(dir.path()).expect("it writes");

        let short = meta();
        short.write(dir.path()).expect("it writes again");

        assert_eq!(Meta::read(dir.path()).expect("it reads"), short);
    }

    /// **A scratch directory is always removed**, and what it holds goes with it.
    ///
    /// There used to be a way to keep one, because `create` renamed the scratch itself
    /// into place. Now the finished instance directory is renamed out from under it, so
    /// every path — failure and success alike — leaves this to clean up.
    #[test]
    fn a_scratch_directory_is_always_removed() {
        let dir = tempfile::tempdir().expect("a scratch directory");

        let doomed = dir.path().join("doomed");
        fs::create_dir(&doomed).expect("it is made");
        fs::write(doomed.join("half-built"), b"...").expect("it is written");

        drop(Scratch::new(doomed.clone()));

        assert!(!doomed.exists(), "the scratch and its contents are removed");
    }
}
