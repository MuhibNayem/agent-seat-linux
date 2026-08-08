# Security policy

`agent-seat-linux` is security-sensitive infrastructure. It proxies a display
protocol and synthesizes input, so changes to authentication, protocol
filtering, process ownership, or cleanup require explicit security review.

## Supported versions

The latest `0.1.x` release is supported while the project is experimental.
Security fixes may include breaking API changes until `1.0`.

## Reporting a vulnerability

Do not open a public issue for an undisclosed vulnerability. Use GitHub's
private vulnerability reporting or Security Advisory feature for this
repository. Include:

- affected version and Linux distribution;
- compositor and toolkit;
- reproduction steps or proof of concept;
- observed process, socket, file-permission, or cross-application impact.

## Trust model

The library assumes:

- the harness and controlled application run under the same trusted Unix user;
- the harness deliberately selected the executable it launches;
- the host compositor, kernel, XWayland, and Rust dependencies are trusted;
- environment variables inherited by the harness are trusted configuration.

The library does not sandbox the controlled application. That application
retains the user's filesystem, network, D-Bus, and other operating-system
permissions unless the embedding harness supplies a separate sandbox.

## Security invariants

- Only same-UID peers are accepted, based on kernel `SO_PEERCRED` data.
- The private Wayland socket is mode `0600`.
- Privileged cross-client Wayland interfaces are never advertised.
- Ambient whole-desktop capture is not implemented.
- XWayland uses a high-entropy MIT-MAGIC-COOKIE, mode `0600` Xauthority,
  mode `0700` runtime directory, and `-nolisten tcp`.
- Controlled children receive a parent-death signal and are explicitly reaped.
- Every temporary path is exact, private, and removed after its owner exits.
- Shutdown is idempotent and closes active proxy connections before joining
  worker and bridge threads.

Any change that weakens an invariant requires a documented threat-model update.

## Non-goals

- Safely executing malicious or untrusted applications.
- Bypassing the compositor to control applications not launched by the caller.
- Hiding controlled applications from the desktop user.
- Replacing Flatpak, Bubblewrap, containers, seccomp, or an operating-system
  permission model.
