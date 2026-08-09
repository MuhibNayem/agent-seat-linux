//! Typed error shared by every fallible agent-seat operation.

/// Every failure mode of the agent seat, from session discovery through
/// capture and input injection.
///
/// Low-level APIs such as [`crate::AgentSeat`] and [`crate::SeatApp`] return
/// this type directly; the high-level [`crate::Error`] wraps it and exposes
/// it through [`std::error::Error::source`]. The `String` payloads carry
/// free-form context while the variant identifies the failing subsystem.
#[derive(Debug, thiserror::Error)]
pub enum SeatError {
    /// No Wayland session exists to proxy into: `WAYLAND_DISPLAY` is unset
    /// or the compositor socket it names does not exist.
    #[error("no Wayland session is available to proxy into")]
    NoWaylandSession,

    /// `XDG_RUNTIME_DIR` is unset or empty, so no socket path can be derived.
    #[error("XDG_RUNTIME_DIR is not set; no runtime directory for the seat socket")]
    MissingRuntimeDir,

    /// The private agent seat socket could not be bound or configured.
    #[error("could not create the agent seat socket: {0}")]
    SocketCreate(String),

    /// The XWayland compatibility bridge failed to start or serve.
    #[error("XWayland bridge failure: {0}")]
    Xwayland(String),

    /// `xauth` credential setup for the XWayland bridge failed.
    #[error("XWayland authority failure: {0}")]
    Xauth(String),

    /// Kernel peer credentials of a connecting app could not be read or did
    /// not validate.
    #[error("peer credential failure: {0}")]
    PeerCredential(String),

    /// Reading or decoding the app's rendered frame failed.
    #[error("capture failure: {0}")]
    Capture(String),

    /// Synthesizing or delivering input events to the app failed.
    #[error("input injection failure: {0}")]
    Input(String),

    /// Spawning or supervising a helper process or worker thread failed.
    #[error("process failure: {0}")]
    Process(String),

    /// An underlying I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Any other failure that does not fit a dedicated variant.
    #[error("{0}")]
    Custom(String),
}

impl From<String> for SeatError {
    fn from(message: String) -> Self {
        Self::Custom(message)
    }
}

impl From<&str> for SeatError {
    fn from(message: &str) -> Self {
        Self::Custom(message.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_conversions_become_custom() {
        let owned = SeatError::from(String::from("boom"));
        assert!(matches!(&owned, SeatError::Custom(_)));
        assert_eq!(owned.to_string(), "boom");

        let borrowed = SeatError::from("boom");
        assert!(matches!(&borrowed, SeatError::Custom(_)));
        assert_eq!(borrowed.to_string(), "boom");
    }

    #[test]
    fn io_errors_keep_their_message_and_chain() {
        // A real source-bearing io::Error keeps its chain through `Io`.
        let inner = std::io::Error::other("disk");
        let has_source = std::error::Error::source(&inner).is_some();
        let error = SeatError::from(inner);
        assert!(matches!(&error, SeatError::Io(_)));
        assert!(error.to_string().contains("disk"));
        assert_eq!(std::error::Error::source(&error).is_some(), has_source);
    }
}
