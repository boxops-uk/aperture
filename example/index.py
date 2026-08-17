#!/usr/bin/env python3
"""Index the Python under `src/`, and write the facts out as JSON.

This is a **real indexer**: it parses with the standard library's `ast`, resolves
the references it can resolve, and emits exactly the facts the `aperture` shell
writes into its store. Python is here because its parser ships with it — the
point of the exercise is the *facts*, not the front end that finds them.

Run it from anywhere; it rewrites `index.json` next to itself:

    python3 example/index.py

The shell compiles that file in with `include_str!`, so `cargo build` picks the
new facts up. It is checked in because Aperture has no ingestion yet (PLAN
phase 7) — a fact file and a loader are what eventually replace both this script's
output format and the seeding code that reads it.

# What it emits

One array per predicate, in **write order**, and a row may only point at an array
before it — *by position*. A fact id is what a write returns ([I11]), so an indexer
cannot know one; a position is the only reference it can express, and `seed` in
`src/main.rs` turns each position into the id of the fact it wrote. Referential
integrity is then a consequence of the order rather than a check.

    {
      "files":   ["store/codec.py", ...],
      "modules": [{"name": "store.codec", "file": 0}, ...],
      "decls":   [{"module": 0, "name": "encode_key", "line": 31, "kind": "def"}, ...],
      "names":   [{"name": "encode_key", "to": 4}, ...],
      "refs":    [{"file": 0, "at": {"line": 8, "col": 12}, "to": 4}, ...],
      "imports": [{"from": 1, "to": 0}, ...]
    }

`names` is the same names again, keyed the other way round: a declaration's key
begins with its module, so a prefix of a *name* cannot narrow that scan, and the
answer in a fact database is a second predicate keyed by the name — Glean's
`SearchByName`. It is derived data written by hand, which is what a deriver does
until stored derivation exists (PLAN phase 8b).

# What it does not do

**Types, scopes, or any resolution needing either.** A reference resolves when a
bare name is one this module declares or imported by name, or when a dotted
attribute reaches through an imported module (`store.engine.store_open`). So a
method call on a value — `db.put(...)` — is not a reference here, because knowing
what `db` is means inferring its type; and a local variable shadowing a
declaration's name would be a false hit. A real indexer for a real language does
this properly. This one is honest about the line it stops at.
"""

from __future__ import annotations

import ast
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SRC = HERE / "src"
OUT = HERE / "index.json"


def module_of(path: str) -> str:
    """`store/codec.py` is the module `store.codec`."""
    return path.removesuffix(".py").replace("/", ".")


def span_of(node, name: str) -> dict:
    """Where a declaration's name starts, and where the whole thing ends.

    Two spans, not one, because a viewer wants both: the identifier is what you
    highlight and click, and the whole node is what you fold. `col_offset` points at
    the *statement* — `def` — so the name starts after the keyword, which is what the
    offsets below add back.
    """
    if isinstance(node, ast.AsyncFunctionDef):
        lead = len("async def ")
    elif isinstance(node, ast.FunctionDef):
        lead = len("def ")
    elif isinstance(node, ast.ClassDef):
        lead = len("class ")
    else:
        # An assignment target *is* the name, so its own column is the answer.
        lead = 0

    return {
        "col": node.col_offset + 1 + lead,
        "endLine": node.end_lineno or node.lineno,
        "endCol": (node.end_col_offset or node.col_offset) + 1,
    }


def declarations(tree: ast.Module):
    """What a module declares, in source order: `(name, line, kind, span)`.

    A class's methods are qualified with its name — `Store.put` — because that is
    what someone searching for one types, and it is what makes the search index
    below worth having: `Store` is then a prefix of five declarations.
    """
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            yield node.name, node.lineno, "def", span_of(node, node.name)
        elif isinstance(node, ast.ClassDef):
            yield node.name, node.lineno, "class", span_of(node, node.name)
            for member in node.body:
                if isinstance(member, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    yield (
                        f"{node.name}.{member.name}",
                        member.lineno,
                        "method",
                        span_of(member, member.name),
                    )
        elif isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    yield target.id, node.lineno, "const", span_of(target, target.id)


def imports(tree: ast.Module, module: str, modules: set[str], declared: dict) -> tuple:
    """What a module's imports bring into scope.

    Three things, because the two ways to import bind two different kinds of name:

    - `scope` maps a bare name to a declaration's position — its own declarations,
      and everything `from store.codec import encode_key` bound;
    - `aliases` maps a name to a module — what `import store.engine` bound, or its
      `as` name, which is what a dotted reference is resolved through;
    - `imported` is the set of modules *in this corpus* it imports, one
      `src.Import` fact each.
    """
    scope = {name: position for name, position in declared[module].items()}
    aliases: dict[str, str] = {}
    imported: set[str] = set()

    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name in modules:
                    imported.add(alias.name)
                    aliases[alias.asname or alias.name] = alias.name
        elif isinstance(node, ast.ImportFrom) and node.module in modules:
            imported.add(node.module)
            for alias in node.names:
                position = declared[node.module].get(alias.name)
                if position is not None:
                    scope[alias.asname or alias.name] = position

    return scope, aliases, imported


def through_module(node: ast.Attribute, aliases: dict, declared: dict):
    """`store.engine.store_open` — an attribute reached through an imported module.

    The dotted chain is flattened, everything but the last part has to name an
    imported module, and the last part has to be something that module declares.
    Anything else — an attribute of a value, of `self`, of a builtin — is not
    resolvable without types, and is left alone.
    """
    parts = [node.attr]
    base = node.value

    while isinstance(base, ast.Attribute):
        parts.append(base.attr)
        base = base.value

    if not isinstance(base, ast.Name):
        return None

    parts.append(base.id)
    parts.reverse()

    module = aliases.get(".".join(parts[:-1]))
    if module is None:
        return None

    return declared[module].get(parts[-1])


def references(tree: ast.Module, file: int, scope: dict, aliases: dict, declared: dict):
    """Every reference in a module that resolves to a declaration.

    `file` is the file the reference is *in*, and it is not derivable from the rest of
    the row: `to` reaches the file the declaration is in, which for most references is
    a different one — that being the point of a reference. A location is the file and
    the position together, so a row without the file names a line in no particular
    file and cannot be looked at.
    """
    found = []

    for node in ast.walk(tree):
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
            position = scope.get(node.id)
        elif isinstance(node, ast.Attribute) and isinstance(node.ctx, ast.Load):
            position = through_module(node, aliases, declared)
        else:
            continue

        if position is not None:
            # `col_offset` is 0-based and counts UTF-8 bytes; a column is reported
            # the way an editor shows it, from 1.
            #
            # `length` is the whole expression's, so `keys.encode_key` is one link
            # over both words rather than a link over `keys` alone. A reference that
            # wraps a line has no single extent, and 0 says so rather than guessing.
            one_line = (node.end_lineno or node.lineno) == node.lineno
            length = (node.end_col_offset or 0) - node.col_offset if one_line else 0

            found.append(
                {
                    "file": file,
                    "at": {
                        "line": node.lineno,
                        "col": node.col_offset + 1,
                        "length": length,
                    },
                    "to": position,
                }
            )

    # Source order, not walk order: `ast.walk` is breadth-first, and the facts read
    # better — and diff better — down the file.
    found.sort(key=lambda ref: (ref["at"]["line"], ref["at"]["col"], ref["to"]))
    return found


def main() -> int:
    paths = sorted(path.relative_to(SRC).as_posix() for path in SRC.rglob("*.py"))
    if not paths:
        print(f"index.py: nothing to index under {SRC}", file=sys.stderr)
        return 1

    modules = [module_of(path) for path in paths]
    trees = [
        ast.parse((SRC / path).read_text(encoding="utf-8"), filename=path)
        for path in paths
    ]

    # One file per module and one module per file, so a module's position is its
    # file's. Written first because everything else points at them.
    decls: list[dict] = []
    spans: list[dict] = []
    declared: dict[str, dict[str, int]] = {module: {} for module in modules}

    for position, (module, tree) in enumerate(zip(modules, trees)):
        for name, line, kind, span in declarations(tree):
            declared[module][name] = len(decls)
            spans.append({"decl": len(decls), **span})
            decls.append(
                {"module": position, "name": name, "line": line, "kind": kind}
            )

    names = [{"name": decl["name"], "to": position} for position, decl in enumerate(decls)]

    refs: list[dict] = []
    edges: list[dict] = []

    for position, (module, tree) in enumerate(zip(modules, trees)):
        scope, aliases, imported = imports(tree, module, set(modules), declared)
        refs.extend(references(tree, position, scope, aliases, declared))
        edges.extend(
            {"from": position, "to": modules.index(target)}
            for target in sorted(imported)
        )

    index = {
        "files": paths,
        "modules": [
            {"name": module, "file": position}
            for position, module in enumerate(modules)
        ],
        "decls": decls,
        "spans": spans,
        "names": names,
        "refs": refs,
        "imports": edges,
    }

    OUT.write_text(json.dumps(index, indent=2) + "\n", encoding="utf-8")

    total = sum(len(rows) for rows in index.values())
    print(
        f"{OUT.relative_to(HERE.parent)}: {total} facts — "
        f"{len(paths)} files, {len(decls)} declarations, "
        f"{len(refs)} references, {len(edges)} imports",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
