---
name: release
description: Publish a new GitHub Release of pnl-bot by bumping the Cargo.toml version, tagging vX.Y.Z and pushing, so the Release workflow attaches the static x86_64/aarch64 Linux builds. Use when the user asks to release, publish, cut or tag a version.
argument-hint: "[patch|minor|major|X.Y.Z]"
disable-model-invocation: true
---

# Release pnl-bot

`.github/workflows/release.yml` runs on any pushed `v*` tag: it builds
`pnl-bot-linux-x86_64.tar.gz` and `pnl-bot-linux-aarch64.tar.gz`, then creates a
GitHub Release with both archives plus `SHA256SUMS`. The build **fails if the tag
does not equal `v` + the `version` in `Cargo.toml`**, so the version bump and the
tag must always agree.

Argument: `$ARGUMENTS` — `patch` (default), `minor`, `major`, or an explicit `X.Y.Z`.

## 1. Preflight

Stop and report if any of these fail:

- `git status --porcelain` is empty (no uncommitted changes).
- Current branch is `master`.
- `git fetch origin --tags` then `master` is not behind `origin/master`.
- `gh auth status` succeeds (needed to watch the run and check the release).
- `cargo build --release --locked` succeeds locally (catches breakage before CI).

## 2. Pick the version

- Current version: `grep -m1 '^version' Cargo.toml | cut -d'"' -f2`.
- Latest tag: `git tag --list 'v*' --sort=-v:refname | head -1`.
- If the current version has **no tag yet** (e.g. the first release), and no
  explicit version was given, release the current version as-is — skip step 3.
- Otherwise compute the new version from the argument. It must be greater than
  the latest tag and `vX.Y.Z` must not already exist locally or on `origin`
  (`git ls-remote --tags origin vX.Y.Z`).

## 3. Bump the version

- Edit only the `version = "..."` line in the `[package]` section of `Cargo.toml`.
- Refresh the lockfile: `cargo update -p pnl-bot --offline` (falls back to
  `cargo check` if offline fails). `Cargo.lock` must change only for `pnl-bot`.
- Commit both files: `Release vX.Y.Z`.

## 4. Confirm, then tag and push

Pushing a tag publishes a public release, so show the user the version, the
commits since the previous tag (`git log --oneline <prev>..HEAD`) and ask for
confirmation before pushing — unless they already explicitly said to go ahead.

```sh
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin master
git push origin vX.Y.Z
```

Push the branch before the tag so the tagged commit is on `master`.

## 5. Watch the build and verify the release

- Find the run: `gh run list --workflow release.yml --branch vX.Y.Z --limit 1`
  (the run can take a few seconds to appear), then `gh run watch <id> --exit-status`
  in the background.
- On success, `gh release view vX.Y.Z` must list three assets:
  `pnl-bot-linux-x86_64.tar.gz`, `pnl-bot-linux-aarch64.tar.gz`, `SHA256SUMS`.
  Give the user the release URL.
- On failure, show the failing step with `gh run view <id> --log-failed`. Do not
  delete or move the tag on your own; explain the options (fix and release the
  next patch version, or with the user's approval delete the tag/release with
  `gh release delete vX.Y.Z --cleanup-tag` and re-tag after the fix).
