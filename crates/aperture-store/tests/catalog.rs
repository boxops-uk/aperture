//! The store root, against a real filesystem and real fjall databases.
//!
//! An integration test because the claims are about directories: what survives a
//! failure, what a listing can see while a database is held open, and what a second
//! process is refused. None of that is observable from inside a unit test of the
//! types involved.

use std::{fs, sync::Arc};

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_store::{
    catalog::{Catalog, Intent, LOCK_FILE, Selector},
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

/// Seal an instance, allowing zero facts — these tests are about directories, not
/// about what is in them.
fn seal(catalog: &Catalog, entry: &aperture_store::catalog::Entry) {
    catalog
        .finish(
            &Selector::at(entry.name(), &entry.meta.instance),
            &schema(),
            true,
        )
        .expect("it seals");
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
    let entry = catalog.create("code", &schema()).expect("it creates");

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

    // **The schema copy is the schema**, at the same positions — which is what makes a
    // database self-describing rather than merely annotated. Positions matter: they are
    // the tag in every FactId it will hold.
    let embedded = schema_doc::read(&entry.path)
        .expect("the schema copy")
        .expect("there is one");

    assert!(aperture_schema::syntax::print::equivalent(
        &schema(),
        &embedded
    ));
    assert_eq!(
        aperture_schema::fingerprint::of(&embedded),
        entry.meta.schema_fingerprint,
        "the copy and the number the sidecar records are of the same schema"
    );
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
    catalog.create("code", &schema()).expect("it creates");

    let (_entry, reopened) = catalog
        .open_read(&Selector::of("code"))
        .expect("it reopens");

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
    catalog.create("alpha", &schema()).expect("it creates");
    catalog.create("beta", &schema()).expect("it creates");

    let (_entry, held) = catalog
        .open_write(&Selector::of("alpha"))
        .expect("it opens");

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
                catalog.create(bad, &schema()),
                Err(StoreError::BadDatabaseName { .. })
            ),
            "{bad:?} should be refused"
        );
    }
}

/// **`ops-I2`: once Complete, no writable handle exists.** Refused at establishment
/// rather than defended per write, so immutability is the absence of a thing.
#[test]
fn a_complete_database_cannot_be_opened_for_writing() {
    let (_dir, catalog) = catalog();
    let entry = catalog.create("code", &schema()).expect("it creates");

    // Sealing is 9b's; this reaches in to set the status so the *refusal* can be
    // tested now, on the establishment path that will still be the one enforcing it.
    let mut meta = entry.meta.clone();
    meta.status = Status::Complete;
    meta.write(&entry.path).expect("it writes");

    match catalog.open_write(&Selector::of("code")).map(|_| ()) {
        Err(StoreError::NotWritable { name, status }) => {
            assert_eq!(name, "code");
            assert_eq!(status, Status::Complete);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // ...and reading it is still fine, which is the whole point of sealing one.
    catalog
        .open_read(&Selector::of("code"))
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

    // A *file* named `code`: the name directory cannot be created over it, so the
    // create fails where it touches the filesystem. Not a name clash — a name holding
    // instances is the ordinary case now — but a directory that cannot exist.
    fs::write(catalog.root().join("code"), b"in the way").expect("it writes");

    assert!(matches!(
        catalog.create("code", &schema()),
        Err(StoreError::Meta { .. })
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
    catalog.create("real", &schema()).expect("it creates");

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
    catalog.create("good", &schema()).expect("it creates");
    let broken = catalog.create("bad", &schema()).expect("it creates");

    fs::write(broken.path.join(META_FILE), b"{not json").expect("it writes");

    let listing = catalog.list().expect("it still lists");
    assert_eq!(
        listing.entries.iter().map(|e| e.name()).collect::<Vec<_>>(),
        vec!["good"]
    );
    assert_eq!(listing.problems.len(), 1);
    assert!(format!("{}", listing.problems[0]).contains("malformed"));
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
    catalog.create("code", &schema()).expect("it creates");

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
    let created = catalog.create("code", &schema()).expect("it creates");

    let found = catalog
        .resolve(&Selector::of("code"), Intent::Read)
        .expect("it is found");

    assert_eq!(found.meta, created.meta);
    assert_eq!(
        found.meta.schema_fingerprint,
        aperture_schema::fingerprint::of(&schema()),
        "the number recorded is the schema's own identity, not one a caller supplied"
    );
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
                    .open_read(&Selector::of("code"))
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
        .create("alpha", &schema())
        .expect("the first database");

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        // `abort`, not `exit`: no destructors, so the scratch guard does not get to
        // clean up. That is the case being tested.
        std::process::abort();
    });

    // Whatever this returns is irrelevant — the watchdog is expected to win. If it
    // somehow does not, the parent's `!status.success()` catches it.
    let _ = catalog.create("code", &schema());

    // Keep the process alive long enough for the watchdog even if `create` was fast.
    std::thread::sleep(std::time::Duration::from_millis(delay_ms + 500));
}

// ---------------------------------------------------------------------------
// Many instances under one name — the Glean `Repo = (name, hash)` shape, with a
// generated ULID where Glean takes a caller-supplied revision.
// ---------------------------------------------------------------------------

/// **A name holds instances, and `create` adds one.** This replaces the old rule that a
/// second `create` under a live name was refused: the whole point of an instance is that
/// a database-per-CI-run has somewhere to go.
#[test]
fn creating_the_same_name_twice_makes_a_second_instance() {
    let (_dir, catalog) = catalog();

    let first = catalog.create("code", &schema()).expect("it creates");
    let second = catalog.create("code", &schema()).expect("it creates again");

    assert_ne!(first.meta.instance, second.meta.instance);

    let listing = catalog.list().expect("it lists");
    assert_eq!(listing.entries.len(), 2, "{:?}", listing.problems);
    assert!(listing.entries.iter().all(|entry| entry.name() == "code"));

    // Both live under one name directory, each in its own instance directory.
    for entry in &listing.entries {
        assert_eq!(
            entry.path,
            catalog.root().join("code").join(&entry.meta.instance)
        );
        assert!(entry.path.join(META_FILE).is_file());
    }
}

/// **A bare name means the newest instance worth reading.** Ranked sealed-first, then
/// newest — which is Glean's rule (`filter Complete`, `sortBy created descending`) with
/// the fallback the single-instance case needs.
#[test]
fn a_bare_name_reads_the_newest_sealed_instance() {
    let (_dir, catalog) = catalog();

    let old = catalog.create("code", &schema()).expect("it creates");
    seal(&catalog, &old);
    let new = catalog.create("code", &schema()).expect("it creates again");

    let chosen = catalog
        .resolve(&Selector::of("code"), Intent::Read)
        .expect("it resolves");

    assert_eq!(
        chosen.meta.instance, old.meta.instance,
        "a sealed instance outranks a newer unsealed one"
    );
    assert_ne!(chosen.meta.instance, new.meta.instance);
}

/// The other half of the ranking: with nothing sealed, newest wins — so a root holding
/// one Writable database is still readable by name, which is what it was before.
#[test]
fn a_bare_name_reads_the_newest_instance_when_none_is_sealed() {
    let (_dir, catalog) = catalog();

    let _first = catalog.create("code", &schema()).expect("it creates");
    let second = catalog.create("code", &schema()).expect("it creates again");

    let chosen = catalog
        .resolve(&Selector::of("code"), Intent::Read)
        .expect("it resolves");

    assert_eq!(chosen.meta.instance, second.meta.instance);
}

/// **A writer wants the Writable one, not the newest one.** `ops-I2` makes that
/// unambiguous when exactly one is unsealed.
#[test]
fn a_bare_name_writes_to_the_only_writable_instance() {
    let (_dir, catalog) = catalog();

    let sealed = catalog.create("code", &schema()).expect("it creates");
    seal(&catalog, &sealed);
    let open = catalog.create("code", &schema()).expect("it creates again");

    let chosen = catalog
        .resolve(&Selector::of("code"), Intent::Write)
        .expect("it resolves");

    assert_eq!(chosen.meta.instance, open.meta.instance);
}

/// **Two Writable instances is a question, not a guess.** Reading the wrong one answers
/// oddly; writing to the wrong one is unrecoverable, so this refuses and names them.
#[test]
fn a_bare_name_refuses_to_write_when_two_are_writable() {
    let (_dir, catalog) = catalog();

    let a = catalog.create("code", &schema()).expect("it creates");
    let b = catalog.create("code", &schema()).expect("it creates again");

    match catalog.resolve(&Selector::of("code"), Intent::Write) {
        Err(StoreError::AmbiguousDatabase { name, instances }) => {
            assert_eq!(name, "code");
            assert!(instances.contains(&a.meta.instance), "{instances:?}");
            assert!(instances.contains(&b.meta.instance), "{instances:?}");
        }
        other => panic!("expected an ambiguity, got {other:?}"),
    }
}

/// A prefix is enough, because a ULID is 26 characters and nobody is typing one.
#[test]
fn an_instance_prefix_selects_one() {
    let (_dir, catalog) = catalog();

    let _other = catalog.create("code", &schema()).expect("it creates");
    let wanted = catalog.create("code", &schema()).expect("it creates again");

    // A ULID's first ten characters are the millisecond timestamp, so two made in the
    // same millisecond share them; the entropy that follows is what makes a prefix
    // selective, and this takes enough of it to be sure.
    let prefix = &wanted.meta.instance[..14];
    let chosen = catalog
        .resolve(&Selector::at("code", prefix), Intent::Read)
        .expect("it resolves");

    assert_eq!(chosen.meta.instance, wanted.meta.instance);
}

/// A prefix matching two instances is refused rather than resolved to either.
#[test]
fn an_ambiguous_instance_prefix_is_refused() {
    let (_dir, catalog) = catalog();

    catalog.create("code", &schema()).expect("it creates");
    catalog.create("code", &schema()).expect("it creates again");

    // The empty prefix matches everything, which is the sharpest form of ambiguous.
    assert!(matches!(
        catalog.resolve(&Selector::at("code", ""), Intent::Read),
        Err(StoreError::AmbiguousDatabase { .. })
    ));
}

#[test]
fn an_unknown_instance_is_refused_by_name() {
    let (_dir, catalog) = catalog();
    catalog.create("code", &schema()).expect("it creates");

    match catalog.resolve(&Selector::at("code", "ZZZZ"), Intent::Read) {
        Err(StoreError::NoSuchInstance { name, instance }) => {
            assert_eq!((name.as_str(), instance.as_str()), ("code", "ZZZZ"));
        }
        other => panic!("expected no-such-instance, got {other:?}"),
    }
}

/// **Removing an instance leaves its siblings.** The old rule renamed the whole name
/// directory, which with two instances would take a database nobody asked about.
#[test]
fn removing_one_instance_leaves_the_others() {
    let (_dir, catalog) = catalog();

    let doomed = catalog.create("code", &schema()).expect("it creates");
    let spared = catalog.create("code", &schema()).expect("it creates again");

    catalog
        .remove(&Selector::at("code", &doomed.meta.instance))
        .expect("it removes");

    let listing = catalog.list().expect("it lists");
    assert_eq!(listing.entries.len(), 1, "{:?}", listing.problems);
    assert_eq!(listing.entries[0].meta.instance, spared.meta.instance);
    assert!(spared.path.is_dir());
    assert!(!doomed.path.exists());

    // The name directory survives, because something is still under it.
    assert!(catalog.root().join("code").is_dir());
}

/// And taking the last one takes the name with it, so an empty directory is not left to
/// make `code` look like it still exists.
#[test]
fn removing_the_last_instance_removes_the_name() {
    let (_dir, catalog) = catalog();
    catalog.create("code", &schema()).expect("it creates");

    catalog.remove(&Selector::of("code")).expect("it removes");

    assert!(!catalog.root().join("code").exists());
    assert!(matches!(
        catalog.remove(&Selector::of("code")),
        Err(StoreError::NoSuchDatabase(_))
    ));
}

/// Destructive and ambiguous is refused: `rm code` with three instances is a question.
#[test]
fn removing_a_bare_name_is_refused_when_several_exist() {
    let (_dir, catalog) = catalog();
    catalog.create("code", &schema()).expect("it creates");
    catalog.create("code", &schema()).expect("it creates again");

    assert!(matches!(
        catalog.remove(&Selector::of("code")),
        Err(StoreError::AmbiguousDatabase { .. })
    ));
}

/// `@` separates a name from an instance, so a name may not contain one — otherwise
/// `a@b` names either a database or an instance of one, and nothing can say which.
#[test]
fn a_name_containing_the_instance_separator_is_refused() {
    let (_dir, catalog) = catalog();

    match catalog.create("code@old", &schema()) {
        Err(StoreError::BadDatabaseName { name, .. }) => assert_eq!(name, "code@old"),
        other => panic!("expected a bad name, got {other:?}"),
    }
}

#[test]
fn a_selector_round_trips_through_its_text_form() {
    for text in ["code", "code@01JQ", "aperture.db"] {
        let selector = Selector::parse(text).expect("it parses");
        assert_eq!(selector.to_string(), text);
    }

    // A bare `@` names no instance, and an empty name is not a name.
    assert!(Selector::parse("code@").is_err());
    assert!(Selector::parse("@01JQ").is_err());
    assert!(Selector::parse("a@b@c").is_err());
}

/// **A listing is in resolution order**, so the first row shown for a name is the one an
/// unqualified read of that name binds. The two used to be sorted differently, which made
/// the listing quietly misleading the moment a name held two instances.
#[test]
fn a_listing_shows_a_names_instances_in_the_order_resolution_picks_them() {
    let (_dir, catalog) = catalog();

    let older = catalog.create("code", &schema()).expect("it creates");
    let newer = catalog.create("code", &schema()).expect("it creates again");

    // Both unsealed: newest first.
    let listed = catalog.list().expect("it lists");
    let order: Vec<&str> = listed
        .entries
        .iter()
        .map(|entry| entry.meta.instance.as_str())
        .collect();
    assert_eq!(order, vec![&newer.meta.instance, &older.meta.instance]);

    // Seal the older one and it moves to the front, because sealed outranks newer — and
    // that is exactly where an unqualified read now goes.
    seal(&catalog, &older);

    let listed = catalog.list().expect("it lists");
    assert_eq!(listed.entries[0].meta.instance, older.meta.instance);
    assert_eq!(
        catalog
            .resolve(&Selector::of("code"), Intent::Read)
            .expect("it resolves")
            .meta
            .instance,
        listed.entries[0].meta.instance,
        "the head of the listing is what a bare name reads"
    );
}
