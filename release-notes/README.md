# Release Notes

GitHub releases use manually maintained notes from this directory. Commit
messages are not used to generate release content.

For a release with `version = "X.Y.Z"` in `Cargo.toml`:

1. Create `release-notes/vX.Y.Z.md`.
2. Update `Cargo.toml` and `Cargo.lock` to the same version.
3. Merge the release commit into `main` and wait for CI to pass.
4. Create and push the `vX.Y.Z` tag on that `main` commit.

The release workflow rejects tags that do not match `Cargo.toml`, do not point
to `main` history, or do not have a non-empty matching release note. Pre-release
versions such as `0.3.0-alpha.1` are published as GitHub pre-releases.
