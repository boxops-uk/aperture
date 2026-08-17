//! The command tree, driven as a person drives it.
//!
//! Runs the real binary — `CARGO_BIN_EXE_aperture` is set for integration tests, so
//! this needs no `assert_cmd` and no dependency. What it checks is the *sequence*: a
//! database's life is create → write → seal → remove, and each step has to see what
//! the last one did.

use std::{path::Path, process::Command};

/// Run `aperture` against a scratch store root.
fn aperture(root: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_aperture"))
        .arg("--data-dir")
        .arg(root)
        .args(args)
        .output()
        .expect("the binary runs");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn ok(root: &Path, args: &[&str]) -> String {
    let (success, stdout, stderr) = aperture(root, args);
    assert!(success, "`aperture {args:?}` failed:\n{stderr}");
    stdout
}

fn fails(root: &Path, args: &[&str]) -> String {
    let (success, stdout, stderr) = aperture(root, args);
    assert!(
        !success,
        "`aperture {args:?}` was supposed to fail:\n{stdout}"
    );
    stderr
}

/// **The acceptance criterion**: create → list → describe → finish → list → rm, with
/// `list` showing the status change.
#[test]
fn a_database_lives_and_dies_through_the_command_tree() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();

    // Nothing yet, and saying so rather than printing an empty table.
    assert!(ok(root, &["list"]).contains("no databases"));

    let created = ok(root, &["create", "code"]);
    assert!(created.starts_with("created code ("), "{created}");

    let listed = ok(root, &["list"]);
    assert!(listed.contains("code"), "{listed}");
    assert!(listed.contains("writable"), "{listed}");

    // A Writable database's counts are genuinely unknown until `finish` walks it, so
    // they print as a dash. A zero would be a claim.
    let described = ok(root, &["describe", "code"]);
    assert!(described.contains("status    writable"), "{described}");
    assert!(described.contains("recorded at finish"), "{described}");

    // ...and `describe` shows the schema, read from the copy inside the database
    // rather than from anything compiled in.
    assert!(described.contains("src.Decl"), "{described}");
    assert!(
        described.contains("{ file : src.File, name : string }"),
        "{described}"
    );

    // Empty, so sealing takes saying so.
    let refused = fails(root, &["finish", "code"]);
    assert!(refused.contains("--allow-zero-facts"), "{refused}");

    let sealed = ok(root, &["finish", "code", "--allow-zero-facts"]);
    assert!(sealed.contains("sealed code"), "{sealed}");
    assert!(sealed.contains("identity"), "{sealed}");

    // The status change is visible where someone would look for it.
    let after = ok(root, &["list"]);
    assert!(after.contains("complete"), "{after}");
    assert!(!after.contains("writable"), "{after}");

    // Sealing again is a no-op with a notice, not an error.
    assert!(
        ok(root, &["finish", "code"]).contains("already complete"),
        "finishing twice should be allowed"
    );

    // Deleting is not undoable and there is no trash, so the default is to ask.
    let asked = ok(root, &["db", "rm", "code"]);
    let _ = asked;
    assert!(
        ok(root, &["list"]).contains("code"),
        "a refused delete must not have deleted anything"
    );

    ok(root, &["db", "rm", "code", "--yes"]);
    assert!(ok(root, &["list"]).contains("no databases"));
}

/// `--format json` is a different rendering of the same thing, made client-side. The
/// server never produces JSON.
#[test]
fn json_output_is_a_rendering_not_a_different_query() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();

    ok(root, &["create", "code"]);

    let listed: serde_json::Value =
        serde_json::from_str(&ok(root, &["list", "--format", "json"])).expect("valid JSON");

    let databases = listed["databases"].as_array().expect("an array");
    assert_eq!(databases.len(), 1);
    assert_eq!(databases[0]["name"], "code");
    assert_eq!(databases[0]["status"], "writable");

    // The absences are absences in JSON too, rather than zeros or nulls.
    assert!(databases[0].get("facts").is_none(), "{:?}", databases[0]);
    assert!(databases[0].get("content_fingerprint").is_none());
    assert!(databases[0].get("externally_modified").is_none(), "ops-I6");

    let described: serde_json::Value =
        serde_json::from_str(&ok(root, &["describe", "code", "--format", "json"]))
            .expect("valid JSON");

    assert_eq!(described["name"], "code");

    // The built-in schema's own count, not a number written down twice: what this is
    // checking is that `describe` reports the schema the database was created with, and
    // a literal here fails whenever a predicate is appended — which says nothing about
    // the rendering.
    assert_eq!(
        described["schema"]["predicates"].as_array().unwrap().len(),
        aperture_cli::code_index::schema().len()
    );
}

/// A name that is not a database says so, rather than reporting an empty result.
#[test]
fn an_unknown_database_is_named() {
    let dir = tempfile::tempdir().expect("a scratch directory");

    for args in [
        vec!["describe", "nope"],
        vec!["finish", "nope"],
        vec!["db", "rm", "nope", "--yes"],
    ] {
        let stderr = fails(dir.path(), &args);
        assert!(stderr.contains("nope"), "{args:?}: {stderr}");
    }
}

/// Two databases coexist, and `list` shows both in a stable order.
#[test]
fn several_databases_coexist() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();

    ok(root, &["create", "beta"]);
    ok(root, &["create", "alpha"]);

    let listed = ok(root, &["list"]);
    let alpha = listed.find("alpha").expect("alpha is listed");
    let beta = listed.find("beta").expect("beta is listed");

    assert!(
        alpha < beta,
        "listed by name, not by creation order:\n{listed}"
    );

    // A name is one database: creating it twice is refused rather than making a
    // second instance nobody asked for.
    assert!(fails(root, &["create", "alpha"]).contains("already exists"));
}

/// **`ops-I1` has no silent fallback.** A root held by something that is *not*
/// listening refuses the command and names both halves; it never opens the directory
/// anyway.
///
/// The lock is taken directly rather than by starting a server, and since 9d that is
/// no longer merely the cheaper way to hold it — it is the case being tested. A
/// running server is found on its socket and the command routes through it
/// (`over_a_server.rs`); what is left here is the genuinely confusing situation of a
/// root that is owned and unreachable, which is the one that needs an actionable
/// message.
#[test]
fn a_held_store_root_refuses_lifecycle_commands() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();

    ok(root, &["create", "code"]);

    let catalog = aperture_store::catalog::Catalog::open(root).expect("a store root");
    let held = catalog.lock().expect("this process holds it");

    for args in [
        vec!["create", "other"],
        vec!["finish", "code"],
        vec!["db", "rm", "code", "--yes"],
    ] {
        let stderr = fails(root, &args);
        assert!(
            stderr.contains("held by another process"),
            "{args:?} should have been refused: {stderr}"
        );
        assert!(stderr.contains(&root.display().to_string()), "{stderr}");

        // Both halves, because "held" alone leaves someone with nowhere to look.
        assert!(stderr.contains("aperture.sock"), "{stderr}");
    }

    // ...but reading works throughout, because enumeration never opens fjall
    // (`ops-I7`). This is the one thing that must not fail while a server is up.
    assert!(ok(root, &["list"]).contains("code"));
    assert!(ok(root, &["describe", "code"]).contains("writable"));

    drop(held);

    // Released, and the same command now works — the refusal was contention, not a
    // permanent state.
    ok(root, &["create", "other"]);
}

/// **A schema is a file the tool reads**, and the three questions it can be asked
/// before any database holds one — [operations §5](../docs/aperture-cli-design.md)'s
/// `check`, `fingerprint` and `diff`.
#[test]
fn a_schema_is_checked_fingerprinted_and_diffed_as_a_file() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();

    let schema = root.join("tiny.aps");
    std::fs::write(
        &schema,
        "schema log {\n  predicate Line : string\n  predicate Entry : { line : Line, level : int } }\n",
    )
    .expect("it writes");

    let checked = ok(root, &["schema", "check", schema.to_str().expect("utf-8")]);
    assert!(checked.contains("2 predicate(s) in 1 file(s)"), "{checked}");
    assert!(checked.contains("fingerprint 0x"), "{checked}");

    // The number a client carries, and the per-predicate map beside it.
    let printed = ok(
        root,
        &["schema", "fingerprint", schema.to_str().expect("utf-8")],
    );
    assert!(printed.contains("log.Line"), "{printed}");
    assert!(printed.contains("log.Entry"), "{printed}");

    // The canonical form is what the number is *of* — the thing a second
    // implementation is written against, so it prints on demand rather than never.
    let canonical = ok(
        root,
        &[
            "schema",
            "fingerprint",
            schema.to_str().expect("utf-8"),
            "--canonical",
        ],
    );
    assert!(canonical.starts_with("aperture-schema-v1\n"), "{canonical}");
    assert!(canonical.contains("log.Line:string"), "{canonical}");

    // A schema that is wrong is refused with the reason, against the file it is in.
    let broken = root.join("broken.aps");
    std::fs::write(&broken, "schema log { predicate Line : bananas }\n").expect("it writes");

    let refused = fails(root, &["schema", "check", broken.to_str().expect("utf-8")]);
    assert!(refused.contains("bananas"), "{refused}");
    assert!(refused.contains("broken.aps"), "{refused}");

    // An import nothing answers says what it looked for and where.
    let importing = root.join("importing.aps");
    std::fs::write(&importing, "schema app { import lang.rust }\n").expect("it writes");

    let unresolved = fails(
        root,
        &["schema", "check", importing.to_str().expect("utf-8")],
    );
    assert!(unresolved.contains("lang.rust"), "{unresolved}");
    assert!(unresolved.contains("lang/rust.aps"), "{unresolved}");
}

/// **`create --schema` is the one moment a database's schema can be chosen** (I13), and
/// what it chose is visible afterwards: in the copy the database embeds, in the
/// fingerprint the sidecar records, and in a `diff` against the file it was built from.
#[test]
fn a_database_is_created_against_a_schema_file_and_carries_it() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();

    let schema = root.join("tiny.aps");
    std::fs::write(
        &schema,
        "# a schema of one's own\nschema log { predicate Line : string }\n",
    )
    .expect("it writes");

    let path = schema.to_str().expect("utf-8");

    let created = ok(root, &["create", "tiny", "--schema", path]);
    assert!(
        created.contains(path),
        "it should say what it built against: {created}"
    );

    // The default is still the built-in one, so the two databases under one root hold
    // different schemas — which is the whole point of the copy being per database.
    ok(root, &["create", "builtin"]);

    let described = ok(root, &["describe", "tiny"]);
    assert!(described.contains("log.Line"), "{described}");
    assert!(!described.contains("src.File"), "{described}");

    // `--schema` dumps the copy itself, which is text `create --schema` would take back.
    let dumped = ok(root, &["describe", "tiny", "--schema"]);
    assert!(dumped.contains("predicate Line : string"), "{dumped}");

    // And the copy agrees with the file it came from, which is what `diff` is for.
    let same = ok(root, &["schema", "diff", path, "tiny"]);
    assert!(same.contains("Identical"), "{same}");

    // Against the other database it is Breaking, with per-predicate reasons — the two
    // fingerprints in `list` say the same thing more briefly.
    let differs = ok(root, &["schema", "diff", "tiny", "builtin"]);
    assert!(differs.contains("Breaking"), "{differs}");
    assert!(differs.contains("- log.Line  (removed)"), "{differs}");

    let listed = ok(root, &["list"]);
    let fingerprints: Vec<&str> = listed
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(3))
        .collect();
    assert_eq!(fingerprints.len(), 2);
    assert_ne!(
        fingerprints[0], fingerprints[1],
        "two schemas, two identities: {listed}"
    );

    // A schema that does not resolve creates nothing at all.
    let broken = root.join("broken.aps");
    std::fs::write(&broken, "schema log { predicate Line : bananas }\n").expect("it writes");

    let refused = fails(
        root,
        &[
            "create",
            "never",
            "--schema",
            broken.to_str().expect("utf-8"),
        ],
    );
    assert!(refused.contains("bananas"), "{refused}");
    assert!(
        !ok(root, &["list"]).contains("never"),
        "nothing was created"
    );
}

/// **A compatible change is one that only adds**, which is chapter 6's subset
/// containment seen from the command line — and the reason `diff` exists rather than a
/// string comparison of two files.
#[test]
fn adding_a_predicate_is_the_one_compatible_change() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path();

    let before = root.join("before.aps");
    let after = root.join("after.aps");
    let changed = root.join("changed.aps");

    std::fs::write(&before, "schema log { predicate Line : string }\n").expect("it writes");
    std::fs::write(
        &after,
        "schema log { predicate Line : string\n predicate Level : int }\n",
    )
    .expect("it writes");
    std::fs::write(&changed, "schema log { predicate Line : int }\n").expect("it writes");

    let (before, after, changed) = (
        before.to_str().expect("utf-8"),
        after.to_str().expect("utf-8"),
        changed.to_str().expect("utf-8"),
    );

    assert!(
        ok(root, &["schema", "diff", before, before]).contains("Identical"),
        "a schema is identical to itself"
    );

    let added = ok(root, &["schema", "diff", before, after]);
    assert!(added.contains("Compatible (1 added)"), "{added}");
    assert!(added.contains("+ log.Level"), "{added}");

    // Removing is breaking in the other direction — the relation is not symmetric, and
    // a diff that answered the same both ways would be answering a different question.
    let removed = ok(root, &["schema", "diff", after, before]);
    assert!(removed.contains("Breaking"), "{removed}");
    assert!(removed.contains("- log.Level  (removed)"), "{removed}");

    // A key that changed type is breaking even though nothing was removed: a key's
    // fields are positional, so every fact already written decodes to something else.
    let modified = ok(root, &["schema", "diff", before, changed]);
    assert!(modified.contains("Breaking"), "{modified}");
    assert!(modified.contains("~ log.Line  (modified"), "{modified}");
}
