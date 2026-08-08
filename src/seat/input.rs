//! Input injection for the agent seat.
//!
//! Synthesizes `wl_pointer` and `wl_keyboard` events and delivers them
//! straight to the proxied app's own input objects. Because the events go to
//! the app's connection
//! (not the compositor's focus routing), the agent can click and type in its
//! app even while the user works in a different foreground window.

use smallvec::SmallVec;
use wayland_backend::protocol::{Argument, Message};
use wayland_backend::server::ObjectId as SId;

use super::proxy::Conn;

// wl_pointer event opcodes.
const PTR_ENTER: u16 = 0;
const PTR_LEAVE: u16 = 1;
const PTR_MOTION: u16 = 2;
const PTR_BUTTON: u16 = 3;
const PTR_AXIS: u16 = 4;
const PTR_FRAME: u16 = 5;
const PTR_AXIS_DISCRETE: u16 = 8;

// wl_keyboard event opcodes.
const KBD_ENTER: u16 = 1;
const KBD_LEAVE: u16 = 2;
const KBD_KEY: u16 = 3;

// Linux input-event button codes.
pub const BTN_LEFT: u32 = 0x110;

// wl_keyboard key states.
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;

/// Convert a logical coordinate to wl fixed-point (24.8).
fn to_fixed(v: f64) -> i32 {
    (v * 256.0).round() as i32
}

fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

/// Send a single event to the app. Assumes the conn lock is held by the
/// caller (send_event itself only takes the server's internal lock).
fn send(
    conn: &Conn,
    sender: &SId,
    opcode: u16,
    args: SmallVec<[Argument<SId, i32>; 4]>,
) -> Result<(), String> {
    let msg = Message {
        sender_id: sender.clone(),
        opcode,
        args,
    };
    conn.server_handle
        .send_event(msg)
        .map_err(|error| format!("could not deliver input event opcode {opcode}: {error}"))
}

fn next_serial(conn: &mut Conn) -> u32 {
    let s = conn.next_serial;
    conn.next_serial = conn.next_serial.wrapping_add(1);
    s
}

/// Pick the surface to inject into: the keyboard-focused surface if known,
/// otherwise the most recently committed surface.
fn target_surface(conn: &Conn) -> Option<SId> {
    if let Some(s) = conn.focused_surface.clone() {
        return Some(s);
    }
    conn.surfaces
        .iter()
        .max_by_key(|(_, st)| st.commit_count)
        .map(|(id, _)| id.clone())
}

/// Ensure the injected pointer has focus on `surface` (sending leave/enter).
fn ensure_pointer_focus(conn: &mut Conn, surface: &SId, x: f64, y: f64) -> Result<(), String> {
    let pointer = conn
        .pointer_obj
        .clone()
        .ok_or_else(|| "the app has not created a wl_pointer yet".to_string())?;
    if conn.pointer_focus.as_ref() == Some(surface) {
        return Ok(());
    }
    // Leave the previous surface, if any.
    if let Some(prev) = conn.pointer_focus.clone() {
        let serial = next_serial(conn);
        let mut args = SmallVec::new();
        args.push(Argument::Uint(serial));
        args.push(Argument::Object(prev));
        send(conn, &pointer, PTR_LEAVE, args)?;
        send(conn, &pointer, PTR_FRAME, SmallVec::new())?;
    }
    // Enter the new surface at (x, y).
    let serial = next_serial(conn);
    let mut args = SmallVec::new();
    args.push(Argument::Uint(serial));
    args.push(Argument::Object(surface.clone()));
    args.push(Argument::Fixed(to_fixed(x)));
    args.push(Argument::Fixed(to_fixed(y)));
    send(conn, &pointer, PTR_ENTER, args)?;
    send(conn, &pointer, PTR_FRAME, SmallVec::new())?;
    conn.pointer_focus = Some(surface.clone());
    Ok(())
}

/// Ensure synthetic keyboard events are routed to the same app surface the
/// agent most recently clicked, rather than guessing among Electron's helper
/// and popup surfaces.
fn ensure_keyboard_focus(conn: &mut Conn, surface: &SId) -> Result<(), String> {
    let keyboard = conn
        .keyboard_obj
        .clone()
        .ok_or_else(|| "the app has not created a wl_keyboard yet".to_string())?;
    if conn.keyboard_focus.as_ref() == Some(surface) {
        return Ok(());
    }
    if let Some(previous) = conn.keyboard_focus.clone() {
        let mut leave = SmallVec::new();
        leave.push(Argument::Uint(next_serial(conn)));
        leave.push(Argument::Object(previous));
        send(conn, &keyboard, KBD_LEAVE, leave)?;
    }
    let mut enter = SmallVec::new();
    enter.push(Argument::Uint(next_serial(conn)));
    enter.push(Argument::Object(surface.clone()));
    enter.push(Argument::Array(Box::<Vec<u8>>::default()));
    send(conn, &keyboard, KBD_ENTER, enter)?;
    conn.keyboard_focus = Some(surface.clone());
    Ok(())
}

/// Click at surface-local (x, y). `button` is a Linux input code (BTN_*),
/// `count` is the number of clicks.
pub(crate) fn inject_click(
    conn: &mut Conn,
    x: f64,
    y: f64,
    button: u32,
    count: u32,
) -> Result<(), String> {
    let surface =
        target_surface(conn).ok_or_else(|| "the app has no surface to click in yet".to_string())?;
    let pointer = conn
        .pointer_obj
        .clone()
        .ok_or_else(|| "the app has not created a wl_pointer yet".to_string())?;

    ensure_pointer_focus(conn, &surface, x, y)?;
    // A real compositor routes subsequent keys to the clicked surface. Keep
    // the private seat's desired focus in step with that behavior.
    conn.focused_surface = Some(surface.clone());

    // Move to the point (in case focus was already there at a different spot).
    let mut motion = SmallVec::new();
    motion.push(Argument::Uint(now_ms()));
    motion.push(Argument::Fixed(to_fixed(x)));
    motion.push(Argument::Fixed(to_fixed(y)));
    send(conn, &pointer, PTR_MOTION, motion)?;
    send(conn, &pointer, PTR_FRAME, SmallVec::new())?;

    for _ in 0..count.max(1) {
        let serial = next_serial(conn);
        let t = now_ms();
        let mut press = SmallVec::new();
        press.push(Argument::Uint(serial));
        press.push(Argument::Uint(t));
        press.push(Argument::Uint(button));
        press.push(Argument::Uint(KEY_PRESSED));
        send(conn, &pointer, PTR_BUTTON, press)?;
        send(conn, &pointer, PTR_FRAME, SmallVec::new())?;

        // Hold the button briefly so toolkits register it as a real click.
        std::thread::sleep(std::time::Duration::from_millis(45));

        let serial = next_serial(conn);
        let t = now_ms();
        let mut release = SmallVec::new();
        release.push(Argument::Uint(serial));
        release.push(Argument::Uint(t));
        release.push(Argument::Uint(button));
        release.push(Argument::Uint(KEY_RELEASED));
        send(conn, &pointer, PTR_BUTTON, release)?;
        send(conn, &pointer, PTR_FRAME, SmallVec::new())?;

        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    Ok(())
}

/// Click while holding a portable modifier list (for example
/// `ctrl+shift`). Modifier key events and the matching wl_keyboard.modifiers
/// state are delivered to the same bound app as the pointer event.
pub(crate) fn inject_click_with_modifiers(
    conn: &mut Conn,
    x: f64,
    y: f64,
    button: u32,
    count: u32,
    modifiers: Option<&str>,
) -> Result<(), String> {
    let held = press_named_modifiers(conn, modifiers)?;
    let click_result = inject_click(conn, x, y, button, count);
    let release_result = release_named_modifiers(conn, &held);
    click_result.and(release_result)
}

/// Scroll at surface-local (x, y). `discrete_x`/`discrete_y` are notch counts
/// (positive = right/down).
pub(crate) fn inject_scroll(
    conn: &mut Conn,
    x: f64,
    y: f64,
    discrete_x: i32,
    discrete_y: i32,
) -> Result<(), String> {
    let surface = target_surface(conn)
        .ok_or_else(|| "the app has no surface to scroll in yet".to_string())?;
    let pointer = conn
        .pointer_obj
        .clone()
        .ok_or_else(|| "the app has not created a wl_pointer yet".to_string())?;

    ensure_pointer_focus(conn, &surface, x, y)?;

    let t = now_ms();
    if discrete_y != 0 {
        // axis value: one notch = 10.0 (wl_pointer axis units).
        let mut axis = SmallVec::new();
        axis.push(Argument::Uint(t));
        axis.push(Argument::Uint(0)); // 0 = vertical
        axis.push(Argument::Fixed(to_fixed(f64::from(discrete_y) * 10.0)));
        send(conn, &pointer, PTR_AXIS, axis)?;
        let mut disc = SmallVec::new();
        disc.push(Argument::Uint(0)); // vertical
        disc.push(Argument::Int(discrete_y));
        send(conn, &pointer, PTR_AXIS_DISCRETE, disc)?;
    }
    if discrete_x != 0 {
        let mut axis = SmallVec::new();
        axis.push(Argument::Uint(t));
        axis.push(Argument::Uint(1)); // 1 = horizontal
        axis.push(Argument::Fixed(to_fixed(f64::from(discrete_x) * 10.0)));
        send(conn, &pointer, PTR_AXIS, axis)?;
        let mut disc = SmallVec::new();
        disc.push(Argument::Uint(1)); // horizontal
        disc.push(Argument::Int(discrete_x));
        send(conn, &pointer, PTR_AXIS_DISCRETE, disc)?;
    }
    send(conn, &pointer, PTR_FRAME, SmallVec::new())?;
    Ok(())
}

/// Drag inside the bound surface without touching the user's physical
/// pointer. Coordinates are surface-local, matching the captured frame.
pub(crate) fn inject_drag(
    conn: &mut Conn,
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
) -> Result<(), String> {
    let surface =
        target_surface(conn).ok_or_else(|| "the app has no surface to drag in yet".to_string())?;
    let pointer = conn
        .pointer_obj
        .clone()
        .ok_or_else(|| "the app has not created a wl_pointer yet".to_string())?;
    ensure_pointer_focus(conn, &surface, from_x, from_y)?;

    let mut start = SmallVec::new();
    start.push(Argument::Uint(now_ms()));
    start.push(Argument::Fixed(to_fixed(from_x)));
    start.push(Argument::Fixed(to_fixed(from_y)));
    send(conn, &pointer, PTR_MOTION, start)?;

    let mut press = SmallVec::new();
    press.push(Argument::Uint(next_serial(conn)));
    press.push(Argument::Uint(now_ms()));
    press.push(Argument::Uint(BTN_LEFT));
    press.push(Argument::Uint(KEY_PRESSED));
    send(conn, &pointer, PTR_BUTTON, press)?;
    send(conn, &pointer, PTR_FRAME, SmallVec::new())?;

    const STEPS: i32 = 24;
    for step in 1..=STEPS {
        let t = f64::from(step) / f64::from(STEPS);
        let mut motion = SmallVec::new();
        motion.push(Argument::Uint(now_ms()));
        motion.push(Argument::Fixed(to_fixed(from_x + (to_x - from_x) * t)));
        motion.push(Argument::Fixed(to_fixed(from_y + (to_y - from_y) * t)));
        send(conn, &pointer, PTR_MOTION, motion)?;
        send(conn, &pointer, PTR_FRAME, SmallVec::new())?;
        std::thread::sleep(std::time::Duration::from_millis(8));
    }

    let mut release = SmallVec::new();
    release.push(Argument::Uint(next_serial(conn)));
    release.push(Argument::Uint(now_ms()));
    release.push(Argument::Uint(BTN_LEFT));
    release.push(Argument::Uint(KEY_RELEASED));
    send(conn, &pointer, PTR_BUTTON, release)?;
    send(conn, &pointer, PTR_FRAME, SmallVec::new())?;
    Ok(())
}

/// Press or release a single keycode on the injected keyboard.
pub(crate) fn inject_key_raw(conn: &mut Conn, keycode: u32, pressed: bool) -> Result<(), String> {
    let keyboard = conn
        .keyboard_obj
        .clone()
        .ok_or_else(|| "the app has not created a wl_keyboard yet".to_string())?;
    let surface = target_surface(conn)
        .ok_or_else(|| "the app has no surface to receive keyboard input".to_string())?;
    ensure_keyboard_focus(conn, &surface)?;
    let serial = next_serial(conn);
    // wl_keyboard.key keycodes are evdev keycodes; xkbcommon resolves symbols
    // against the keymap's internal numbering which is evdev+8, so translate
    // back to the evdev keycode the app expects.
    let evdev_keycode = keycode.saturating_sub(8);
    let mut args = SmallVec::new();
    args.push(Argument::Uint(serial));
    args.push(Argument::Uint(now_ms()));
    args.push(Argument::Uint(evdev_keycode));
    args.push(Argument::Uint(if pressed {
        KEY_PRESSED
    } else {
        KEY_RELEASED
    }));
    send(conn, &keyboard, KBD_KEY, args)
}

/// Send a wl_keyboard.modifiers event (opcode 4). GTK/text widgets rely on
/// this event (not just the Shift key event) to track modifier state.
fn send_modifiers(conn: &mut Conn, mods_depressed: u32) -> Result<(), String> {
    let keyboard = conn
        .keyboard_obj
        .clone()
        .ok_or_else(|| "the app has not created a wl_keyboard yet".to_string())?;
    let serial = next_serial(conn);
    let mut args = SmallVec::new();
    args.push(Argument::Uint(serial));
    args.push(Argument::Uint(mods_depressed)); // mods_depressed
    args.push(Argument::Uint(0)); // mods_latched
    args.push(Argument::Uint(0)); // mods_locked
    args.push(Argument::Uint(0)); // group
    send(conn, &keyboard, 4 /* modifiers */, args)
}

#[derive(Clone, Copy)]
struct HeldModifier {
    keycode: u32,
    mask: u32,
}

fn modifier_definition(name: &str) -> Option<(&'static str, &'static str)> {
    match name.trim().to_ascii_lowercase().as_str() {
        "shift" => Some(("shift", "Shift")),
        "ctrl" | "control" => Some(("ctrl", "Control")),
        "alt" | "option" => Some(("alt", "Mod1")),
        "super" | "cmd" | "command" | "meta" => Some(("super", "Mod4")),
        _ => None,
    }
}

fn modifier_mask(keymap: &xkbcommon::xkb::Keymap, xkb_name: &str) -> Result<u32, String> {
    let index = keymap.mod_get_index(&xkb_name);
    if index == u32::MAX {
        return Err(format!("keyboard layout has no '{xkb_name}' modifier"));
    }
    1u32.checked_shl(index)
        .ok_or_else(|| format!("invalid modifier index {index} for '{xkb_name}'"))
}

fn parse_keymap(keymap_text: &str) -> Result<xkbcommon::xkb::Keymap, String> {
    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    xkbcommon::xkb::Keymap::new_from_string(
        &context,
        keymap_text.to_string(),
        xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| "failed to parse the app's keymap".to_string())
}

fn press_named_modifiers(
    conn: &mut Conn,
    modifiers: Option<&str>,
) -> Result<Vec<HeldModifier>, String> {
    let names: Vec<&str> = modifiers
        .unwrap_or_default()
        .split('+')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }

    let keymap_text = read_keymap_text(conn)?;
    let keymap = parse_keymap(&keymap_text)?;
    let mut held = Vec::new();
    let mut depressed = 0u32;
    for name in names {
        let Some((key_name, xkb_name)) = modifier_definition(name) else {
            let _ = release_named_modifiers(conn, &held);
            return Err(format!("unknown modifier '{name}'"));
        };
        let keysym = crate::keymap::keysym_for_key(key_name)
            .ok_or_else(|| format!("unknown modifier key '{name}'"))?;
        let Some(keycode) = find_keycode_for_keysym(&keymap_text, keysym)? else {
            let _ = release_named_modifiers(conn, &held);
            return Err(format!("keyboard layout has no keycode for '{name}'"));
        };
        let mask = modifier_mask(&keymap, xkb_name)?;
        inject_key_raw(conn, keycode, true)?;
        depressed |= mask;
        send_modifiers(conn, depressed)?;
        held.push(HeldModifier { keycode, mask });
    }
    Ok(held)
}

fn release_named_modifiers(conn: &mut Conn, held: &[HeldModifier]) -> Result<(), String> {
    let mut depressed = held.iter().fold(0u32, |mask, item| mask | item.mask);
    for item in held.iter().rev() {
        inject_key_raw(conn, item.keycode, false)?;
        depressed &= !item.mask;
        send_modifiers(conn, depressed)?;
    }
    Ok(())
}

/// Press a key or combination inside the bound app (for example
/// `ctrl+shift+p`).
pub(crate) fn inject_key_combo(conn: &mut Conn, combination: &str) -> Result<(), String> {
    let parts: Vec<&str> = combination
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let Some((last, modifiers)) = parts.split_last() else {
        return Err("empty key".to_string());
    };
    let modifier_text = modifiers.join("+");
    let held = press_named_modifiers(conn, Some(&modifier_text))?;

    let keymap_text = read_keymap_text(conn)?;
    let key_result = (|| {
        let keysym =
            crate::keymap::keysym_for_key(last).ok_or_else(|| format!("unknown key '{last}'"))?;
        let keycode = find_keycode_for_keysym(&keymap_text, keysym)?
            .ok_or_else(|| format!("keyboard layout has no keycode for '{last}'"))?;
        inject_key_raw(conn, keycode, true)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        inject_key_raw(conn, keycode, false)
    })();
    let release_result = release_named_modifiers(conn, &held);
    key_result.and(release_result)
}

// ------------------------------------------------------------------
// Text injection (keysym -> keycode via the app's own keymap)
// ------------------------------------------------------------------

const KEYSYM_SHIFT_L: u32 = 0xffe1;
const KEYSYM_ENTER: u32 = 0xff0d;
const KEYSYM_TAB: u32 = 0xff09;
const KEYSYM_BACKSPACE: u32 = 0xff08;
const KEYSYM_ESCAPE: u32 = 0xff1b;

/// Read the keymap text the compositor sent (wl_keyboard.keymap fd).
fn read_keymap_text(conn: &Conn) -> Result<String, String> {
    let fd = conn.keymap_fd.as_ref().ok_or_else(|| {
        "no keymap captured yet (app has not received wl_keyboard.keymap)".to_string()
    })?;
    let size = conn.keymap_size as usize;
    use std::os::fd::AsRawFd;
    // SAFETY: keymap_fd is an owned descriptor received with keymap_size from
    // the compositor. The returned mapping is checked before it is read.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(format!(
            "mmap of the keymap fd failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: mapped is a successful read-only mapping of exactly size bytes
    // and remains mapped for the lifetime of this temporary slice.
    let bytes = unsafe { std::slice::from_raw_parts(mapped as *const u8, size) };
    // The keymap buffer may be padded with NULs; stop at the first NUL.
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    // SAFETY: mapped came from the successful mmap above and is released once
    // with the same size after all reads are complete.
    unsafe {
        libc::munmap(mapped, size);
    }
    Ok(text)
}

/// Resolved keycode for a character, plus whether Shift must be held.
struct CharResolution {
    keycode: u32,
    needs_shift: bool,
}

/// Outcome of resolving a text run against the keymap.
struct TextResolution {
    shift_keycode: Option<u32>,
    /// Modifier mask for Shift (bit set in mods_depressed when Shift is held).
    shift_mask: u32,
    resolutions: Vec<Option<CharResolution>>,
}

/// Build a keymap and find, for each character, the keycode that produces it
/// (checking level 0 then level 1 = Shift). Also locate the Shift keycode and
/// the Shift modifier mask (for wl_keyboard.modifiers events).
fn resolve_text(keymap_text: &str, text: &str) -> Result<TextResolution, String> {
    let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_string(
        &ctx,
        keymap_text.to_string(),
        xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| "failed to parse the app's keymap".to_string())?;

    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();

    // Shift modifier mask (for wl_keyboard.modifiers). mod_get_index returns
    // XKB_MOD_INVALID (u32::MAX) when the modifier is absent.
    let shift_idx = keymap.mod_get_index(&"Shift");
    let shift_mask = if shift_idx == u32::MAX {
        1u32
    } else {
        1u32 << shift_idx
    };

    // Find the Shift_L keycode.
    let mut shift_keycode: Option<u32> = None;
    'find_shift: for kc in min..=max {
        let key = xkbcommon::xkb::Keycode::from(kc);
        for sym in keymap.key_get_syms_by_level(key, 0, 0) {
            if sym.raw() == KEYSYM_SHIFT_L {
                shift_keycode = Some(kc);
                break 'find_shift;
            }
        }
    }

    let mut resolutions = Vec::new();
    for ch in text.chars() {
        let target = xkbcommon::xkb::utf32_to_keysym(ch as u32);
        let mut found: Option<CharResolution> = None;
        'search: for kc in min..=max {
            let key = xkbcommon::xkb::Keycode::from(kc);
            // Level 0 (no shift).
            for sym in keymap.key_get_syms_by_level(key, 0, 0) {
                if *sym == target {
                    found = Some(CharResolution {
                        keycode: kc,
                        needs_shift: false,
                    });
                    break 'search;
                }
            }
            // Level 1 (shift).
            for sym in keymap.key_get_syms_by_level(key, 0, 1) {
                if *sym == target {
                    found = Some(CharResolution {
                        keycode: kc,
                        needs_shift: true,
                    });
                    break 'search;
                }
            }
        }
        resolutions.push(found);
    }
    if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
        eprintln!(
            "seat KEYMAP min={min} max={max} shift={shift_keycode:?} mask={shift_mask:#x} text={text:?}"
        );
        for (ch, res) in text.chars().zip(resolutions.iter()) {
            eprintln!(
                "seat RESOLVE '{ch}' -> {:?}",
                res.as_ref().map(|r| (r.keycode, r.needs_shift))
            );
        }
    }
    Ok(TextResolution {
        shift_keycode,
        shift_mask,
        resolutions,
    })
}

/// Type a string into the app. Special ASCII control characters are mapped to
/// their keysyms; other characters are resolved through the app's keymap.
pub(crate) fn inject_text(conn: &mut Conn, text: &str) -> Result<(), String> {
    // Native Wayland editors (including Chromium/Electron) expose
    // text-input-v3 for committed text. Prefer it when enabled: unlike
    // synthesizing one key per character, this is layout-independent and is
    // the compositor-standard path for Unicode input.
    if conn.text_input_enabled {
        if let Some(text_input) = conn.text_input_obj.clone() {
            let committed = std::ffi::CString::new(text)
                .map_err(|_| "text input cannot contain a NUL character".to_string())?;
            let mut commit = SmallVec::new();
            commit.push(Argument::Str(Some(Box::new(committed))));
            send(conn, &text_input, 3 /* commit_string */, commit)?;
            let mut done = SmallVec::new();
            done.push(Argument::Uint(conn.text_input_commit_serial));
            send(conn, &text_input, 5 /* done */, done)?;
            return Ok(());
        }
    }

    let keymap_text = read_keymap_text(conn)?;

    // Split into runs of plain characters vs. special keys so we resolve the
    // keymap once for the plain characters.
    let mut plain = String::new();
    enum Item {
        Plain(String),
        Special(u32),
    }
    let mut items: Vec<Item> = Vec::new();
    for ch in text.chars() {
        let special = match ch {
            '\n' => Some(KEYSYM_ENTER),
            '\t' => Some(KEYSYM_TAB),
            '\u{08}' => Some(KEYSYM_BACKSPACE),
            '\u{1b}' => Some(KEYSYM_ESCAPE),
            _ => None,
        };
        if let Some(sym) = special {
            if !plain.is_empty() {
                items.push(Item::Plain(std::mem::take(&mut plain)));
            }
            items.push(Item::Special(sym));
        } else {
            plain.push(ch);
        }
    }
    if !plain.is_empty() {
        items.push(Item::Plain(plain));
    }

    for item in items {
        match item {
            Item::Special(sym) => {
                let keycode = find_keycode_for_keysym(&keymap_text, sym)?
                    .ok_or_else(|| format!("no keycode for special keysym {sym:#x}"))?;
                inject_key_raw(conn, keycode, true)?;
                inject_key_raw(conn, keycode, false)?;
            }
            Item::Plain(run) => {
                let resolution = resolve_text(&keymap_text, &run)?;
                let shift_mask = resolution.shift_mask;
                for (ch, res) in run.chars().zip(resolution.resolutions) {
                    let Some(res) = res else {
                        return Err(format!(
                            "character '{ch}' has no keycode on this keyboard layout"
                        ));
                    };
                    if res.needs_shift {
                        let shift = resolution.shift_keycode.ok_or_else(|| {
                            "layout needs Shift but no Shift keycode was found".to_string()
                        })?;
                        // Press Shift and announce it via a modifiers event; GTK
                        // text widgets key off the modifiers event for state.
                        inject_key_raw(conn, shift, true)?;
                        send_modifiers(conn, shift_mask)?;
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    inject_key_raw(conn, res.keycode, true)?;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    inject_key_raw(conn, res.keycode, false)?;
                    if res.needs_shift {
                        let shift = resolution.shift_keycode.unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        inject_key_raw(conn, shift, false)?;
                        send_modifiers(conn, 0)?;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    }
    Ok(())
}

/// Locate the keycode that produces a given keysym at level 0.
fn find_keycode_for_keysym(keymap_text: &str, keysym: u32) -> Result<Option<u32>, String> {
    let keymap = parse_keymap(keymap_text)?;
    let target = xkbcommon::xkb::Keysym::from(keysym);
    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();
    for kc in min..=max {
        let key = xkbcommon::xkb::Keycode::from(kc);
        for sym in keymap.key_get_syms_by_level(key, 0, 0) {
            if *sym == target {
                return Ok(Some(kc));
            }
        }
    }
    Ok(None)
}
