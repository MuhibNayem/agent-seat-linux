# Compatibility matrix

`agent-seat-linux` is validated continuously against a small matrix and best-effort
against a wider one. This file is the source of truth for what "supported" means
and how to extend the matrix.

## Support tiers

| Tier | Meaning |
|---|---|
| **Validated** | Exercised in CI or by a maintainer on real hardware; breakage is a release blocker. |
| **Expected** | The mechanism should work (it only requires `wl_shm` + our private proxy); not yet exercised — please report results. |
| **Unsupported** | Known not to work; tracked with an issue. |

## Compositors / sessions

| Compositor | Session | Capture | Input | Tier | Notes |
|---|---|---|---|---|---|
| GNOME / Mutter | Wayland | ✅ | ✅ | Validated | Primary development target. |
| weston | Wayland | ✅ | ✅ | Validated | Used for headless/dev testing. |
| KDE Plasma / KWin | Wayland | ✅ | ✅ | Expected | Report via issue. |
| Sway / wlroots | Wayland | ✅ | ✅ | Expected | Report via issue. |
| Hyprland | Wayland | ✅ | ✅ | Expected | Report via issue. |
| Any X11-only session | X11 | — | — | Unsupported | This crate targets Wayland; use the XWayland bridge only as an in-session fallback. |

## Application toolkits

The launcher injects software-rendering / environment hints per toolkit so a
readable `wl_shm` frame is produced; if a native toolkit yields no readable
frame, the high-level API retries through the owner-authenticated XWayland bridge.

| Toolkit | Native Wayland | XWayland fallback | Tier |
|---|---|---|---|
| GTK 3/4 | ✅ | ✅ | Validated |
| Qt 5/6 (wayland) | ✅ | ✅ | Validated |
| Electron / Chromium | ✅ | ✅ | Validated |
| SDL2 | ✅ | ✅ | Expected |
| Java / XToolkit | — | ✅ | Expected |
| GPU-only / DMA-BUF-only clients | ⚠️ | ✅ | Expected | Falls back to XWayland; no DMA-BUF readback yet. |

## Requirements recap

- Linux with an active Wayland session (`XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`).
- `Xwayland` + `xauth` for the compatibility fallback.
- Apps must be launched through the library (no attach to arbitrary windows).
- Capture requires `wl_shm` (software rendering) today.

## Validating a new target

1. Run the bundled example against a representative app:
   `cargo run --example basic -- <app>` and `cargo run --example agent_loop -- <app>`.
2. Confirm `ControlledApp::transport()` reports the expected `Transport`.
3. Confirm `capture()` returns a non-blank frame and that click/key/scroll/text
   input land in the app.
4. Open a PR adding a row above with the tier flipped to **Validated**, noting
   distro + compositor/toolkit versions.
