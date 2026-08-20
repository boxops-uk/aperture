//! **The seam names no implementation** — read off the dependency graph, which
//! is the only place the claim is true or false.
//!
//! Not a numbered invariant, because nothing checks it at runtime: it is a
//! property of what Cargo links. `fjord-engine` reaching `fjall` costs nothing
//! on a host and makes the engine impossible to compile for
//! `wasm32-unknown-unknown` — `getrandom` refuses that target outright — so the
//! failure this guards against is one `cargo add` away and invisible until
//! somebody tries the browser build.
//!
//! **What this reads is the workspace graph**, member by member, from the
//! manifests: which crate names which. It deliberately does not walk
//! `Cargo.lock`, because that file unions every kind of dependency and lists
//! target-gated ones unconditionally — `libc` appears under crates that do not
//! link it off Unix — so a walk of it fails for crates that are in fact clean.
//! The other half of the claim is a **compile**: CI runs
//! `cargo check -p fjord-engine --target wasm32-unknown-unknown`, which is the
//! only thing that can prove a dependency nobody has thought to name here.
//!
//! Dev-dependencies are skipped, and that is the point rather than a loophole:
//! the engine's batteries hold the two stores against each other and so *must*
//! see fjall. Optional dependencies are skipped for the same reason — the
//! `proptest` strategies pull `getrandom` through `rand`, and no build that
//! targets a browser turns them on.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// The workspace root, from this crate's manifest directory.
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> sits two levels under the workspace root")
        .to_path_buf()
}

/// Every workspace member, by package name, with its manifest path.
fn members() -> BTreeMap<String, PathBuf> {
    let crates = workspace().join("crates");
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(&crates).expect("crates/ is readable") {
        let manifest = entry.expect("a directory entry").path().join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Some(name) = package_name(&text)
        {
            found.insert(name, manifest);
        }
    }
    assert!(
        found.len() >= 10,
        "only {} workspace members found — the walk is not reading crates/",
        found.len()
    );
    found
}

fn package_name(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .skip_while(|line| line.trim() != "[package]")
        .find_map(|line| line.strip_prefix("name = "))
        .map(|name| name.trim().trim_matches('"').to_owned())
}

/// A member's non-optional `[dependencies]` and `[build-dependencies]`, by name.
fn declared(manifest: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(manifest).expect("a member manifest is readable");
    let mut deps = BTreeSet::new();
    let mut inside = false;

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = matches!(line, "[dependencies]" | "[build-dependencies]");
            continue;
        }
        if !inside || line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        if rest.contains("optional = true") {
            continue;
        }
        deps.insert(name.trim().to_owned());
    }
    deps
}

/// Every workspace member `root` links in a default build, transitively, plus
/// the direct third-party names each of them declares.
fn closure(root: &str) -> BTreeSet<String> {
    let members = members();
    let mut seen = BTreeSet::new();
    let mut queue = vec![root.to_owned()];

    while let Some(package) = queue.pop() {
        if !seen.insert(package.clone()) {
            continue;
        }
        // A third-party name is recorded and not walked: its own graph is
        // Cargo's business, and the compile in CI is what checks it.
        if let Some(manifest) = members.get(&package) {
            queue.extend(declared(manifest));
        }
    }
    seen.remove(root);
    seen
}

/// What a browser cannot run, whatever a host makes of it.
const FORBIDDEN: &[&str] = &[
    "fjall",
    "fjord-store-fjall",
    "lsm-tree",
    "getrandom",
    "libc",
];

#[test]
fn the_seam_crate_does_not_link_fjall() {
    for crate_name in ["fjord-store", "fjord-store-mem"] {
        let closure = closure(crate_name);
        assert!(
            closure.contains("fjord-schema"),
            "{crate_name}'s closure came out as {closure:?} — the walk is broken, \
             and a broken walk finds nothing forbidden"
        );
        for forbidden in FORBIDDEN {
            assert!(
                !closure.contains(*forbidden),
                "{crate_name} links `{forbidden}`: the seam has grown the shape of \
                 one implementation, and the engine can no longer reach a browser"
            );
        }
    }
}

#[test]
fn the_engine_links_nothing_a_browser_cannot_run() {
    let closure = closure("fjord-engine");
    assert!(
        closure.contains("fjord-store") && closure.contains("logos"),
        "the engine's closure ({closure:?}) is not what a walk of it should find"
    );
    for forbidden in FORBIDDEN {
        assert!(
            !closure.contains(*forbidden),
            "fjord-engine links `{forbidden}`, so `cargo check -p fjord-engine \
             --target wasm32-unknown-unknown` no longer compiles — the browser is \
             downstream of this line"
        );
    }
}

/// **The positive control.** Every assertion above is a `!contains`, and a walk
/// that found nothing at all would satisfy every one of them.
#[test]
fn the_walk_finds_the_backend_where_the_backend_is() {
    let backend = closure("fjord-store-fjall");
    for expected in ["fjall", "getrandom", "libc", "fjord-store"] {
        assert!(
            backend.contains(expected),
            "the fjall backend's closure does not contain `{expected}` — the walk \
             is not reading the graph it claims to"
        );
    }
    assert!(
        closure("fjord-cli").contains("fjord-store-fjall"),
        "a crate that drives the lifecycle does not reach the backend that holds it"
    );
}
