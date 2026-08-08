# agent-seat-linux

App-scoped Wayland capture and input for Linux automation agents.

`agent-seat-linux` gives an agent harness a complete computer-use primitive:
launch an application as a normal visible desktop window, capture only that
application, inject pointer and keyboard input into its private Wayland
connection, and tear everything down when the harness exits.

It does **not** capture the ambient desktop, move the user's real pointer, or
depend on Tauri, Chorus, an LLM provider, or a particular agent framework.

> Status: experimental `0.1`. The API and compatibility matrix may change
> while more compositors and toolkits are validated.

## Why this exists

Wayland deliberately prevents ordinary clients from reading other clients'
surfaces or injecting input into them. Desktop portals can provide whole-screen
or user-approved remote-desktop access, but that is a poor primitive for an
agent that should control exactly one application.

This library creates a private Wayland socket and transparently proxies the
controlled application to the user's real compositor. The app stays visible
and usable by hand. Its `wl_shm` frames and input objects pass through the
private seat, allowing capture and input at the app boundary. If a native
Wayland toolkit does not expose a readable frame, the high-level API retries
through an owner-authenticated XWayland bridge automatically.

## Capabilities

- Launch any executable using a framework-neutral `LaunchConfig`.
- Automatic native Wayland to XWayland compatibility fallback.
- Window-scoped RGB frame capture.
- Click, modifier-click, scroll, drag, key combinations, and Unicode text.
- Multiple toolkit support through environment hints: GTK, Qt, Electron,
  SDL, and Java/XToolkit fallback.
- Same-UID peer authentication with Linux `SO_PEERCRED`.
- Owner-only seat sockets, Xauthority files, and runtime directories.
- No X11 TCP listener.
- Parent-death process handling, explicit child reaping, and Drop cleanup.
- Privileged Wayland interfaces are filtered from controlled clients.

## Requirements

- Linux with an active Wayland session.
- `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` in the harness environment.
- `Xwayland` and `xauth` for compatibility fallback.
- Apps must be launched through this library. It does not attach to arbitrary
  already-running windows.
- Capture currently requires `wl_shm`; the launcher supplies software-rendering
  hints and falls back to XWayland when necessary.

Ubuntu/Debian development dependencies:

```bash
sudo apt install libwayland-dev libxkbcommon-dev xwayland xauth
```

## Add it to a harness

Until a crates.io release is published:

```toml
[dependencies]
agent-seat-linux = { git = "https://github.com/MuhibNayem/agent-seat-linux", tag = "v0.1.0" }
```

## Complete example

```rust,no_run
use agent_seat_linux::{ComputerUse, LaunchConfig, PointerButton};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let harness = ComputerUse::new()?;
    let mut app = harness.launch(LaunchConfig::new("gnome-calculator"))?;

    let frame = app.capture()?;
    frame.image.save("calculator.png")?;

    app.click(320.0, 420.0, PointerButton::Left, 1)?;
    app.press_key("ctrl+a")?;
    app.type_text("42")?;

    app.stop();
    Ok(())
}
```

Run the included example:

```bash
cargo run --example basic -- gnome-calculator
```

The app's `Transport` reports whether native Wayland or XWayland was selected.

## Using another language or agent framework

The crate's high-level boundary is deliberately small:

1. Your harness converts its tool call into `LaunchConfig`.
2. `ComputerUse::launch` returns one `ControlledApp`.
3. Route screenshot, click, scroll, drag, key, and text tools to that handle.
4. Drop `ControlledApp` to stop the application; drop `ComputerUse` to close
   the seat, bridges, threads, sockets, and credentials.

Python, Node, JVM, Go, and C harnesses can wrap this API through their usual
Rust FFI layer without inheriting any Chorus-specific data model.

## Low-level API

Advanced consumers can construct `AgentSeat` directly, configure their own
`std::process::Command`, select a connected `SeatApp`, or own the
`XwaylandBridge` lifecycle. Most harnesses should use `ComputerUse`.

## Security boundary

This is a control primitive, not a sandbox. It is designed to reduce authority
to applications intentionally launched through its private socket, but the
controlled program still runs with the harness user's normal operating-system
permissions. Do not use it to execute untrusted binaries.

Read [SECURITY.md](SECURITY.md) before embedding the library in a product.

## Current limitations

- Linux/Wayland only. X11 desktop sessions are not the target of this crate.
- No GPU DMA-BUF readback yet; software-rendered `wl_shm` is used instead.
- No attachment to arbitrary existing applications.
- Accessibility semantics such as click-by-label are intentionally outside the
  core library; frameworks can layer AT-SPI on top.
- File chooser, notification, tray, and privileged compositor protocols may
  require host-specific integration and are not forwarded by default.

## Origin

The mechanism was developed for the Linux computer-use backend of
[Chorus](https://github.com/MuhibNayem/chorus-app) and extracted as a standalone
library so other agent harnesses can share and improve it.

## License

Licensed under either Apache License 2.0 or MIT, at your option.
