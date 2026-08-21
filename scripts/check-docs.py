#!/usr/bin/env python3
"""The drift gate: the checks that would have caught the documentation going stale.

Four checks, each of which failed silently once:
  1. every relative link and anchor in website/content/ resolves;
  2. every `invariants.md#iN`-style citation in crates/, docs/ and clients/ resolves
     to an anchor the registry actually declares;
  3. nothing references a documentation file that no longer exists;
  4. no `Phase [0-9]` reference survives in crates/ or website/ — the compiler-pass
     sense of "phase" is fine, a build-plan number is not (bench/ keeps its history).

Standard library only, like the site generator. Exit 1 on any finding.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONTENT = ROOT / "website" / "content"

findings: list[str] = []


def fail(message: str) -> None:
    findings.append(message)


# ---- 1. site-internal links and anchors -----------------------------------------

def site_anchors(text: str) -> set[str]:
    anchors = set(re.findall(r'<a id="([A-Za-z0-9_-]+)"></a>', text))
    anchors |= set(re.findall(r"\{#([A-Za-z0-9_-]+)\}", text))
    for heading in re.findall(r"^#{1,6} +(.+?)(?:\{#[A-Za-z0-9_-]+\})?$", text, re.M):
        # **The marks come off first**, as `build.py` and `web/`'s parser both do
        # (`plain`, then `slugify`). A heading that cites an invariant carries a
        # link, and slugifying the raw text mangles the target into the anchor —
        # which reads as a missing anchor for a link that resolves perfectly.
        stripped = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", heading.strip())
        stripped = re.sub(r"[`*~]", "", stripped)
        slug = re.sub(r"[^a-z0-9 -]", "", stripped.lower())
        anchors.add(re.sub(r"[ -]+", "-", slug).strip("-"))
    return anchors


pages = {p.stem: p.read_text(encoding="utf-8") for p in CONTENT.glob("*.md")}
anchors = {slug: site_anchors(text) for slug, text in pages.items()}

for slug, text in pages.items():
    for target in re.findall(r"\]\(([^)]+)\)", text):
        if target.startswith(("http://", "https://", "mailto:")):
            continue
        page, _, anchor = target.partition("#")
        if page and not page.endswith(".html"):
            continue  # not a site link
        name = page[:-5] if page else slug
        if name not in pages:
            fail(f"website/content/{slug}.md links to missing page {target}")
        elif anchor and anchor not in anchors[name]:
            fail(f"website/content/{slug}.md links to missing anchor {target}")

# ---- 2. invariant citations resolve ----------------------------------------------

registry = anchors.get("invariants", set())
citation = re.compile(r"invariants\.md#([A-Za-z0-9_-]+)")

for area in ("crates", "docs", "clients", "bench"):
    for path in (ROOT / area).rglob("*"):
        if path.suffix not in {".rs", ".md", ".cs", ".llw", ".toml"}:
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        for anchor in citation.findall(text):
            if anchor not in registry:
                fail(f"{path.relative_to(ROOT)} cites invariants.md#{anchor}, not in the registry")

# ---- 3. no references to retired documentation files -----------------------------

RETIRED = re.compile(
    r"docs/(0[1-7]-[a-z-]+|invariants|conventions|testing|fjord-cli-design|glossary"
    r"|open-decisions|performance|auth|query-surface|repository-rules"
    r"|glean-comparison|glean-capabilities|phase-[0-9.]+[a-z-]*)\.md"
)

for area in ("crates", "docs", "clients", "bench", "website", "scripts", ".github"):
    base = ROOT / area
    if not base.exists():
        continue
    for path in base.rglob("*"):
        if path.suffix not in {".rs", ".md", ".cs", ".llw", ".toml", ".yml", ".py", ".sh"}:
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        for line_no, line in enumerate(text.splitlines(), 1):
            if RETIRED.search(line):
                fail(f"{path.relative_to(ROOT)}:{line_no} references a retired doc: {line.strip()[:90]}")

for name in ("README.md", "AGENTS.md", "PLAN.md", "CLAUDE.md", "CHANGELOG.md", "Cargo.toml"):
    text = (ROOT / name).read_text(encoding="utf-8")
    for line_no, line in enumerate(text.splitlines(), 1):
        if RETIRED.search(line):
            fail(f"{name}:{line_no} references a retired doc: {line.strip()[:90]}")

# ---- 4. no build-plan phase numbers in code or the book ---------------------------

PHASE = re.compile(r"Phase [0-9]")
for area in ("crates", "website"):
    for path in (ROOT / area).rglob("*"):
        if path.suffix not in {".rs", ".md", ".llw", ".toml"}:
            continue
        text = path.read_text(encoding="utf-8", errors="ignore")
        for line_no, line in enumerate(text.splitlines(), 1):
            if PHASE.search(line):
                fail(f"{path.relative_to(ROOT)}:{line_no} carries a phase number: {line.strip()[:90]}")

if findings:
    print(f"{len(findings)} finding(s):", file=sys.stderr)
    for finding in findings:
        print(f"  {finding}", file=sys.stderr)
    sys.exit(1)

print("docs are consistent: links resolve, citations resolve, nothing retired is referenced")
