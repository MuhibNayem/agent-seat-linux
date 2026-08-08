# Contributing

Contributions are welcome, especially compatibility reports for compositors,
toolkits, graphics stacks, and input methods.

## Development setup

```bash
sudo apt install libwayland-dev libxkbcommon-dev xwayland xauth
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo package --allow-dirty
```

Live tests are ignored by default because they launch visible applications.
Run them only inside an active Wayland session:

```bash
cargo test seat::tests::proxy_captures_app_frame -- --ignored --nocapture
```

## Pull requests

- Keep Chorus, Tauri, and agent-provider types out of the crate.
- Add a regression test for every protocol, lifecycle, or security fix.
- Preserve app-scoped capture; do not add ambient desktop capture.
- Document newly advertised Wayland interfaces and explain their authority.
- Treat new `unsafe` blocks as security-sensitive and document their safety.
- Update `SECURITY.md` when a trust-boundary invariant changes.

By contributing, you agree that your contribution is licensed under the
project's `MIT OR Apache-2.0` terms.
