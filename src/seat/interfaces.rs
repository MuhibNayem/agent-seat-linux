//! Static interface registry for the agent-seat Wayland proxy.
//!
//! The proxy forwards every global whose interface is listed here and
//! silently filters everything else (vendor extensions an app can live
//! without). Denylisted interfaces are deliberately excluded because they
//! would let the proxied app reach outside its own window (session locks,
//! other clients' toplevels, clipboard snooping, input grabs, ...).

use wayland_backend::protocol::Interface;
use wayland_client::protocol::{
    wl_compositor::WlCompositor, wl_data_device_manager::WlDataDeviceManager, wl_output::WlOutput,
    wl_seat::WlSeat, wl_shm::WlShm, wl_subcompositor::WlSubcompositor,
};
use wayland_client::Proxy;

macro_rules! registry {
    ($($name:literal => $expr:expr),* $(,)?) => {
        /// Look up a known interface by its wire name.
        pub fn by_name(name: &str) -> Option<&'static Interface> {
            $(if name == $name { return Some($expr); })*
            None
        }
    };
}

registry! {
    // ---- core ----
    "wl_compositor" => WlCompositor::interface(),
    "wl_subcompositor" => WlSubcompositor::interface(),
    "wl_shm" => WlShm::interface(),
    "wl_seat" => WlSeat::interface(),
    "wl_output" => WlOutput::interface(),
    "wl_data_device_manager" => WlDataDeviceManager::interface(),

    // ---- stable ----
    // Kept in the registry so protocol objects can still be decoded in tests,
    // but deliberately not advertised by the agent seat (see `is_denied`).
    // The host must be able to read every frame it forwards. GPU dma-bufs are
    // frequently tiled or device-private, so a proxy cannot safely mmap them;
    // withholding this optional global makes toolkits use capturable wl_shm.
    "zwp_linux_dmabuf_v1" =>
        wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1::interface(),
    "wp_viewporter" =>
        wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter::interface(),
    "wp_presentation" =>
        wayland_protocols::wp::presentation_time::client::wp_presentation::WpPresentation::interface(),
    "xdg_wm_base" => wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase::interface(),

    // ---- staging ----
    "wp_fractional_scale_manager_v1" =>
        wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1::interface(),
    "wp_cursor_shape_manager_v1" =>
        wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1::interface(),
    "wp_content_type_manager_v1" =>
        wayland_protocols::wp::content_type::v1::client::wp_content_type_manager_v1::WpContentTypeManagerV1::interface(),
    "wp_single_pixel_buffer_manager_v1" =>
        wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1::interface(),
    "wp_tearing_control_manager_v1" =>
        wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_manager_v1::WpTearingControlManagerV1::interface(),
    "xdg_activation_v1" =>
        wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1::interface(),
    "xdg_dialog_manager_v1" =>
        wayland_protocols::xdg::dialog::v1::client::xdg_wm_dialog_v1::XdgWmDialogV1::interface(),
    "ext_idle_notifier_v1" =>
        wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1::interface(),
    "xdg_system_bell_v1" =>
        wayland_protocols::xdg::system_bell::v1::client::xdg_system_bell_v1::XdgSystemBellV1::interface(),

    // ---- unstable ----
    "zxdg_decoration_manager_v1" =>
        wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1::interface(),
    "zxdg_output_manager_v1" =>
        wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1::interface(),
    "zwp_idle_inhibit_manager_v1" =>
        wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1::interface(),
    "zwp_pointer_constraints_v1" =>
        wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1::interface(),
    "zwp_relative_pointer_manager_v1" =>
        wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1::interface(),
    "zwp_primary_selection_device_manager_v1" =>
        wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1::interface(),
    "zwp_text_input_manager_v3" =>
        wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3::interface(),
    "zwp_input_timestamps_manager_v1" =>
        wayland_protocols::wp::input_timestamps::zv1::client::zwp_input_timestamps_manager_v1::ZwpInputTimestampsManagerV1::interface(),
    "zwp_pointer_gestures_v1" =>
        wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1::ZwpPointerGesturesV1::interface(),
    "zwp_tablet_manager_v2" =>
        wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2::ZwpTabletManagerV2::interface(),
}

/// Interfaces that exist in wayland-protocols but must never be forwarded:
/// they let a client reach beyond its own surfaces. The proxy simply does not
/// advertise globals with these names.
pub fn is_denied(name: &str) -> bool {
    matches!(
        name,
        "zwp_linux_dmabuf_v1"
            | "ext_session_lock_manager_v1"
            | "wp_security_context_manager_v1"
            | "wp_drm_lease_device_v1"
            | "ext_transient_seat_manager_v1"
            | "zwp_input_method_manager_v2"
            | "zwp_keyboard_shortcuts_inhibit_manager_v1"
            | "zwp_xwayland_keyboard_grab_manager_v1"
            | "ext_data_control_manager_v1"
            | "ext_foreign_toplevel_list_v1"
            | "zxdg_exporter_v1"
            | "zxdg_exporter_v2"
            | "zxdg_importer_v1"
            | "zxdg_importer_v2"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_the_essential_globals() {
        for name in [
            "wl_compositor",
            "wl_shm",
            "wl_seat",
            "wl_output",
            "xdg_wm_base",
            "zwp_linux_dmabuf_v1",
            "wp_viewporter",
            "wp_fractional_scale_manager_v1",
            "xdg_activation_v1",
        ] {
            assert!(by_name(name).is_some(), "missing interface {name}");
            assert_eq!(by_name(name).unwrap().name, name);
        }
    }

    #[test]
    fn privileged_interfaces_are_denied() {
        assert!(is_denied("zwp_linux_dmabuf_v1"));
        assert!(is_denied("ext_session_lock_manager_v1"));
        assert!(is_denied("ext_foreign_toplevel_list_v1"));
        assert!(!is_denied("wl_compositor"));
    }
}
