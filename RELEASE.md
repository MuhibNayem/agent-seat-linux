# Release checklist (crates.io)

Releases are cut from `main` by tagging `vX.Y.Z`; CI publishes to crates.io and
docs.rs builds the documentation automatically.

## Before tagging

- [ ] `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test --all-targets`, and `cargo doc --no-deps` are green (CI enforces).
- [ ] `cargo package --list` succeeds and the package contains no secrets,
      sockets, or large assets (`target/` is excluded automatically).
- [ ] `Cargo.toml` version bumped and `CHANGELOG.md` has an entry for it
      (keep `[Unreleased]` → versioned section).
- [ ] `README.md`, `COMPATIBILITY.md`, and `docs/FFI.md` reflect the release
      (capabilities, tiers, API changes).
- [ ] Semver review: any breaking change to the public API requires a minor
      (pre-1.0) / major (post-1.0) bump and a migration note in the CHANGELOG.
- [ ] `rust-version` (MSRV) still builds: `cargo +<msrv> check`.

## Publishing

Publishing is automated by `.github/workflows/release.yml` on tag push. For a
manual publish (e.g. first release or CI outage):

1. `cargo login $CARGO_REGISTRY_TOKEN` (token scoped to `publish:update`).
2. `cargo publish --dry-run` then `cargo publish`.
3. Verify the crate page and docs.rs build for the new version.
4. Create the GitHub release from the tag with the CHANGELOG notes.

## After publishing

- [ ] Add a downstream smoke test: a fresh project depending on
      `agent-seat-linux = "X.Y"` from crates.io builds and runs `examples/basic`.
- [ ] Announce the release and update the compatibility matrix with any newly
      validated compositors/toolkits.

## Secrets

- `CARGO_REGISTRY_TOKEN` lives only in the repo's GitHub Actions secrets; it is
  never committed and the release workflow runs only on tag pushes from `main`.
