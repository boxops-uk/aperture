# What this repository enforces, and what it cannot

Fjord's history is the evidence for its design: every chapter cites commits, and
[`bench/FINDINGS.md`](../bench/FINDINGS.md) numbers are only worth reading if the tree that
produced them is the tree recorded. So the rules below are not policy decoration — they are
what makes a commit citable. All of them are **server-side rulesets with no bypass actors**,
which means they apply to repository admins too.

## Enforced

| Rule | Scope | What it refuses |
|---|---|---|
| Signed commits | every branch | a push containing any commit GitHub cannot verify |
| No force-push | every branch | rewriting a pushed ref |
| Linear history | `main`, `release/*` | a merge commit |
| No deletion | `main`, `release/*` | deleting the branch |
| Pull request required | `main`, `release/*` | pushing straight to the branch |
| `build` check | `main`, `release/*` | landing anything that does not compile under the tree's own `Cargo.lock` |
| `attest` check | `release/*` | landing without SLSA provenance |
| No tag re-pointing | every tag | moving a tag to another commit |
| No tag deletion | `v*` | unmaking a release tag |

Merges are squash or rebase only; merge commits are disabled repository-wide, so linear
history is enforced by two independent mechanisms rather than one.

`GITHUB_TOKEN` defaults to **read**, and workflows may not approve pull requests — otherwise
a workflow could satisfy a review requirement or push to `main` itself. A fork's workflow
needs approval before it runs at all.

Write access is the other half of all of this, and currently one account has it. There are no
outside collaborators, no teams and no deploy keys, so a branch can only be created by the
account that owns the repository; anyone else must fork.

## Verifying a binary

Every push to `main` or `release/*`, and every `v*` tag, signs provenance naming the
binaries it built:

```
gh attestation verify ./fjord --repo boxops-uk/fjord
```

A binary the workflow did not build has no attestation and fails this check.

## What is *not* enforced, and why

These are limits of the platform, recorded so nobody mistakes them for guarantees.

- **A tag object may be unsigned.** GitHub's API accepts `required_signatures` on a tag
  ruleset and never applies it — the ruleset stores as active and enforces nothing. The rule
  was removed rather than left reading as a promise. What holds instead: an unsigned commit
  cannot enter the repository by any route (branch push, tag push, or REST ref creation all
  refuse it), so a tag can only ever point at a commit that is already verified.
- **An admin can weaken any of this.** A repository admin can disable or delete a ruleset.
  The audit log records it; nothing prevents it. Moving these to organisation-level rulesets
  puts them beyond repository admins, leaving only organisation owners.
- **A pull request can edit its own gate.** A change to `.github/workflows/release.yml` is
  checked by the workflow as that change leaves it. A reusable workflow pinned from `main`
  would close this; one seat cannot.
- **Publishing a release is not gated.** Nothing stops a hand-uploaded binary on a GitHub
  Release. The guarantee is the consumer's: verify the attestation.
- **"Verified" means signed by a key registered to an account with access** — not
  necessarily by a person, and on `main` usually not by us. Because `main` takes changes only
  by pull request and GitHub offers no fast-forward-only merge, GitHub creates the merge
  commit and signs it with **its own** key: the commit reads `committer GitHub` and verifies
  against `B5690EEEBB952194`, not against the signing key every other commit carries. The
  badge therefore says *GitHub vouches this merge happened*, which is a weaker claim than the
  one the 232 extracted commits make about themselves. Locally such a commit reports `E`
  rather than `G`, since this repository verifies SSH signatures and that one is PGP.
- **Review is not enforced.** GitHub cannot require approval conditionally on who wrote the
  pull request, and never lets an author approve their own, so any non-zero approval count
  deadlocks a sole maintainer — bypass actors do not help, and an organisation owner's
  implicit admin satisfies neither the `OrganizationAdmin` nor the `RepositoryRole` form. The
  rule is written and **parked disabled**, to be enabled the day a second account has write
  access. Until then what stands in for it is access control, above.
