//! App-scoped capture and input for Linux automation agents.
//!
//! `agent-seat-linux` exposes a private Wayland socket. Applications launched
//! with that socket as `WAYLAND_DISPLAY` remain ordinary visible windows on
//! the user's compositor, while the owning process can capture their
//! `wl_shm` frames and inject input at the application boundary.
//!
//! The crate deliberately does not capture the ambient desktop and does not
//! forward privileged Wayland interfaces. See [`AgentSeat`] to get started.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("agent-seat-linux supports Linux only");

mod harness;
mod keymap;
mod process;
mod seat;

pub use harness::{
    ComputerUse, ComputerUseBuilder, ControlledApp, Error, LaunchConfig, PointerButton, Result,
    Transport,
};
pub use seat::{available, seat, shutdown, AgentSeat, CapturedFrame, SeatApp, XwaylandBridge};
