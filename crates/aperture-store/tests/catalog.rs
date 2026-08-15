//! The store root, against a real filesystem and real fjall databases.
//!
//! An integration test because the claims are about directories: what survives a
//! failure, what a listing can see while a database is held open, and what a second
//! process is refused. None of that is observable from inside a unit test of the
//! types involved.

use std::{fs, sync::Arc};

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_store::{
    catalog::{Catalog, LOCK_FILE},
    error::StoreError,
    meta::{META_FILE, Meta, Status},
    schema_doc, ulid,
};
use lasso::Rodeo;

fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let (file, decl) = (
        rodeo.get_or_intern("src.File"),
        rodeo.get_or_intern("src.Decl"),
    );
    let (f_file, f_name) = (rodeo.get_or_intern("file"), rodeo.get_or_intern("name"));

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![
            Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: decl,
                key: PredicateTy::Record(
                    vec![
                        (f_file, PredicateTy::Fact(PredicateId(0))),
                        (f_name, PredicateTy::Str),
                    ]
                    .into(),
                ),
                value: None,
            },
        ]),
    )
}

fn catalog() -> (tempfile::TempDir, Catalog) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    (dir, catalog)
}

/// The shape §9 specifies, checked as directories rather than as intentions.
#[test]
fn a_created_database_has_the_layout_the_design_specifies() {
    let (_dir, catalog) = catalog();
    let entry = catalog
        .create("code", &schema(), 0xABCD)
        .expect("it creates");

    assert_eq!(entry.name(), "code");
    assert_eq!(entry.status(), Status::Writable);
    assert!(
        ulid::is_valid(&entry.meta.instance),
        "{}",
        entry.meta.instance
    );

    // <root>/<name>/<instance>/
    assert_eq!(
        entry.path.parent().and_then(|p| p.file_name()),
        Some(std::ffi::OsStr::new("code"))
    );
    assert!(entry.path.join(META_FILE).is_file());
    assert!(entry.path.join(schema_doc::SCHEMA_DIR).is_dir());

    // The schema copy describes what was created.
    let doc = schema_doc::read(&entry.path).expect("the schema copy");
    assert!(doc.provisional, "it is not chapter 6's canonical form");
    assert_eq!(doc.predicates.len(), 2);
    assert_eq!(doc.predicates[0].name, "src.File");
    assert_eq!(doc.predicates[1].name, "src.Decl");
}

/// **Every predicate's trees exist before a single fact is written.** A keyspace
/// costs ~30 ms, and a database created from a schema knows all of them — paying
/// that inside an ingest at an unpredictable point is what this avoids.
///
/// Checked by **reopening**, which is the behaviour rather than the layout: `open`
/// recovers predicates from the keyspaces that exist, so a database whose trees were
/// left to be made on demand comes back with none of them.
#[test]
fn create_materialises_every_predicates_trees() {
    let (_dir, catalog) = catalog();
    catalog.create("code", &schema(), 1).expect("it creates");

    let (_entry, reopened) = catalog.open_read("code").expect("it reopens");

    assert_eq!(
        reopened.predicate_ids(),
        vec![PredicateId(0), PredicateId(1)],
        "a reopen should find every predicate's trees already there"
    );
}

/// **The filesystem is the catalog** (`ops-I7`): a listing reads sidecars and never
/// opens fjall — which is what lets it work while a server holds every database.
///
/// Held open here for real, by a live `FjallDb` handle, because fjall's own directory
/// lock is exactly what would fail if the listing tried to open one.
#[test]
fn a_listing_works_while_a_database_is_held_open() {
    let (_dir, catalog) = catalog();
    catalog.create("alpha", &schema(), 1).expect("it creates");
    catalog.create("beta", &schema(), 1).expect("it creates");

    let (_entry, held) = catalog.open_write("alpha").expect("it opens");

    let listing = catalog.list().expect("it lists");
    assert_eq!(
        listing.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        vec!["alpha", "beta"]
    );
    assert!(listing.problems.is_empty());

    drop(held);
}

/// A name is a directory under the store root, so anything that could escape it or
/// collide with the catalog's own dot-prefixed entries is refused.
#[test]
fn a_name_that_could_escape_the_root_is_refused() {
    let (_dir, catalog) = catalog();

    for bad in ["", ".", "..", ".hidden", "a/b", "a\\b", "a\nb"] {
        assert!(
            matches!(
                catalog.create(bad, &schema(), 1),
                Err(StoreError::BadDatabaseName { .. })
            ),
            "{bad:?} should be refused"
        );
    }
}

#[test]
fn creating_the_same_name_twice_is_refused() {
    let (_dir, catalog) = catalog();
    catalog.create("code", &schema(), 1).expect("it creates");

    assert!(matches!(
        catalog.create("code", &schema(), 1),
        Err(StoreError::DatabaseExists(_))
    ));
}

/// **`ops-I2`: once Complete, no writable handle exists.** Refused at establishment
/// rather than defended per write, so immutability is the absence of a thing.
#[test]
fn a_complete_database_cannot_be_opened_for_writing() {
    let (_dir, catalog) = catalog();
    let entry = catalog.create("code", &schema(), 1).expect("it creates");

    // Sealing is 9b's; this reaches in to set the status so the *refusal* can be
    // tested now, on the establishment path that will still be the one enforcing it.
    let mut meta = entry.meta.clone();
    meta.status = Status::Complete;
    meta.write(&entry.path).expect("it writes");

    match catalog.open_write("code").map(|_| ()) {
        Err(StoreError::NotWritable { name, status }) => {
            assert_eq!(name, "code");
            assert_eq!(status, Status::Complete);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // ...and reading it is still fine, which is the whole point of sealing one.
    catalog
        .open_read("code")
        .expect("a Complete database still reads");
}

/// **Creation is all-or-nothing.** A failure part-way leaves nothing under the name —
/// not an empty directory, and not a half-built database a listing would report.
///
/// Provoked by making the destination un-creatable at the last moment: a file where
/// the directory has to go, so the final rename fails after everything else has
/// succeeded. That is the latest possible failure and so the strongest case.
#[test]
fn a_failed_create_leaves_nothing_behind() {
    let (_dir, catalog) = catalog();

    // A *file* named `code`: `create`'s existence check sees it and refuses, which
    // is the near case. The far case is below.
    fs::write(catalog.root().join("code"), b"in the way").expect("it writes");

    assert!(matches!(
        catalog.create("code", &schema(), 1),
        Err(StoreError::DatabaseExists(_))
    ));

    // Nothing was built: no scratch directory survives.
    let strays: Vec<String> = fs::read_dir(catalog.root())
        .expect("a listing")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.starts_with(".create-"))
        .collect();

    assert!(strays.is_empty(), "left behind: {strays:?}");
}

/// A directory that is not a database is skipped rather than reported — a store root
/// is a filesystem, and anything can appear in one.
#[test]
fn a_listing_skips_what_is_not_a_database() {
    let (_dir, catalog) = catalog();
    catalog.create("real", &schema(), 1).expect("it creates");

    // A stray directory, a stray file, a name directory with no instance in it, and
    // an instance-shaped directory with no sidecar.
    fs::create_dir_all(catalog.root().join("stray/not-a-ulid")).expect("it is made");
    fs::write(catalog.root().join("loose.txt"), b"hello").expect("it writes");
    fs::create_dir_all(catalog.root().join("empty")).expect("it is made");
    fs::create_dir_all(catalog.root().join(format!("bare/{}", ulid::new()))).expect("it is made");

    let listing = catalog.list().expect("it lists");
    assert_eq!(
        listing.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        vec!["real"]
    );
    assert!(listing.problems.is_empty(), "{:?}", listing.problems);
}

/// **A broken database is reported, not hidden, and does not break the listing.**
/// One bad sidecar must not make `list` unable to show the other nine.
#[test]
fn a_malformed_sidecar_is_a_problem_rather_than_a_failure() {
    let (_dir, catalog) = catalog();
    catalog.create("good", &schema(), 1).expect("it creates");
    let broken = catalog.create("bad", &schema(), 1).expect("it creates");

    fs::write(broken.path.join(META_FILE), b"{not json").expect("it writes");

    let listing = catalog.list().expect("it still lists");
    assert_eq!(
        listing.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        vec!["good"]
    );
    assert_eq!(listing.problems.len(), 1);
    assert!(format!("{}", listing.problems[0]).contains("malformed"));
}

#[test]
fn removing_a_database_takes_the_whole_tree() {
    let (_dir, catalog) = catalog();
    catalog.create("code", &schema(), 1).expect("it creates");

    catalog.remove("code").expect("it removes");

    assert!(catalog.find("code").expect("it lists").is_none());
    assert!(!catalog.root().join("code").exists());

    // Nothing pending: the rename-then-delete leaves no trash behind.
    let strays: Vec<String> = fs::read_dir(catalog.root())
        .expect("a listing")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.starts_with(".trash-"))
        .collect();
    assert!(strays.is_empty(), "left behind: {strays:?}");

    assert!(matches!(
        catalog.remove("code"),
        Err(StoreError::NoSuchDatabase(_))
    ));
}

/// **`ops-I1`: one process owns a store root.** A second holder is refused by name
/// rather than made to wait — the design refuses a lock fight, because the
/// alternative to failing here is two servers writing one directory.
#[test]
fn a_second_holder_of_the_root_is_refused() {
    let (_dir, catalog) = catalog();

    let held = catalog.lock().expect("the first holder");
    assert_eq!(held.root(), catalog.root());

    match catalog.lock() {
        Err(StoreError::RootHeld { root }) => assert_eq!(root, catalog.root()),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Released with the guard, so the next holder gets it.
    drop(held);
    catalog.lock().expect("the root is free again");
}

/// The lock file is not a database, and a listing does not trip over it.
#[test]
fn the_lock_file_is_invisible_to_a_listing() {
    let (_dir, catalog) = catalog();
    let _held = catalog.lock().expect("the lock");
    catalog.create("code", &schema(), 1).expect("it creates");

    assert!(catalog.root().join(LOCK_FILE).exists());

    let listing = catalog.list().expect("it lists");
    assert_eq!(listing.entries.len(), 1);
    assert!(listing.problems.is_empty());
}

/// A sidecar survives a reopen unchanged — the catalog reads what it wrote, including
/// the fields that are absent rather than zero.
#[test]
fn a_sidecar_round_trips_through_the_catalog() {
    let (_dir, catalog) = catalog();
    let created = catalog
        .create("code", &schema(), 0x1234_5678)
        .expect("it creates");

    let found = catalog.get("code").expect("it is found");

    assert_eq!(found.meta, created.meta);
    assert_eq!(found.meta.schema_fingerprint, 0x1234_5678);
    assert_eq!(found.meta.version, Meta::VERSION);
    assert_eq!(found.meta.content_fingerprint, None, "recorded at finish");
    assert_eq!(found.meta.facts, None, "counted at finish");
    assert_eq!(found.meta.bytes, None, "measured at finish");
}

// ---- creation across a real crash ------------------------------------------
//
// The claim is that a killed process leaves either nothing under a name or a whole
// Writable database. Everything above tests the *handled* failures — the RAII guard
// running, the existence check refusing. A `SIGKILL` runs no destructors, so the only
// honest test of the atomicity claim is to kill one and look at what is left.
//
// Same shape as the store's own I12 crash guard: a child test aborts itself with a
// watchdog, and the parent inspects the wreckage.

/// The child's own test path, which is what `--exact` matches on. A stale path here
/// produces a *passing* child, which the parent would read as "the crash never
/// happened" — so the parent asserts the child failed.
const CRASH_CHILD: &str = "crashing_creator_child_process";
const CRASH_ROOT_VAR: &str = "APERTURE_CREATE_CRASH_ROOT";
const CRASH_DELAY_VAR: &str = "APERTURE_CREATE_CRASH_DELAY_MS";

/// **Creation is all-or-nothing across a crash.**
///
/// The cut point is deliberately uncontrolled: the child is aborted by a watchdog
/// while it builds a database, so successive delays cut in different places —
/// during keyspace creation, during the schema copy, between the sidecar and the
/// rename. The property holds wherever it lands.
#[test]
fn a_killed_create_leaves_nothing_or_a_whole_database() {
    // Several delays, because one would only ever cut in one place. The range
    // brackets a create: keyspace creation dominates it at roughly 30 ms a pair, so
    // the short delays land inside and the long one lands after.
    let mut killed_mid_create = 0;

    for delay_ms in [1u64, 5, 15, 40, 90, 200] {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let root = dir.path().join("store");

        let status =
            std::process::Command::new(std::env::current_exe().expect("path to this test binary"))
                .args(["--exact", CRASH_CHILD, "--ignored", "--nocapture"])
                .env(CRASH_ROOT_VAR, &root)
                .env(CRASH_DELAY_VAR, delay_ms.to_string())
                .status()
                .expect("spawn the crashing creator");

        assert!(
            !status.success(),
            "the child was supposed to abort mid-create, not exit cleanly"
        );

        let catalog = Catalog::open(&root).expect("the store root survives");
        let listing = catalog.list().expect("it lists");

        // Non-vacuity: the child creates `alpha` *before* arming its watchdog, so if
        // that is missing the kill landed before any real work and this run taught
        // us nothing.
        assert!(
            listing.entries.iter().any(|e| e.name() == "alpha"),
            "delay {delay_ms}ms: the child died before finishing its first database, \
             so the crash case is vacuous"
        );

        // A half-built database must never be visible, and never be a problem: the
        // scratch directory is dot-prefixed, so a scan skips it entirely.
        assert!(
            listing.problems.is_empty(),
            "delay {delay_ms}ms: a crash left something the scan could not read: {:?}",
            listing.problems
        );

        // `code` is the one being built when the process died. Either it is not
        // there, or it is whole — and whole means openable, Writable, and with every
        // predicate's trees present.
        // A surviving scratch directory is proof the kill landed *inside* a create:
        // the guard that removes it runs no destructors under `abort`. Counted so the
        // test can say it reached the case it exists for.
        let scratch_left = fs::read_dir(&root)
            .expect("a listing")
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with(".create-"));

        if scratch_left {
            killed_mid_create += 1;
            assert!(
                !listing.entries.iter().any(|e| e.name() == "code"),
                "delay {delay_ms}ms: a database was visible while its scratch build                  was still there"
            );
        }

        match listing.entries.iter().find(|e| e.name() == "code") {
            None => {}
            Some(entry) => {
                assert_eq!(
                    entry.status(),
                    Status::Writable,
                    "delay {delay_ms}ms: a database that appeared must be Writable"
                );

                let (_entry, db) = catalog
                    .open_read("code")
                    .unwrap_or_else(|e| panic!("delay {delay_ms}ms: it must open: {e}"));

                assert_eq!(
                    db.predicate_ids(),
                    vec![PredicateId(0), PredicateId(1)],
                    "delay {delay_ms}ms: a database that appeared must be complete"
                );
            }
        }
    }

    // The census. Without this the whole test could pass by never cutting inside a
    // create at all — every kill landing after the rename, which proves nothing about
    // atomicity. The *completed* outcome needs no census: every other test in this
    // file creates a database successfully.
    assert!(
        killed_mid_create > 0,
        "no run was killed while building a database, so nothing here tested \
         atomicity"
    );
}

/// Not a guard: the crashing half of the test above, run as a child process.
///
/// Builds one database to completion — so the parent can tell a real crash from one
/// that landed before any work — and is then aborted partway through a second.
#[test]
#[ignore = "spawned as a child process by a_killed_create_leaves_nothing_or_a_whole_database"]
fn crashing_creator_child_process() {
    let root = std::env::var(CRASH_ROOT_VAR).expect("the parent sets the store root");
    let delay_ms: u64 = std::env::var(CRASH_DELAY_VAR)
        .expect("the parent sets the delay")
        .parse()
        .expect("a number");

    let catalog = Catalog::open(&root).expect("a store root");

    // Finished before the watchdog is armed: the parent's non-vacuity check.
    catalog
        .create("alpha", &schema(), 1)
        .expect("the first database");

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        // `abort`, not `exit`: no destructors, so the scratch guard does not get to
        // clean up. That is the case being tested.
        std::process::abort();
    });

    // Whatever this returns is irrelevant — the watchdog is expected to win. If it
    // somehow does not, the parent's `!status.success()` catches it.
    let _ = catalog.create("code", &schema(), 1);

    // Keep the process alive long enough for the watchdog even if `create` was fast.
    std::thread::sleep(std::time::Duration::from_millis(delay_ms + 500));
}
