# Releasing headroom-hook

`main` is what's in prod today: protected, PR-only, never pushed to directly by
anything (human or bot). `dev` is where work — including automated dependency
bumps — accumulates ahead of the next release.

## Automated dependency bumps

`.github/workflows/headroom-release-watch.yml` polls `headroomlabs-ai/headroom`
daily for a new release newer than `FLOOR`. When one appears, it:

1. Re-pins `headroom-core` to the new release tag.
2. Builds + tests (lexical/no-ml) — must be green before anything is pushed.
3. Bumps this crate's own patch version.
4. Pushes the commit to `dev`.
5. Opens a PR from `dev` -> `main` (or pushes onto an already-open one), requesting
   review from the repo owner.

Nothing is tagged or published at this point — the bump only exists on `dev` and
in an open PR.

## Publishing a release

Once that PR is reviewed and merged into `main`,
`.github/workflows/headroom-release-publish.yml` triggers automatically (on the
PR's `closed`+`merged` event), and:

1. Reads the version from `main`'s `Cargo.toml`.
2. Tags the merged commit `vX.Y.Z`.
3. Triggers `docker.yml` and `release.yml` against that tag (via
   `workflow_dispatch`, since a `GITHUB_TOKEN`-pushed tag doesn't self-trigger
   `on: push: tags:`) — these build and publish the signed plugin tarballs and
   the `getbusbar/headroom-hook` image.

So: **reviewing/merging the dev -> main PR is the one human touchpoint** in an
otherwise fully automated pipeline — approve it (GitHub's normal "review
requested" email works for this) and the tag + publish happen on their own.

## Manual releases

Anything not driven by the upstream-headroom bump (a manual feature, a fix) goes
through the same shape by hand: commit to a branch off `dev`, PR into `dev`,
then PR `dev` -> `main` when ready to ship — merging that PR triggers the same
tag-and-publish automation above.
