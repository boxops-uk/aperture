//! Sealing a database, and the identity that makes sealing mean something.
//!
//! The claims here are all about *two* databases — built from the same facts in
//! different orders, or from different facts — so almost every test builds a pair and
//! compares their fingerprints. That is the only shape in which `ops-I4` says
//! anything: "a DB built twice from identical inputs is identical" is not a property
//! of one database.

use std::sync::Arc;

use aperture_schema::schema::{Predicate, PredicateId, PredicateTy, Schema};
use aperture_store::{
    catalog::{Catalog, Intent, Selector},
    error::StoreError,
    fact::{Fact, ToValue, record},
    identity,
    meta::Status,
    store::FjallDb,
};
use lasso::Rodeo;

const FILE: PredicateId = PredicateId(0);
const MODULE: PredicateId = PredicateId(1);

/// `src.File : string`, `src.Module : { file : src.File, name : string }`,
/// `src.Decl : { module : src.Module, name : string } -> string`.
///
/// Two levels of reference, so the identity walk has a chain to expand rather than a
/// single hop — which is where "expand recursively" stops being a word.
fn schema() -> Schema {
    let mut rodeo = Rodeo::new();
    let mut sym = |name: &str| rodeo.get_or_intern(name);

    let (file, module, decl) = (sym("src.File"), sym("src.Module"), sym("src.Decl"));
    let (f_file, f_module, f_name) = (sym("file"), sym("module"), sym("name"));

    Schema::new(
        rodeo.into_reader(),
        Arc::from(vec![
            Predicate {
                name: file,
                key: PredicateTy::Str,
                value: None,
            },
            Predicate {
                name: module,
                key: PredicateTy::Record(Arc::from([
                    (f_file, PredicateTy::Fact(FILE)),
                    (f_name, PredicateTy::Str),
                ])),
                value: None,
            },
            Predicate {
                name: decl,
                key: PredicateTy::Record(Arc::from([
                    (f_module, PredicateTy::Fact(MODULE)),
                    (f_name, PredicateTy::Str),
                ])),
                value: Some(PredicateTy::Str),
            },
        ]),
    )
}

// ---- facts, written by hand through the typed seam --------------------------

struct FileFact(&'static str);

impl Fact for FileFact {
    const PREDICATE: &'static str = "src.File";
    fn key(&self) -> aperture_encoding::tuple::Value {
        self.0.to_value()
    }
}

struct ModuleFact {
    file: aperture_schema::id::FactId,
    name: &'static str,
}

impl Fact for ModuleFact {
    const PREDICATE: &'static str = "src.Module";
    fn key(&self) -> aperture_encoding::tuple::Value {
        record([
            ("file", self.file.to_value()),
            ("name", self.name.to_value()),
        ])
    }
}

struct DeclFact {
    module: aperture_schema::id::FactId,
    name: &'static str,
    kind: &'static str,
}

impl Fact for DeclFact {
    const PREDICATE: &'static str = "src.Decl";
    fn key(&self) -> aperture_encoding::tuple::Value {
        record([
            ("module", self.module.to_value()),
            ("name", self.name.to_value()),
        ])
    }
    fn value(&self) -> Option<aperture_encoding::tuple::Value> {
        Some(self.kind.to_value())
    }
}

/// One logical database: files, the modules in them, the declarations in those.
type Content = &'static [(
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
)];

const CONTENT: Content = &[
    (
        "store/keys.py",
        "keys",
        &[("key_of", "def"), ("key_prefix", "def")],
    ),
    ("store/codec.py", "codec", &[("encode_key", "def")]),
];

/// Three further batches of *distinct* facts, for the one test that needs a tree
/// spread across more than one table.
///
/// Distinct because re-offering a fact that is already there is `ops-I5`'s silent
/// dedup — no write, so nothing to flush, so no second table.
const BATCHES: &[Content] = &[
    &[("engine/plan.py", "plan", &[("lower", "def")])],
    &[("engine/iter.py", "iter", &[("enumerate", "def")])],
    &[("engine/ty.py", "ty", &[("unify", "def")])],
];

/// Write `content` into `db`, in the order given.
fn write(db: &FjallDb, schema: &Schema, content: Content) {
    for (path, module, decls) in content {
        let file = db.put(schema, &FileFact(path)).expect("a file");
        let module = db
            .put(schema, &ModuleFact { file, name: module })
            .expect("a module");

        for (decl, kind) in *decls {
            db.put(
                schema,
                &DeclFact {
                    module,
                    name: decl,
                    kind,
                },
            )
            .expect("a declaration");
        }
    }
}

/// Write `content` into a fresh database called `name`, in the order given, and seal
/// it the offline way: nothing holds the store open by the time it is sealed.
fn build(catalog: &Catalog, name: &str, content: Content) -> u64 {
    let schema = schema();
    catalog.create(name, &schema).expect("it creates");
    let (_entry, db) = catalog.open_write(&Selector::of(name)).expect("it opens");

    write(&db, &schema, content);
    drop(db);

    catalog
        .finish(&Selector::of(name), &schema, false)
        .expect("it seals")
        .fingerprint
}

fn catalog() -> (tempfile::TempDir, Catalog) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let catalog = Catalog::open(dir.path().join("store")).expect("a store root");
    (dir, catalog)
}

// ---- ops-I3: the order sealing happens in -----------------------------------

#[test]
fn sealing_records_the_identity_and_flips_the_status() {
    let (_dir, catalog) = catalog();
    let fingerprint = build(&catalog, "code", CONTENT);

    let entry = catalog
        .resolve(&Selector::of("code"), Intent::Read)
        .expect("it is found");

    assert_eq!(entry.status(), Status::Complete);
    assert_eq!(entry.meta.content_fingerprint, Some(fingerprint));
    assert_eq!(entry.meta.facts, Some(7), "2 files + 2 modules + 3 decls");
    assert!(entry.meta.bytes.unwrap_or(0) > 0, "a size was measured");
}

/// **Two front doors, one implementation.** A database sealed through a handle this
/// process already holds is the same artifact as one sealed by opening the directory:
/// same identity, same counts, same status.
///
/// It has to be, and the reason is `ops-I1` rather than tidiness. A server owns every
/// database under its root, so `finish` arriving over the wire is a seal of a store
/// this process is *already* holding — the offline path's first act, opening the
/// directory, is the one thing it cannot do. If the two doors answered differently,
/// `ops-I4` would quietly depend on which one a build came through.
#[test]
fn sealing_through_a_held_handle_is_the_same_artifact() {
    let (_dir, catalog) = catalog();
    let schema = schema();

    let offline = build(&catalog, "offline", CONTENT);

    catalog.create("held", &schema).expect("it creates");
    let (_entry, db) = catalog.open_write(&Selector::of("held")).expect("it opens");
    write(&db, &schema, CONTENT);

    // The handle stays open across the seal, which is the whole difference.
    let sealed = catalog
        .finish_held(&Selector::of("held"), &db, &schema, false)
        .expect("it seals");

    assert_eq!(sealed.fingerprint, offline, "same content, same identity");
    assert_eq!(sealed.facts, 7);
    assert!(!sealed.already_complete);

    let entry = catalog
        .resolve(&Selector::of("held"), Intent::Read)
        .expect("it is found");
    assert_eq!(entry.status(), Status::Complete);
    assert_eq!(entry.meta.content_fingerprint, Some(offline));
    assert!(entry.meta.bytes.unwrap_or(0) > 0, "a size was measured");

    // ...and `ops-I2` holds on this door too, from the moment the sidecar flipped —
    // even though the handle that wrote it is still open. Closing the store is how
    // the offline path says so; the server says it by sealing inside the writer lock.
    assert!(matches!(
        catalog.open_write(&Selector::of("held")).map(|_| ()),
        Err(StoreError::NotWritable {
            status: Status::Complete,
            ..
        })
    ));

    drop(db);
}

/// Sealing twice through a held handle is the same no-op the offline path answers
/// with, rather than a second walk that would recompute an identity nobody asked for.
#[test]
fn finishing_a_held_database_twice_is_a_no_op() {
    let (_dir, catalog) = catalog();
    let schema = schema();

    catalog.create("code", &schema).expect("it creates");
    let (_entry, db) = catalog.open_write(&Selector::of("code")).expect("it opens");
    write(&db, &schema, CONTENT);

    let first = catalog
        .finish_held(&Selector::of("code"), &db, &schema, false)
        .expect("it seals");
    let again = catalog
        .finish_held(&Selector::of("code"), &db, &schema, false)
        .expect("it is a no-op");

    assert!(!first.already_complete);
    assert!(again.already_complete);
    assert_eq!(again.fingerprint, first.fingerprint);
    assert_eq!(again.facts, first.facts);
}

/// **`ops-I2` is downstream of sealing**: after `finish` there is no writable handle
/// to be had, forever.
#[test]
fn a_sealed_database_can_never_be_written_again() {
    let (_dir, catalog) = catalog();
    build(&catalog, "code", CONTENT);

    assert!(matches!(
        catalog.open_write(&Selector::of("code")).map(|_| ()),
        Err(StoreError::NotWritable {
            status: Status::Complete,
            ..
        })
    ));

    catalog
        .open_read(&Selector::of("code"))
        .expect("but it still reads");
}

/// Finishing twice is a no-op with a notice rather than an error: a re-run after a
/// crash cannot tell whether it is the re-run or the original, and both must succeed.
#[test]
fn finishing_twice_is_a_no_op() {
    let (_dir, catalog) = catalog();
    let fingerprint = build(&catalog, "code", CONTENT);

    let again = catalog
        .finish(&Selector::of("code"), &schema(), false)
        .expect("finishing again is allowed");

    assert!(again.already_complete);
    assert_eq!(
        again.fingerprint, fingerprint,
        "and answers the same identity"
    );
}

/// **A silently-empty sealed artifact is the classic CI failure that looks like
/// success**, so making one takes saying so.
#[test]
fn an_empty_database_will_not_seal_without_being_told_to() {
    let (_dir, catalog) = catalog();
    let schema = schema();
    catalog.create("empty", &schema).expect("it creates");

    assert!(matches!(
        catalog.finish(&Selector::of("empty"), &schema, false),
        Err(StoreError::EmptyDatabase(_))
    ));

    // Still Writable: a refused seal changes nothing.
    assert_eq!(
        catalog
            .resolve(&Selector::of("empty"), Intent::Read)
            .expect("it is found")
            .status(),
        Status::Writable
    );

    let sealed = catalog
        .finish(&Selector::of("empty"), &schema, true)
        .expect("with the flag, it seals");
    assert_eq!(sealed.facts, 0);
    assert_eq!(
        catalog
            .resolve(&Selector::of("empty"), Intent::Read)
            .expect("it is found")
            .status(),
        Status::Complete
    );
}

// ---- ops-I4: identity means something ---------------------------------------

/// **The headline.** Two databases built from the same facts in a **different order**
/// have the same identity.
///
/// This is the property physical ids would have destroyed: written in the other
/// order, every fact gets a different `FactId`, and a reference inside a key is one of
/// those ids. Only expanding references to their targets' logical keys makes the two
/// agree.
#[test]
fn the_same_facts_in_a_different_order_have_the_same_identity() {
    let (_dir, catalog) = catalog();

    const REVERSED: Content = &[
        ("store/codec.py", "codec", &[("encode_key", "def")]),
        (
            "store/keys.py",
            "keys",
            &[("key_prefix", "def"), ("key_of", "def")],
        ),
    ];

    let forwards = build(&catalog, "forwards", CONTENT);
    let backwards = build(&catalog, "backwards", REVERSED);

    assert_eq!(
        forwards, backwards,
        "the same content written in another order must have the same identity"
    );

    // **Non-vacuity**, and it has to be measured on a predicate whose key *holds* a
    // reference. A file's key is a bare string, identical in both databases by
    // construction; a module's key holds the file's `FactId`, and those are exactly
    // what the two build orders assign differently. If these agreed byte for byte,
    // the two databases would have been physically identical and the test would have
    // proved nothing about expansion.
    let (_e, a) = catalog
        .open_read(&Selector::of("forwards"))
        .expect("it opens");
    let (_e, b) = catalog
        .open_read(&Selector::of("backwards"))
        .expect("it opens");

    let module_rows = |db: &FjallDb| {
        use aperture_store::fact_store::FactStore;
        let reader = db.reader();
        reader
            .scan(&MODULE.0.to_be_bytes(), None)
            .expect("a scan")
            .map(|row| row.expect("a row").0.to_vec())
            .collect::<Vec<_>>()
    };

    assert_ne!(
        module_rows(&a),
        module_rows(&b),
        "the two databases were supposed to assign file ids differently; if their \
         module rows agree byte for byte the expansion is untested"
    );
}

/// Two databases with **different** content have different identities. The
/// counterpart to the test above, and the one that says the hash is not a constant.
#[test]
fn different_facts_have_different_identities() {
    let (_dir, catalog) = catalog();

    const EXTRA: Content = &[
        (
            "store/keys.py",
            "keys",
            &[("key_of", "def"), ("key_prefix", "def")],
        ),
        ("store/codec.py", "codec", &[("encode_key", "def")]),
        ("query/plan.py", "plan", &[("Plan", "class")]),
    ];

    // One declaration's *kind* differs, which lives on the value side — so this also
    // says the value side reaches the hash.
    const OTHER_KIND: Content = &[
        (
            "store/keys.py",
            "keys",
            &[("key_of", "class"), ("key_prefix", "def")],
        ),
        ("store/codec.py", "codec", &[("encode_key", "def")]),
    ];

    let base = build(&catalog, "base", CONTENT);
    let extra = build(&catalog, "extra", EXTRA);
    let kind = build(&catalog, "kind", OTHER_KIND);

    assert_ne!(base, extra, "an added fact must change the identity");
    assert_ne!(base, kind, "a changed value side must change the identity");
}

/// A **renamed target** changes the identity of everything that names it, which is
/// what "expand references" buys: the referring fact's own key bytes are unchanged.
#[test]
fn renaming_a_referenced_fact_changes_what_names_it() {
    let (_dir, catalog) = catalog();

    const RENAMED: Content = &[
        (
            "store/KEYS.py",
            "keys",
            &[("key_of", "def"), ("key_prefix", "def")],
        ),
        ("store/codec.py", "codec", &[("encode_key", "def")]),
    ];

    assert_ne!(
        build(&catalog, "before", CONTENT),
        build(&catalog, "after", RENAMED),
        "a module's identity includes the path of the file it names"
    );
}

/// The identity is a **function of the content**, so computing it twice over one
/// database agrees. Guards against anything in the walk depending on iteration state.
#[test]
fn the_identity_is_a_function_of_the_database() {
    let (_dir, catalog) = catalog();
    let sealed = build(&catalog, "code", CONTENT);

    let (entry, db) = catalog.open_read(&Selector::of("code")).expect("it opens");
    let again =
        identity::compute(&db, &schema(), entry.meta.schema_fingerprint).expect("it recomputes");

    assert_eq!(again.fingerprint, sealed);
    assert_eq!(again.facts, 7);
}

/// The schema is part of identity, not just the facts — `hash(canonical schema, base
/// facts)`. Two databases with identical facts and different schema fingerprints are
/// different artifacts.
#[test]
fn the_schema_fingerprint_reaches_the_identity() {
    let (_dir, catalog) = catalog();
    build(&catalog, "code", CONTENT);

    let (entry, db) = catalog.open_read(&Selector::of("code")).expect("it opens");

    let as_recorded =
        identity::compute(&db, &schema(), entry.meta.schema_fingerprint).expect("it computes");
    let as_if_other =
        identity::compute(&db, &schema(), entry.meta.schema_fingerprint ^ 1).expect("it computes");

    assert_ne!(as_recorded.fingerprint, as_if_other.fingerprint);
}

// ---- what sealing leaves on the disk ----------------------------------------

/// **Sealing merges every tree.** A `Complete` database is immutable forever and is
/// the thing copied per reader process ([operations §5]), so the shape ingestion
/// happened to leave the LSM in is the shape every future reader pays for — and a
/// re-seek into an unmerged tree was measured at up to 180× one into a merged tree
/// (`bench/FINDINGS.md`). The server re-seeks once per level per 256-row page, so this
/// is on the path of every paged query, forever.
///
/// The three flushed batches are the point of the test, not setup: at this size every
/// fact would otherwise live in one memtable, and a compaction guard with nothing to
/// merge passes without measuring anything. The precondition is asserted, so it cannot
/// quietly stop being true.
///
/// [operations §5]: ../../../docs/aperture-cli-design.md
#[test]
fn sealing_merges_every_tree_into_one_table() {
    let (_dir, catalog) = catalog();
    let schema = schema();

    catalog.create("code", &schema).expect("it creates");
    let (_entry, db) = catalog.open_write(&Selector::of("code")).expect("it opens");

    write(&db, &schema, CONTENT);
    db.flush_to_tables().expect("it flushes");
    for batch in BATCHES {
        write(&db, &schema, batch);
        db.flush_to_tables().expect("it flushes");
    }

    let before = db.table_counts();
    assert!(
        before.iter().any(|count| *count > 1),
        "nothing to merge: every tree is already one table ({before:?}), \
         so this test would pass without compacting anything"
    );
    drop(db);

    catalog
        .finish(&Selector::of("code"), &schema, false)
        .expect("it seals");

    let (_entry, db) = catalog
        .open_read(&Selector::of("code"))
        .expect("it reopens");
    let after = db.table_counts();

    assert!(
        after.iter().all(|count| *count <= 1),
        "a sealed database still has a tree spread across tables: {after:?}"
    );
}

/// **Merging changes what the bytes are, never what the database says.**
///
/// `ops-I4` is a promise about content, and compaction rewrites the files underneath
/// it — dropping superseded versions, repacking blocks. The identity computed *before*
/// any of that must be the identity sealing records, or the fingerprint is a property
/// of a storage layout rather than of the facts.
#[test]
fn merging_does_not_change_the_identity() {
    let (_dir, catalog) = catalog();
    let schema = schema();

    catalog.create("code", &schema).expect("it creates");
    let (entry, db) = catalog.open_write(&Selector::of("code")).expect("it opens");

    write(&db, &schema, CONTENT);
    db.flush_to_tables().expect("it flushes");

    let unmerged =
        identity::compute(&db, &schema, entry.meta.schema_fingerprint).expect("it computes");
    drop(db);

    let sealed = catalog
        .finish(&Selector::of("code"), &schema, false)
        .expect("it seals");

    assert_eq!(sealed.fingerprint, unmerged.fingerprint);
    assert_eq!(sealed.facts, unmerged.facts);

    // And the facts are still all there to be read, which a fingerprint over a walk
    // that found nothing would also satisfy.
    let (entry, db) = catalog
        .open_read(&Selector::of("code"))
        .expect("it reopens");
    let after =
        identity::compute(&db, &schema, entry.meta.schema_fingerprint).expect("it recomputes");

    assert_eq!(after.fingerprint, unmerged.fingerprint);
    assert_eq!(after.facts, 7);
}

// ---- ops-I3 across a real crash ---------------------------------------------

/// The child's own test path, which `--exact` matches on. A stale path produces a
/// *passing* child, which the parent would read as "the crash never happened" — hence
/// the assertion that the child failed.
const CRASH_CHILD: &str = "crashing_finisher_child_process";
const CRASH_ROOT_VAR: &str = "APERTURE_FINISH_CRASH_ROOT";
const CRASH_DELAY_VAR: &str = "APERTURE_FINISH_CRASH_DELAY_MS";

/// **`ops-I3` across a crash**: it must never be observable that metadata says
/// Complete while the data is not durable, and a crash mid-`finish` must leave a
/// Writable database the command can be re-run on.
///
/// The failure is injected by killing the process rather than by a hook in the code,
/// which is the only way to test the ordering itself: a hook would test the hook.
#[test]
fn a_killed_finish_leaves_a_re_runnable_database() {
    let mut killed_mid_finish = 0;

    for delay_ms in [1u64, 3, 8, 20, 50, 150] {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let root = dir.path().join("store");

        let status =
            std::process::Command::new(std::env::current_exe().expect("path to this test binary"))
                .args(["--exact", CRASH_CHILD, "--ignored", "--nocapture"])
                .env(CRASH_ROOT_VAR, &root)
                .env(CRASH_DELAY_VAR, delay_ms.to_string())
                .status()
                .expect("spawn the crashing finisher");

        assert!(
            !status.success(),
            "the child was supposed to abort mid-finish, not exit cleanly"
        );

        let catalog = Catalog::open(&root).expect("the store root survives");
        let entry = catalog
            .resolve(&Selector::of("code"), Intent::Read)
            .expect("the database is there either way");

        match entry.status() {
            // Killed before the sidecar landed. The database is exactly as it was:
            // no identity recorded, and re-runnable — which is the whole claim.
            Status::Writable => {
                killed_mid_finish += 1;

                assert_eq!(
                    entry.meta.content_fingerprint, None,
                    "delay {delay_ms}ms: a Writable database must carry no identity — \
                     one would be a fingerprint another write could invalidate"
                );

                let sealed = catalog
                    .finish(&Selector::of("code"), &schema(), false)
                    .unwrap_or_else(|e| panic!("delay {delay_ms}ms: it must re-run: {e}"));
                assert!(!sealed.already_complete);
                assert_eq!(sealed.facts, 7);
            }

            // The flip landed, so everything before it did too — that is what makes
            // it the last durable act.
            Status::Complete => {
                assert!(
                    entry.meta.content_fingerprint.is_some(),
                    "delay {delay_ms}ms: Complete without an identity would mean the \
                     flip was not last"
                );
                assert_eq!(entry.meta.facts, Some(7));
            }

            Status::Broken => panic!("delay {delay_ms}ms: nothing should produce Broken"),
        }
    }

    assert!(
        killed_mid_finish > 0,
        "no run was killed before its seal landed, so nothing here tested the ordering"
    );
}

/// Not a guard: the crashing half of the test above.
#[test]
#[ignore = "spawned as a child process by a_killed_finish_leaves_a_re_runnable_database"]
fn crashing_finisher_child_process() {
    let root = std::env::var(CRASH_ROOT_VAR).expect("the parent sets the store root");
    let delay_ms: u64 = std::env::var(CRASH_DELAY_VAR)
        .expect("the parent sets the delay")
        .parse()
        .expect("a number");

    let catalog = Catalog::open(&root).expect("a store root");
    let schema = schema();

    // Built and populated before the watchdog is armed, so the kill lands inside
    // `finish` rather than inside `create`.
    catalog.create("code", &schema).expect("it creates");
    {
        let (_entry, db) = catalog.open_write(&Selector::of("code")).expect("it opens");

        for (path, module, decls) in CONTENT {
            let file = db.put(&schema, &FileFact(path)).expect("a file");
            let module = db
                .put(&schema, &ModuleFact { file, name: module })
                .expect("a module");
            for (decl, kind) in *decls {
                db.put(
                    &schema,
                    &DeclFact {
                        module,
                        name: decl,
                        kind,
                    },
                )
                .expect("a declaration");
            }
        }
    }

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        // `abort`, not `exit`: no destructors, no flushes, nothing tidied.
        std::process::abort();
    });

    let _ = catalog.finish(&Selector::of("code"), &schema, false);

    std::thread::sleep(std::time::Duration::from_millis(delay_ms + 500));
}
