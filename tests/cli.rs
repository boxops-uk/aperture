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
    assert_eq!(
        described["schema"]["predicates"].as_array().unwrap().len(),
        6
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

/// **`ops-I1` has no silent fallback.** A held root refuses the command and names the
/// root; it never opens the directory anyway.
///
/// The lock is taken directly rather than by starting a server, which is the same
/// thing to `flock` and does not need a socket, a port or a wait.
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
            stderr.contains("held by a running server"),
            "{args:?} should have been refused: {stderr}"
        );
        assert!(stderr.contains(&root.display().to_string()), "{stderr}");
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
