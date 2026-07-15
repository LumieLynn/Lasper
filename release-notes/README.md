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

## `0.3.0` Stable Release Bar

Do not publish `v0.3.0` stable only because the alpha branch builds. The stable
release should mean the current feature set has a credible safety and recovery
baseline.

Before tagging `v0.3.0`, the project should have:

- no normal product path depending on arbitrary elevated `program + argv`
  execution;
- no normal product path depending on unrestricted absolute-path daemon file
  operations;
- typed daemon operations for machine lifecycle, terminal FD requests, host
  configuration, and the highest-risk storage/provisioning mutations;
- clear user-facing behavior for OCI as experimental or explicitly supported;
- conflict-safe image creation and deletion behavior for `/var/lib/machines`;
- smoke-tested creation, start, terminal, NVIDIA/Wayland, deletion, and cleanup
  on at least one normal systemd host and one WSL-like host when applicable;
- release notes that list remaining caveats rather than implying production
  completeness.
