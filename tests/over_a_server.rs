//! **The phase's command sequence, against a running server.**
//!
//! `tests/cli.rs` drives the same commands with nothing listening, and the two files
//! together are the claim: a lifecycle command works the same either way, and which
//! way it went is a property of the *address* rather than of the command.
//!
//! What makes this test prove routing rather than assume it is the counterfactual next
//! door. A running server holds the root lock (`ops-I1`), so a command that opened the
//! directory in this process **could not succeed** — `a_held_store_root_refuses_lifecycle_commands`
//! is that exact situation, and it fails by name. So every success below is a frame
//! that crossed the socket.

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Run `aperture` against a store root.
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

/// A server in its own process, killed when the test ends.
///
/// It does **not** own the scratch directory, and that is deliberate: one test outlives
/// its server on purpose, and a handle that took the store root with it would make that
/// test pass or fail for the wrong reason.
struct Serving {
    child: Child,
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scratch store root, and the directory keeping it alive.
fn scratch() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path().join("store");
    std::fs::create_dir_all(&root).expect("a store root");
    (dir, root)
}

/// Start `aperture serve` over `root` and wait until it is **accepting**.
///
/// The readiness file, not a sleep: `Listener::announce` writes it after the bind, so
/// a client that sees it can connect. A sleep here would be a race dressed as a wait,
/// and it is the exact race that flag exists to remove.
fn serve(root: &Path) -> Serving {
    let ready = root.join("ready");

    let child = Command::new(env!("CARGO_BIN_EXE_aperture"))
        .arg("--data-dir")
        .arg(root)
        .arg("serve")
        .arg("--ready-file")
        .arg(&ready)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the server starts");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the server never became ready");
        thread::sleep(Duration::from_millis(20));
    }

    Serving { child }
}

/// **The acceptance criterion**: create → list → describe → finish → list → rm, over
/// the wire, against a server that is running throughout.
#[test]
fn the_lifecycle_works_against_a_running_server() {
    let (_dir, root) = scratch();
    let _serving = serve(&root);

    // Nothing yet — and `list` answering at all is `ops-I7`: it reads sidecars and
    // never opens fjall, so a server owning every database under the root is no
    // obstacle. It never needed a control message, which is why it never got one.
    assert!(ok(&root, &["list"]).contains("no databases"));

    // This is the line that was impossible before 9d. The server holds the root lock,
    // so opening the directory here would be refused by name; it succeeds because it
    // went over the socket.
    let created = ok(&root, &["create", "code"]);
    assert!(created.starts_with("created code ("), "{created}");

    let listed = ok(&root, &["list"]);
    assert!(listed.contains("code"), "{listed}");
    assert!(listed.contains("writable"), "{listed}");

    let described = ok(&root, &["describe", "code"]);
    assert!(described.contains("status    writable"), "{described}");
    assert!(described.contains("src.Decl"), "{described}");

    // Empty, so sealing takes saying so — the same refusal, in the same words, as the
    // offline path gives, because it is the same code behind both doors.
    let refused = fails(&root, &["finish", "code"]);
    assert!(refused.contains("--allow-zero-facts"), "{refused}");

    let sealed = ok(&root, &["finish", "code", "--allow-zero-facts"]);
    assert!(sealed.contains("sealed code"), "{sealed}");
    assert!(sealed.contains("identity"), "{sealed}");

    let after = ok(&root, &["list"]);
    assert!(after.contains("complete"), "{after}");
    assert!(!after.contains("writable"), "{after}");

    // Sealing again is a no-op with a notice, over the wire as offline.
    assert!(
        ok(&root, &["finish", "code"]).contains("already complete"),
        "finishing twice should be allowed"
    );

    ok(&root, &["db", "rm", "code", "--yes"]);
    assert!(ok(&root, &["list"]).contains("no databases"));
}

/// A refusal from the server arrives as the server's own words, and the tool exits
/// non-zero — not as a stack trace and not as a success with a warning.
#[test]
fn a_server_refusal_reaches_the_person_who_typed_it() {
    let (_dir, root) = scratch();
    let _serving = serve(&root);

    // Two instances, so a bare-name delete is ambiguous — and the refusal is decided
    // by the *server*, which is what makes this a test of the server's own words
    // reaching the person who typed the command rather than of the offline path's.
    ok(&root, &["create", "code"]);
    ok(&root, &["create", "code"]);

    let stderr = fails(&root, &["db", "rm", "code", "--yes"]);
    assert!(stderr.contains("2 instances"), "{stderr}");
    assert!(stderr.contains("code@"), "{stderr}");

    for args in [
        vec!["finish", "nope", "--allow-zero-facts"],
        vec!["db", "rm", "nope", "--yes"],
    ] {
        let stderr = fails(&root, &args);
        assert!(stderr.contains("nope"), "{args:?}: {stderr}");
    }
}

/// **A database made over the wire is an ordinary database.** The server's `create` is
/// the catalog's `create`, so what lands on the disk is what the offline path would
/// have written — sidecar, embedded schema and all — and the offline tool can pick it
/// up once the server has gone.
#[test]
fn what_the_server_made_outlives_it() {
    let (_dir, root) = scratch();
    let serving = serve(&root);

    ok(&root, &["create", "code"]);

    // The server goes; the scratch directory stays, which is the point.
    drop(serving);

    // The lock went with the process. The same command tree now takes the root itself,
    // and finds a database it did not make.
    let described = ok(&root, &["describe", "code"]);
    assert!(described.contains("status    writable"), "{described}");
    assert!(described.contains("src.Decl"), "{described}");

    let sealed = ok(&root, &["finish", "code", "--allow-zero-facts"]);
    assert!(sealed.contains("sealed code"), "{sealed}");

    ok(&root, &["db", "rm", "code", "--yes"]);
    assert!(ok(&root, &["list"]).contains("no databases"));
}

/// **`aperture query` is always over the wire** (§2's rule 1), streams its rows, and
/// renders them client-side in whichever shape was asked for.
///
/// It writes nothing itself — the CLI has no `write` command until 7b — so what it
/// queries is an empty database. That is enough to check the whole path: connect,
/// compile on the server, descriptor, zero rows, complete. The *rows* path is checked
/// where rows exist, in `aperture-client`'s tests and the loadgen.
#[test]
fn query_speaks_to_the_server_and_renders_client_side() {
    let (_dir, root) = scratch();
    let _serving = serve(&root);

    ok(&root, &["create", "code"]);

    // A scalar head: one unnamed column.
    let table = ok(&root, &["query", "code", "F where src.File F"]);
    assert!(table.contains("VALUE"), "{table}");
    assert!(table.contains("0 row(s)"), "{table}");

    // `count` is the shape a measurement wants: the tally and nothing else.
    assert_eq!(
        ok(
            &root,
            &["query", "code", "F where src.File F", "--format", "count"]
        ),
        "0\n"
    );

    // `json` is a document even when it is empty, so a script can parse it rather
    // than special-casing nothing.
    let json = ok(
        &root,
        &["query", "code", "F where src.File F", "--format", "json"],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).expect("valid JSON"),
        serde_json::json!([])
    );

    // A query that does not compile fails with the compiler's own diagnostics, and
    // the exit code says so.
    let stderr = fails(&root, &["query", "code", "this is not focus"]);
    assert!(stderr.contains("invalid syntax"), "{stderr}");

    // An unknown database is named rather than reported as an empty result.
    let stderr = fails(&root, &["query", "nope", "F where src.File F"]);
    assert!(stderr.contains("nope"), "{stderr}");
}

/// **§2 rule 1 has no fallback.** With nothing listening, a query says what to do
/// about it — it never opens the directory, because a server might be holding it.
#[test]
fn a_query_with_no_server_says_so() {
    let (_dir, root) = scratch();

    // Created offline, so the database exists and only the server is missing.
    ok(&root, &["create", "code"]);

    let stderr = fails(&root, &["query", "code", "F where src.File F"]);
    assert!(stderr.contains("could not connect"), "{stderr}");
    assert!(stderr.contains("aperture serve"), "{stderr}");
    assert!(stderr.contains("aperture.sock"), "{stderr}");
}
