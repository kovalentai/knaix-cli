# Releasing the Knaix CLI

A release is a group of changes that ship together under one version, not one
change per version. Everything bound for the same release is collected on a
release branch, and that branch reaching `main` is what starts the machinery.

## The shape

```
main ──┬── release/v0.5.4 ◄── feat/streaming-repl
       │          ▲       ◄── fix/citation-markers
       │          │       ◄── feat/doc-scoped-chat
       │          │
       │          └── chore(release): bump + CHANGELOG
       │                    │
       └────────────────────┘  squash → tag v0.5.4 → build, sign, publish
```

One release branch is open at a time. Feature and fix branches base on it and
open their pull requests against it, never against `main`. CI enforces this, so
a pull request aimed at the wrong place fails rather than quietly ungrouping
itself.

## Choosing the version

The CLI is pre-1.0, so the major stays at `0` and the minor carries the weight:

| Bump | When |
| --- | --- |
| **minor** (`0.5.3` → `0.6.0`) | An interface someone could be scripting against changes or goes away: a flag removed or renamed, an exit code repurposed, `-o json` output restructured. Also for a capability large enough to be the headline of the release. |
| **patch** (`0.5.3` → `0.5.4`) | Everything else, new flags and new commands included. Adding a capability without disturbing an existing one is a patch. |

The second row is the one that surprises people, so it is worth stating plainly:
a `feat` commit does not on its own mean a minor bump. Releases 0.4.6 through
0.4.9 all carried `Added` sections. Under strict semver each would have taken a
minor, and the version would say a great deal about how much shipped and nothing
about whether an upgrade can break you. Before 1.0 the second question is the
useful one.

`cut-release` reads the commits on the branch and proposes a version on that
basis. It is a proposal. If a release has a headline that the commit prefixes
cannot see, override it.

## Cutting one

**1. Open the branch.** From `main`, in both this repository and `knaix-docs`,
with the same name in each so the docs change and the CLI change it documents
pair by version:

```bash
git checkout main && git pull
git checkout -b release/v0.5.4 && git push -u origin release/v0.5.4
```

The name is a guess at this point. If the release grows a breaking change later,
rename the branch, or keep it and let the release PR carry the real number.

**2. Merge work into it.** Ordinary pull requests, based on the release branch
and merged into it. Each one that changes something a user can see ships its
`knaix-docs` pull request in the same session, against the matching release
branch there.

Leave `CHANGELOG.md` alone in these. It is compiled once, in step 3, and a
feature branch editing it only produces conflicts with the next feature branch.

**3. Cut the release.** Run the **Cut a release** workflow from the Actions tab,
naming the release branch. It proposes the version, sets it in `Cargo.toml` and
`Cargo.lock` via `scripts/bump-version.sh`, drafts a `CHANGELOG.md` section from
the merged pull request titles, and commits the result to the branch.

Both scripts run locally too, if you would rather do it by hand:

```bash
scripts/bump-version.sh "$(scripts/propose-version.sh)"
scripts/draft-changelog.sh 0.5.4
```

The draft is a skeleton with every change listed under its heading. Rewrite it.
The changelog is read by people deciding whether to upgrade, so each entry
should say what changed and why it mattered, which no generator knows.

**4. Open the release pull request.** The workflow summary has a link that
prefills it. It stops one click short on purpose: a pull request opened by a
workflow raises no `pull_request` event, so CI would never run on it and the
required checks would never appear.

**5. Merge and tag.** Once it is green and the changelog reads the way you want:

```bash
git checkout main && git pull
git tag v0.5.4 && git push origin v0.5.4
```

The tag must match `Cargo.toml`; `release.yml` refuses one that does not.

**6. Merge the docs.** Merge the paired `release/v0.5.4` in `knaix-docs` once the
publish has finished. Its CI reads `latest-version` and will not let the
changelog or the install page stay behind what the bucket serves.

## What happens after the tag

Nothing else needs a human. `release.yml` builds all five platform targets,
signs them keylessly with cosign, attests their provenance, publishes the GitHub
Release with an SBOM, then uploads the platform binaries to the release bucket,
writes `latest-version`, and invalidates the CDN.

`latest-version` is written **last, and only if every binary uploaded**. It is
the switch: `install.sh` reads it to decide what to fetch, so a version named
there whose binaries are missing is an install that fails for everyone. Writing
it last means a publish that dies halfway leaves the previous release serving
rather than a broken new one.

The Homebrew formula follows on its own. `kovalentai/homebrew-tap` polls hourly
and bumps itself once the bucket serves the new version, so brew is current
within the hour with nothing pushed from here.

## When something goes wrong

**The tag was wrong.** Delete it locally and remotely, fix, tag again. Nothing
is published until the workflow finishes, and the GitHub Release step re-runs
cleanly over an existing tag via `workflow_dispatch`.

**The publish failed partway.** Re-run the failed job. Uploads overwrite and the
release prefix is invalidated afterwards, so replaced bytes actually reach
people rather than sitting behind a year-long cache. `latest-version` still only
lands if every binary is present, so a re-run either completes the publish or
leaves it where it was.

**A release shipped and should not have.** Do not delete the binaries; anyone who
already installed will be verifying against them. Publish the fix as the next
patch and point `latest-version` at it.
