# Using `agent-seat-linux` from other languages

The crate's high-level boundary is deliberately small so any language with a
Rust FFI layer can wrap it without inheriting a host-app data model:

1. Convert your tool call into a `LaunchConfig`.
2. `ComputerUse::launch` returns one `ControlledApp`.
3. Route screenshot / click / scroll / drag / key / text tools to that handle.
4. Drop `ControlledApp` to stop the app; drop `ComputerUse` to close the seat,
   bridges, threads, sockets, and credentials.

No bindings are shipped yet; the recommended pattern is a thin `extern "C"`
shim over the high-level API. Keep the shim to the five verbs above and pass
opaque handles across the boundary.

## Recommended C-ABI shim (sketch)

```rust
// ffi.rs — build as a cdylib
use agent_seat_linux::{ComputerUse, LaunchConfig, PointerButton};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

pub struct Handle { seat: ComputerUse }          // opaque to callers
pub struct AppHandle { app: agent_seat_linux::ControlledApp }

#[no_mangle] pub extern "C" fn asl_new() -> *mut Handle {
    match ComputerUse::new() { Ok(s) => Box::into_raw(Box::new(Handle{seat:s})), Err(_) => std::ptr::null_mut() }
}

#[no_mangle] pub extern "C" fn asl_launch(h: *mut Handle, program: *const c_char) -> *mut AppHandle {
    let h = unsafe { &*h };
    let prog = unsafe { CStr::from_ptr(program) };
    match h.seat.launch(LaunchConfig::new(prog.to_string_lossy().into_owned())) {
        Ok(app) => Box::into_raw(Box::new(AppHandle{app})),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle] pub extern "C" fn asl_click(a: *mut AppHandle, x: f64, y: f64) -> i32 {
    let a = unsafe { &*a };
    match a.app.click(x, y, PointerButton::Left, 1) { Ok(()) => 0, Err(_) => -1 }
}

#[no_mangle] pub extern "C" fn asl_type_text(a: *mut AppHandle, text: *const c_char) -> i32 {
    let a = unsafe { &*a };
    let t = unsafe { CStr::from_ptr(text) };
    match a.app.type_text(&t.to_string_lossy()) { Ok(()) => 0, Err(_) => -1 }
}

#[no_mangle] pub extern "C" fn asl_free_app(a: *mut AppHandle) { if !a.is_null() { unsafe { drop(Box::from_raw(a)); } } }
#[no_mangle] pub extern "C" fn asl_free(h: *mut Handle)      { if !h.is_null() { unsafe { drop(Box::from_raw(h)); } } }
```

Corresponding C header (abridged):

```c
typedef struct Handle Handle;
typedef struct AppHandle AppHandle;
Handle    *asl_new(void);
AppHandle *asl_launch(Handle *h, const char *program);
int        asl_click(AppHandle *a, double x, double y);
int        asl_type_text(AppHandle *a, const char *text);
void       asl_free_app(AppHandle *a);
void       asl_free(Handle *h);
```

## Conventions for a good binding

- **Errors:** return an integer/`Result` code and expose `asl_last_error()` that
  returns a thread-local `CString` message; never return Rust `String`/`&str`.
- **Ownership:** every `*_new`/`launch` has a matching `*_free`; document that
  dropping the app handle stops the controlled application.
- **Frames:** expose `capture` as `(width, height, *const u8 rgb, len)` plus a
  `free_frame`, or write PNG to a caller-provided path to avoid crossing with
  large buffers.
- **Threading:** the seat runs background threads; call from a single owner
  thread or guard the handles with a mutex in the binding.

Generate the header automatically with [`cbindgen`](https://github.com/mozilla/cbindgen)
if you prefer, but keep the exported surface limited to the verbs above.
