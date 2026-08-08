//! Generic man-in-the-middle Wayland proxy for one agent-launched app.
//!
//! The app connects to a private agent-seat socket; the proxy opens a paired
//! connection to the real compositor and forwards every message in both
//! directions, remapping object IDs between the two ID spaces. For the app,
//! the library *is* the application's display server, which makes app-scoped
//! computer use possible on Wayland:
//!
//! - frames the app renders pass through `wl_surface.attach`/`commit` and can
//!   be read back regardless of window visibility (capture phase),
//! - input events can be synthesized straight into the app's connection
//!   regardless of focus (injection phase),
//! - everything else is forwarded untouched, so the window behaves like any
//!   other window on the user's desktop and remains usable by hand.
//!
//! Unknown or denylisted globals are simply not advertised, so the proxied
//! app can never reach beyond its own surfaces (session locks, foreign
//! toplevels, clipboard snooping, input grabs, ...).
//!
//! fd ownership rule: message structs own their fds (`OwnedFd`); the backends
//! only borrow raw fds during the synchronous send, so the owned message is
//! kept alive across every send call and dropped afterwards.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use smallvec::{smallvec, SmallVec};
use wayland_backend::client::{Backend as CBackend, ObjectId as CId};
use wayland_backend::protocol::{Argument, Interface, Message};
use wayland_backend::server::{
    Backend as SBackend, ClientData, ClientId, GlobalHandler, GlobalId, Handle, ObjectData,
    ObjectId as SId,
};
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::Proxy as _;

use super::interfaces;

/// Roles tag proxied objects so the forwarder can apply protocol-specific
/// bookkeeping (capture, injection) on top of generic forwarding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Display,
    Registry,
    Callback,
    Surface,
    ShmPool,
    ShmBuffer,
    DmaBufParams,
    /// Reserved for GPU-buffer (dmabuf) capture readback.
    #[allow(dead_code)]
    DmaBufBuffer,
    Seat,
    Pointer,
    Keyboard,
    TextInput,
    Generic,
}

fn role_for_interface(iface: &Interface) -> Role {
    match iface.name {
        "wl_surface" => Role::Surface,
        "wl_shm_pool" => Role::ShmPool,
        "wl_buffer" => Role::ShmBuffer,
        "zwp_linux_buffer_params_v1" => Role::DmaBufParams,
        "wl_seat" => Role::Seat,
        "wl_pointer" => Role::Pointer,
        "wl_keyboard" => Role::Keyboard,
        "zwp_text_input_v3" => Role::TextInput,
        "wl_callback" => Role::Callback,
        "wl_registry" => Role::Registry,
        "wl_display" => Role::Display,
        _ => Role::Generic,
    }
}

/// A wl_shm pool created by the app (capture readback source).
#[derive(Debug)]
pub(crate) struct ShmPool {
    pub fd: OwnedFd,
    pub size: i32,
}

/// Metadata of a buffer the app may attach to its surfaces.
#[derive(Debug)]
pub(crate) enum BufferKind {
    Shm {
        fd: OwnedFd,
        pool_size: i32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    },
    DmaBuf {
        #[allow(dead_code)]
        width: i32,
        #[allow(dead_code)]
        height: i32,
        #[allow(dead_code)]
        format: u32,
        #[allow(dead_code)]
        modifier: u64,
        #[allow(dead_code)]
        planes: Vec<DmaBufPlane>,
        #[allow(dead_code)]
        fds: Vec<OwnedFd>,
    },
}

/// Readback state retained by a surface after `wl_surface.attach`. Wayland
/// clients may legally destroy the wl_buffer and wl_shm_pool protocol objects
/// immediately after attaching; the compositor still owns the underlying
/// storage, so the proxy must retain its own fd as well.
#[derive(Debug)]
pub(crate) struct ShmFrame {
    pub fd: OwnedFd,
    pub pool_size: i32,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: u32,
}

fn retain_shm_frame(buffer: &BufferKind) -> Option<ShmFrame> {
    match buffer {
        BufferKind::Shm {
            fd,
            pool_size,
            offset,
            width,
            height,
            stride,
            format,
        } => Some(ShmFrame {
            fd: dup_fd(fd.as_fd()),
            pool_size: *pool_size,
            offset: *offset,
            width: *width,
            height: *height,
            stride: *stride,
            format: *format,
        }),
        BufferKind::DmaBuf { .. } => None,
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // GPU-buffer (dmabuf) capture readback metadata.
pub(crate) struct DmaBufPlane {
    pub fd_index: usize,
    pub offset: u32,
    pub stride: u32,
}

/// Per-surface state for capture.
#[derive(Debug, Default)]
pub(crate) struct SurfaceState {
    /// Readable buffer attached most recently (None = unattached/non-shm).
    pub attached: Option<ShmFrame>,
    /// Incremented on every `commit`; capture keys off this.
    pub commit_count: u64,
}

/// Shared state of one proxied connection. Locked only by the proxy thread
/// (the Mutex exists because ObjectData impls must be Sync).
pub(crate) struct Conn {
    pub self_weak: Weak<Mutex<Conn>>,
    pub server_handle: Handle,
    pub client_id: Option<ClientId>,
    pub upstream: CBackend,
    pub upstream_registry: Option<CId>,

    pub s2u: HashMap<SId, CId>,
    pub u2s: HashMap<CId, SId>,
    /// Upstream protocol id -> server object (for wl_display.delete_id).
    pub u2s_proto: HashMap<u32, SId>,

    /// Upstream global name -> (advertised server global, interface, version).
    pub globals: HashMap<u32, (GlobalId, &'static Interface, u32)>,
    pub server_global_to_name: HashMap<GlobalId, u32>,

    pub shm_pools: HashMap<SId, ShmPool>,
    pub buffers: HashMap<SId, BufferKind>,
    pub surfaces: HashMap<SId, SurfaceState>,
    /// Pending dmabuf planes accumulated via params.add before create:
    /// (planes, fds, modifier).
    pub dmabuf_pending: HashMap<SId, (Vec<DmaBufPlane>, Vec<OwnedFd>, u64)>,

    /// Keymap forwarded to the app (kept for injection).
    pub keymap_fd: Option<OwnedFd>,
    pub keymap_size: u32,
    /// Objects used for injection.
    pub pointer_obj: Option<SId>,
    pub keyboard_obj: Option<SId>,
    pub text_input_obj: Option<SId>,
    pub text_input_pending_enabled: bool,
    pub text_input_enabled: bool,
    pub text_input_commit_serial: u32,
    pub focused_surface: Option<SId>,
    pub next_serial: u32,
    /// Surface the injected pointer currently has focus on (for enter/leave).
    pub pointer_focus: Option<SId>,
    /// Surface the injected keyboard currently has focus on.
    pub keyboard_focus: Option<SId>,

    /// Actions queued by tool threads, drained by the proxy loop.
    pub actions: Vec<super::Action>,

    /// Set when either side hung up.
    pub dead: bool,
}

pub(crate) fn dup_fd(fd: std::os::fd::BorrowedFd<'_>) -> OwnedFd {
    fd.try_clone_to_owned().expect("dup fd")
}

/// True when a client-backend error is just "nothing to read right now".
pub(crate) fn is_would_block(e: &wayland_backend::client::WaylandError) -> bool {
    matches!(
        e,
        wayland_backend::client::WaylandError::Io(io)
            if io.kind() == std::io::ErrorKind::WouldBlock
    )
}

impl Conn {
    fn link(&mut self, server: SId, upstream: CId) {
        self.u2s_proto
            .insert(upstream.protocol_id(), server.clone());
        self.s2u.insert(server.clone(), upstream.clone());
        self.u2s.insert(upstream, server);
    }

    fn unlink_server(&mut self, server: &SId) {
        if let Some(upstream) = self.s2u.remove(server) {
            self.u2s.remove(&upstream);
            self.u2s_proto.remove(&upstream.protocol_id());
        }
        self.shm_pools.remove(server);
        self.buffers.remove(server);
        self.surfaces.remove(server);
        self.dmabuf_pending.remove(server);
        if self.pointer_obj.as_ref() == Some(server) {
            self.pointer_obj = None;
        }
        if self.keyboard_obj.as_ref() == Some(server) {
            self.keyboard_obj = None;
        }
        if self.text_input_obj.as_ref() == Some(server) {
            self.text_input_obj = None;
            self.text_input_pending_enabled = false;
            self.text_input_enabled = false;
            self.text_input_commit_serial = 0;
        }
        if self.focused_surface.as_ref() == Some(server) {
            self.focused_surface = None;
        }
        if self.pointer_focus.as_ref() == Some(server) {
            self.pointer_focus = None;
        }
        if self.keyboard_focus.as_ref() == Some(server) {
            self.keyboard_focus = None;
        }
    }

    fn unlink_upstream(&mut self, upstream: &CId) {
        if let Some(server) = self.u2s.remove(upstream) {
            self.s2u.remove(&server);
        }
        self.u2s_proto.remove(&upstream.protocol_id());
    }

    fn map_object_upstream(&self, id: &SId) -> CId {
        if id.is_null() {
            CId::null()
        } else {
            self.s2u.get(id).cloned().unwrap_or_else(CId::null)
        }
    }

    fn map_object_server(&self, id: &CId) -> SId {
        if id.is_null() {
            SId::null()
        } else {
            self.u2s.get(id).cloned().unwrap_or_else(SId::null)
        }
    }

    /// Record protocol-specific state for a request before it is forwarded.
    fn track_request(&mut self, role: Role, msg: &Message<SId, OwnedFd>) {
        match role {
            Role::Surface => {
                // wl_surface.attach(buffer, x, y)
                if msg.opcode == 1 {
                    if let Some(Argument::Object(buf)) = msg.args.first() {
                        let attached = (!buf.is_null())
                            .then(|| self.buffers.get(buf).and_then(retain_shm_frame))
                            .flatten();
                        let entry = self.surfaces.entry(msg.sender_id.clone()).or_default();
                        entry.attached = attached;
                    }
                }
                // wl_surface.commit
                if msg.opcode == 6 {
                    let entry = self.surfaces.entry(msg.sender_id.clone()).or_default();
                    entry.commit_count += 1;
                }
            }
            Role::ShmPool => {
                // wl_shm_pool: create_buffer=0, destroy=1, resize=2.
                if msg.opcode == 2 {
                    if let Some(Argument::Int(size)) = msg.args.first() {
                        if let Some(pool) = self.shm_pools.get_mut(&msg.sender_id) {
                            pool.size = *size;
                        }
                    }
                }
            }
            Role::TextInput => match msg.opcode {
                1 => self.text_input_pending_enabled = true,
                2 => self.text_input_pending_enabled = false,
                7 => {
                    self.text_input_commit_serial = self.text_input_commit_serial.wrapping_add(1);
                    self.text_input_enabled = self.text_input_pending_enabled;
                }
                _ => {}
            },
            // zwp_linux_buffer_params_v1.add(fd, plane_idx, offset, stride, mod_hi, mod_lo)
            Role::DmaBufParams if msg.opcode == 0 => {
                let mut fd: Option<&OwnedFd> = None;
                let mut vals = [0u32; 5];
                let mut idx = 0;
                for arg in &msg.args {
                    match arg {
                        Argument::Fd(f) => fd = Some(f),
                        Argument::Uint(v) if idx < 5 => {
                            vals[idx] = *v;
                            idx += 1;
                        }
                        _ => {}
                    }
                }
                if let Some(fd) = fd {
                    let entry = self
                        .dmabuf_pending
                        .entry(msg.sender_id.clone())
                        .or_insert_with(|| (Vec::new(), Vec::new(), 0));
                    entry.0.push(DmaBufPlane {
                        fd_index: entry.1.len(),
                        offset: vals[1],
                        stride: vals[2],
                    });
                    entry.1.push(dup_fd(fd.as_fd()));
                    entry.2 = ((vals[4] as u64) << 32) | vals[3] as u64;
                }
            }
            _ => {}
        }
    }

    /// Register newly created objects (request new_id) for capture tracking.
    fn track_created(&mut self, role: Role, new_server: &SId, msg: &Message<SId, OwnedFd>) {
        match role {
            // wl_shm.create_pool(new_id, fd, size)
            Role::ShmPool => {
                let mut fd = None;
                let mut size = 0;
                for arg in &msg.args {
                    match arg {
                        Argument::Fd(f) => fd = Some(dup_fd(f.as_fd())),
                        Argument::Int(v) => size = *v,
                        _ => {}
                    }
                }
                if let Some(fd) = fd {
                    self.shm_pools
                        .insert(new_server.clone(), ShmPool { fd, size });
                }
            }
            // wl_shm_pool.create_buffer(new_id, offset, w, h, stride, format)
            Role::ShmBuffer => {
                let mut ints = Vec::new();
                for arg in &msg.args {
                    match arg {
                        Argument::Int(v) => ints.push(*v),
                        Argument::Uint(v) => ints.push(*v as i32),
                        _ => {}
                    }
                }
                if ints.len() >= 5 {
                    if std::env::var_os("AGENT_SEAT_DEBUG").is_some() {
                        eprintln!(
                            "agent seat: shm buffer created {}x{} stride={} format={:#x}",
                            ints[1], ints[2], ints[3], ints[4]
                        );
                    }
                    if let Some(pool) = self.shm_pools.get(&msg.sender_id) {
                        self.buffers.insert(
                            new_server.clone(),
                            BufferKind::Shm {
                                fd: dup_fd(pool.fd.as_fd()),
                                pool_size: pool.size,
                                offset: ints[0],
                                width: ints[1],
                                height: ints[2],
                                stride: ints[3],
                                format: ints[4] as u32,
                            },
                        );
                    }
                }
            }
            // zwp_linux_buffer_params_v1.create_immediate(new_id, w, h, format, flags)
            Role::DmaBufBuffer => {
                if let Some((planes, fds, modifier)) = self.dmabuf_pending.remove(&msg.sender_id) {
                    let mut ints = Vec::new();
                    for arg in &msg.args {
                        match arg {
                            Argument::Int(v) => ints.push(*v),
                            Argument::Uint(v) => ints.push(*v as i32),
                            _ => {}
                        }
                    }
                    if ints.len() >= 3 {
                        self.buffers.insert(
                            new_server.clone(),
                            BufferKind::DmaBuf {
                                width: ints[0],
                                height: ints[1],
                                format: ints[2] as u32,
                                modifier,
                                planes,
                                fds,
                            },
                        );
                    }
                }
            }
            Role::Pointer => self.pointer_obj = Some(new_server.clone()),
            Role::Keyboard => self.keyboard_obj = Some(new_server.clone()),
            Role::TextInput => self.text_input_obj = Some(new_server.clone()),
            Role::Surface => {
                if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
                    eprintln!("seat SURFACE created");
                }
                self.surfaces.entry(new_server.clone()).or_default();
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------
// Server side: forwards app requests upstream
// ------------------------------------------------------------------

pub(crate) struct SObj {
    pub conn: Weak<Mutex<Conn>>,
    pub role: Role,
}

/// Dispatch state for the per-app server backend (unused: all state lives in
/// the Conn reached through each object's weak reference).
pub(crate) struct ServerState;

impl ObjectData<ServerState> for SObj {
    fn request(
        self: Arc<Self>,
        _handle: &Handle,
        _state: &mut ServerState,
        _client_id: ClientId,
        msg: Message<SId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ServerState>>> {
        let conn = self.conn.upgrade()?;
        let mut conn = conn.lock().unwrap();
        forward_request(&mut conn, self.role, msg)
    }

    fn destroyed(
        self: Arc<Self>,
        _handle: &Handle,
        _state: &mut ServerState,
        _client_id: ClientId,
        object_id: SId,
    ) {
        let Some(conn) = self.conn.upgrade() else {
            return;
        };
        // Phase 1: update the maps under the lock and collect what is needed.
        // The lock MUST be released before destroying the upstream twin:
        // destroy_object fires the upstream-side `destroyed` callback, which
        // locks this same conn — re-locking it here would deadlock (std Mutex
        // is not reentrant).
        let (upstream, upstream_backend) = {
            let mut guard = conn.lock().unwrap();
            let upstream = guard.s2u.remove(&object_id);
            if let Some(u) = &upstream {
                guard.u2s.remove(u);
                guard.u2s_proto.remove(&u.protocol_id());
            }
            guard.shm_pools.remove(&object_id);
            guard.buffers.remove(&object_id);
            guard.surfaces.remove(&object_id);
            guard.dmabuf_pending.remove(&object_id);
            (upstream, guard.upstream.clone())
        };
        // Phase 2: destroy upstream with the conn lock released.
        if let Some(upstream) = upstream {
            let _ = upstream_backend.destroy_object(&upstream);
        }
    }
}

/// Forward one app request to the compositor. Returns the object data for a
/// newly created object when the request carries a new_id.
fn forward_request(
    conn: &mut Conn,
    role: Role,
    msg: Message<SId, OwnedFd>,
) -> Option<Arc<dyn ObjectData<ServerState>>> {
    let info = conn.server_handle.object_info(msg.sender_id.clone()).ok()?;
    let upstream_sender = conn.s2u.get(&msg.sender_id)?.clone();
    let desc = info.interface.requests.get(msg.opcode as usize);

    if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
        eprintln!(
            "seat REQ {}@{} op={} new_id={}",
            info.interface.name,
            msg.sender_id.protocol_id(),
            msg.opcode,
            msg.args
                .iter()
                .any(|a| matches!(a, Argument::NewId(id) if !id.is_null()))
        );
    }

    conn.track_request(role, &msg);

    let has_new_id = msg
        .args
        .iter()
        .any(|a| matches!(a, Argument::NewId(id) if !id.is_null()));

    // The child interface for typed constructors. wl_registry.bind is the
    // only untyped new_id and it is handled by the GlobalHandler path.
    let mut child_spec: Option<(&'static Interface, u32)> = None;
    let mut child_role = Role::Generic;
    if has_new_id {
        let Some(child_iface) = desc.and_then(|d| d.child_interface) else {
            eprintln!(
                "agent seat: dropping request with untyped new_id: {} op={}",
                info.interface.name, msg.opcode
            );
            return None;
        };
        // wayland-backend requires the child version to equal the parent's.
        child_spec = Some((child_iface, info.version));
        child_role = role_for_interface(child_iface);
    }

    // Build the upstream message, borrowing fds from `msg` (kept alive until
    // the send completes below).
    let weak = conn.self_weak.clone();
    let mut args: SmallVec<[Argument<CId, RawFd>; 4]> = SmallVec::new();
    for arg in msg.args.iter() {
        args.push(match arg {
            Argument::Int(v) => Argument::Int(*v),
            Argument::Uint(v) => Argument::Uint(*v),
            Argument::Fixed(v) => Argument::Fixed(*v),
            Argument::Str(v) => Argument::Str(v.clone()),
            Argument::Array(v) => Argument::Array(v.clone()),
            Argument::Object(id) => Argument::Object(conn.map_object_upstream(id)),
            Argument::NewId(_) => Argument::NewId(CId::null()),
            Argument::Fd(fd) => Argument::Fd(fd.as_raw_fd()),
        });
    }
    let upstream_msg = Message {
        sender_id: upstream_sender,
        opcode: msg.opcode,
        args,
    };

    let new_data: Option<Arc<dyn wayland_backend::client::ObjectData>> = has_new_id.then(|| {
        Arc::new(UObj {
            conn: weak.clone(),
            role: child_role,
        }) as Arc<dyn wayland_backend::client::ObjectData>
    });

    let new_upstream = match conn
        .upstream
        .send_request(upstream_msg, new_data, child_spec)
    {
        Ok(id) => id,
        Err(e) => {
            eprintln!(
                "agent seat: failed to forward request {} op={}: {e:?}",
                info.interface.name, msg.opcode
            );
            return None;
        }
    };

    if has_new_id {
        // The server backend already allocated the server-side new object;
        // recover it from the message (the NewId argument carries it).
        let server_id = msg.args.iter().find_map(|a| match a {
            Argument::NewId(id) if !id.is_null() => Some(id.clone()),
            _ => None,
        })?;
        conn.link(server_id.clone(), new_upstream);
        conn.track_created(child_role, &server_id, &msg);
        Some(Arc::new(SObj {
            conn: weak,
            role: child_role,
        }))
    } else {
        None
    }
}

// ------------------------------------------------------------------
// Upstream side: forwards compositor events to the app
// ------------------------------------------------------------------

pub(crate) struct UObj {
    pub conn: Weak<Mutex<Conn>>,
    pub role: Role,
}

impl wayland_backend::client::ObjectData for UObj {
    fn event(
        self: Arc<Self>,
        _backend: &CBackend,
        msg: Message<CId, OwnedFd>,
    ) -> Option<Arc<dyn wayland_backend::client::ObjectData>> {
        let conn = self.conn.upgrade()?;
        let mut conn = conn.lock().unwrap();
        if self.role == Role::Registry {
            handle_registry_event(&mut conn, msg);
            return None;
        }
        forward_event(&mut conn, self.role, msg)
    }

    fn destroyed(&self, object_id: CId) {
        // Use try_lock: this callback can fire from inside a forwarded
        // destructor request while the same thread already holds the conn lock
        // (forward_request -> send_request -> upstream destroy -> here). The
        // map cleanup is redundant in that case — the paired SObj::destroyed
        // already removes the mapping — so skipping it is safe.
        if let Some(conn) = self.conn.upgrade() {
            if let Ok(mut guard) = conn.try_lock() {
                guard.unlink_upstream(&object_id);
            }
        }
    }
}

/// Upstream registry events are not forwarded verbatim: the server backend
/// runs its own registry with its own global names. We re-advertise known
/// globals on our side and remember the name mapping for bind forwarding.
fn handle_registry_event(conn: &mut Conn, msg: Message<CId, OwnedFd>) {
    match msg.opcode {
        // wl_registry.global(name, interface, version)
        0 => {
            let (
                Some(Argument::Uint(name)),
                Some(Argument::Str(iface_name)),
                Some(Argument::Uint(version)),
            ) = (msg.args.first(), msg.args.get(1), msg.args.get(2))
            else {
                return;
            };
            let Some(iface_name) = iface_name.as_ref() else {
                return;
            };
            let Ok(name_str) = iface_name.to_str() else {
                return;
            };
            if interfaces::is_denied(name_str) {
                return;
            }
            let Some(iface) = interfaces::by_name(name_str) else {
                // Unknown vendor global: the app can live without it.
                return;
            };
            let version = (*version).min(iface.version);
            if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
                eprintln!("seat GLOBAL upstream={name} {name_str} v{version}");
            }
            let global_id = conn.server_handle.create_global::<ServerState>(
                iface,
                version,
                Arc::new(GlobalHandlerObj {
                    conn: conn.self_weak.clone(),
                    upstream_name: *name,
                }),
            );
            conn.globals
                .insert(*name, (global_id.clone(), iface, version));
            conn.server_global_to_name.insert(global_id, *name);
        }
        // wl_registry.global_remove(name)
        1 => {
            if let Some(Argument::Uint(name)) = msg.args.first() {
                if let Some((global_id, _, _)) = conn.globals.remove(name) {
                    conn.server_global_to_name.remove(&global_id);
                    conn.server_handle.remove_global::<ServerState>(global_id);
                }
            }
        }
        _ => {}
    }
}

/// Forward one compositor event to the app.
fn forward_event(
    conn: &mut Conn,
    role: Role,
    msg: Message<CId, OwnedFd>,
) -> Option<Arc<dyn wayland_backend::client::ObjectData>> {
    let info = conn.upstream.info(msg.sender_id.clone()).ok()?;
    let desc = info.interface.events.get(msg.opcode as usize);

    // Keep synthetic serials ahead of the compositor serials already seen by
    // the client. Chromium is stricter than GTK about stale keyboard serials.
    let first_arg_is_serial = match role {
        Role::Pointer => matches!(msg.opcode, 0 | 1 | 3),
        Role::Keyboard => matches!(msg.opcode, 1..=4),
        _ => false,
    };
    if first_arg_is_serial {
        if let Some(Argument::Uint(serial)) = msg.args.first() {
            conn.next_serial = serial.wrapping_add(1).max(1);
        }
    }

    // wl_display.error(code, object_id, message): the compositor rejected
    // something — surface it loudly, it is the proxied app's death sentence.
    if role == Role::Display && msg.opcode == 0 {
        let mut code = 0u32;
        let mut message = String::new();
        for arg in &msg.args {
            match arg {
                Argument::Uint(v) => code = *v,
                Argument::Str(s) => {
                    message = s
                        .as_ref()
                        .map(|c| c.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }
                _ => {}
            }
        }
        eprintln!("agent seat: wl_display.error code={code} msg={message}");
    }

    let server_display = conn
        .u2s
        .get(&conn.upstream.display_id())
        .cloned()
        .unwrap_or_else(SId::null);

    // wl_display.delete_id: destroy the server-side twin.
    if role == Role::Display && msg.opcode == 1 {
        if let Some(Argument::Uint(proto_id)) = msg.args.first() {
            if let Some(server_obj) = conn.u2s_proto.remove(proto_id) {
                if let Some(upstream_obj) = conn.s2u.remove(&server_obj) {
                    conn.u2s.remove(&upstream_obj);
                }
                let _ = conn
                    .server_handle
                    .destroy_object::<ServerState>(&server_obj);
            }
        }
        let args: SmallVec<[Argument<SId, RawFd>; 4]> = msg
            .args
            .iter()
            .map(|a| match a {
                Argument::Int(v) => Argument::Int(*v),
                Argument::Uint(v) => Argument::Uint(*v),
                Argument::Fixed(v) => Argument::Fixed(*v),
                Argument::Str(v) => Argument::Str(v.clone()),
                Argument::Array(v) => Argument::Array(v.clone()),
                Argument::Object(id) => Argument::Object(conn.map_object_server(id)),
                Argument::NewId(id) => Argument::NewId(conn.map_object_server(id)),
                Argument::Fd(fd) => Argument::Fd(fd.as_raw_fd()),
            })
            .collect();
        let out = Message {
            sender_id: server_display,
            opcode: msg.opcode,
            args,
        };
        let _ = conn.server_handle.send_event(out);
        return None;
    }

    // Events that create objects (e.g. wl_data_device.data_offer): create the
    // server-side twin first so its id can go into the forwarded event. The
    // child's role is also returned so the client backend can associate data
    // with the new upstream object (returning None there is a panic).
    let mut new_ids: Vec<(CId, SId)> = Vec::new();
    let mut created_role: Option<Role> = None;
    for arg in &msg.args {
        if let Argument::NewId(id) = arg {
            if id.is_null() {
                continue;
            }
            let Some(child_iface) = desc.and_then(|d| d.child_interface) else {
                continue;
            };
            let child_version = info.version.min(child_iface.version);
            let child_role = role_for_interface(child_iface);
            created_role = Some(child_role);
            let Some(client_id) = conn.client_id.clone() else {
                continue;
            };
            let Ok(server_obj) = conn.server_handle.create_object::<ServerState>(
                client_id,
                child_iface,
                child_version,
                Arc::new(SObj {
                    conn: conn.self_weak.clone(),
                    role: child_role,
                }),
            ) else {
                continue;
            };
            conn.link(server_obj.clone(), id.clone());
            new_ids.push((id.clone(), server_obj));
        }
    }

    // wl_keyboard.keymap(format, fd, size): keep a copy of the keymap.
    if role == Role::Keyboard && msg.opcode == 0 {
        if let Some(Argument::Fd(fd)) = msg.args.get(1) {
            if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
                // SAFETY: libc::stat is a plain C output structure and all-zero
                // is a valid initialization before fstat overwrites it.
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: fd is borrowed from the live Wayland event and st
                // points to a correctly sized writable libc::stat.
                let r = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
                eprintln!(
                    "seat KEYMAP fd={} fstat={} size={}",
                    fd.as_raw_fd(),
                    r,
                    if r == 0 { st.st_size } else { -1 }
                );
            }
            conn.keymap_fd = Some(dup_fd(fd.as_fd()));
        }
        if let Some(Argument::Uint(size)) = msg.args.get(2) {
            conn.keymap_size = *size;
        }
    }
    // wl_keyboard.enter/leave args are (serial, surface, ...). Track the
    // server-side surface the app actually received; using arg 0 (serial)
    // silently lost focus and made keyboard injection guess among surfaces.
    if role == Role::Keyboard && (msg.opcode == 1 || msg.opcode == 2) {
        if let Some(Argument::Object(surface)) = msg.args.get(1) {
            let server_surface = (!surface.is_null())
                .then(|| conn.u2s.get(surface).cloned())
                .flatten();
            if msg.opcode == 1 {
                conn.focused_surface = server_surface.clone();
                conn.keyboard_focus = server_surface;
            } else if conn.focused_surface.as_ref() == server_surface.as_ref() {
                conn.focused_surface = None;
                conn.keyboard_focus = None;
            }
        }
    }

    let server_sender = conn.map_object_server(&msg.sender_id);
    let mut args: SmallVec<[Argument<SId, RawFd>; 4]> = SmallVec::new();
    for arg in msg.args.iter() {
        args.push(match arg {
            Argument::Int(v) => Argument::Int(*v),
            Argument::Uint(v) => Argument::Uint(*v),
            Argument::Fixed(v) => Argument::Fixed(*v),
            Argument::Str(v) => Argument::Str(v.clone()),
            Argument::Array(v) => Argument::Array(v.clone()),
            Argument::Object(id) => Argument::Object(conn.map_object_server(id)),
            Argument::NewId(id) => Argument::NewId(
                new_ids
                    .iter()
                    .find(|(cid, _)| cid == id)
                    .map(|(_, sid)| sid.clone())
                    .unwrap_or_else(SId::null),
            ),
            Argument::Fd(fd) => Argument::Fd(fd.as_raw_fd()),
        });
    }
    let out = Message {
        sender_id: server_sender,
        opcode: msg.opcode,
        args,
    };
    if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
        eprintln!("seat EVT {} op={}", info.interface.name, msg.opcode);
    }
    if let Err(error) = conn.server_handle.send_event(out) {
        eprintln!(
            "agent seat: failed to forward event {} op={}: {error:?}",
            info.interface.name, msg.opcode
        );
    }

    // wl_callback.done is a destructor: drop the server-side callback too.
    if role == Role::Callback && msg.opcode == 0 {
        if let Some(server_obj) = conn.u2s.get(&msg.sender_id).cloned() {
            let _ = conn
                .server_handle
                .destroy_object::<ServerState>(&server_obj);
            conn.unlink_server(&server_obj);
        }
    }

    // If this event created an object upstream, hand the client backend the
    // data for the new upstream object.
    created_role.map(|r| {
        Arc::new(UObj {
            conn: conn.self_weak.clone(),
            role: r,
        }) as Arc<dyn wayland_backend::client::ObjectData>
    })
}

// ------------------------------------------------------------------
// Globals
// ------------------------------------------------------------------

pub(crate) struct GlobalHandlerObj {
    pub conn: Weak<Mutex<Conn>>,
    pub upstream_name: u32,
}

impl GlobalHandler<ServerState> for GlobalHandlerObj {
    fn bind(
        self: Arc<Self>,
        _handle: &Handle,
        _state: &mut ServerState,
        _client_id: ClientId,
        global_id: GlobalId,
        object_id: SId,
    ) -> Arc<dyn ObjectData<ServerState>> {
        let Some(conn) = self.conn.upgrade() else {
            return dead_sobj(&self.conn);
        };
        let mut conn = conn.lock().unwrap();
        let weak = conn.self_weak.clone();
        let Some(&(_, iface, _advertised)) = conn.globals.get(&self.upstream_name) else {
            return dead_sobj(&weak);
        };
        let role = role_for_interface(iface);
        let Some(upstream_registry) = conn.upstream_registry.clone() else {
            return dead_sobj(&weak);
        };
        // Bind upstream at the exact version the client requested (the server
        // object was created at it). Keeping both twins at the same version is
        // what makes the child-version checks pass in both directions later.
        let version = conn
            .server_handle
            .object_info(object_id.clone())
            .map(|i| i.version)
            .unwrap_or(_advertised);
        if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
            eprintln!("seat BIND {} v{}", iface.name, version);
        }

        // wl_registry.bind(name, interface, version, new_id)
        let name_c = match std::ffi::CString::new(iface.name) {
            Ok(n) => n,
            Err(_) => return dead_sobj(&weak),
        };
        let args: SmallVec<[Argument<CId, RawFd>; 4]> = smallvec![
            Argument::Uint(self.upstream_name),
            Argument::Str(Some(Box::new(name_c))),
            Argument::Uint(version),
            Argument::NewId(CId::null()),
        ];
        let msg = Message {
            sender_id: upstream_registry,
            opcode: 0,
            args,
        };
        let data = Arc::new(UObj {
            conn: weak.clone(),
            role,
        });
        let Ok(new_upstream) = conn.upstream.send_request(
            msg,
            Some(data as Arc<dyn wayland_backend::client::ObjectData>),
            Some((iface, version)),
        ) else {
            return dead_sobj(&weak);
        };
        let _ = global_id;
        conn.link(object_id.clone(), new_upstream);
        Arc::new(SObj { conn: weak, role })
    }
}

fn dead_sobj(conn: &Weak<Mutex<Conn>>) -> Arc<dyn ObjectData<ServerState>> {
    Arc::new(SObj {
        conn: conn.clone(),
        role: Role::Generic,
    })
}

// ------------------------------------------------------------------
// Connection setup
// ------------------------------------------------------------------

/// ClientData handed to insert_client: marks the conn dead on disconnect.
pub(crate) struct ConnClientData {
    pub conn: Weak<Mutex<Conn>>,
}

impl ClientData for ConnClientData {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(
        &self,
        _client_id: ClientId,
        reason: wayland_backend::server::DisconnectReason,
    ) {
        if std::env::var("AGENT_SEAT_DEBUG").is_ok() {
            eprintln!("agent seat: app disconnected: {reason:?}");
        }
        if let Some(conn) = self.conn.upgrade() {
            conn.lock().unwrap().dead = true;
        }
    }
}

/// Build the proxy connection pair for one accepted app stream.
///
/// The upstream registry handshake happens BEFORE the app is accepted on the
/// server backend: the backend answers `wl_display.sync` locally (it never
/// reaches us), so a client's initial roundtrip completes instantly — the
/// globals must already be registered by then, or toolkits see an empty
/// registry and give up on Wayland (GTK does exactly this).
pub(crate) fn setup(
    app_stream: std::os::unix::net::UnixStream,
    upstream_stream: std::os::unix::net::UnixStream,
) -> Result<(SBackend<ServerState>, Arc<Mutex<Conn>>), String> {
    // Both directions must be non-blocking: a slow/peer that stops reading
    // must never turn a buffered flush into a blocking sendmsg while the conn
    // lock is held (that deadlocks the whole proxy).
    app_stream
        .set_nonblocking(true)
        .map_err(|e| format!("app socket: {e}"))?;
    upstream_stream
        .set_nonblocking(true)
        .map_err(|e| format!("upstream socket: {e}"))?;

    let upstream = CBackend::connect(upstream_stream).map_err(|e| format!("upstream: {e}"))?;
    let server = SBackend::<ServerState>::new().map_err(|e| format!("server: {e}"))?;
    let handle = server.handle();

    let conn = Arc::new_cyclic(|weak: &Weak<Mutex<Conn>>| {
        Mutex::new(Conn {
            self_weak: weak.clone(),
            server_handle: handle.clone(),
            client_id: None,
            upstream: upstream.clone(),
            upstream_registry: None,
            s2u: HashMap::new(),
            u2s: HashMap::new(),
            u2s_proto: HashMap::new(),
            globals: HashMap::new(),
            server_global_to_name: HashMap::new(),
            shm_pools: HashMap::new(),
            buffers: HashMap::new(),
            surfaces: HashMap::new(),
            dmabuf_pending: HashMap::new(),
            keymap_fd: None,
            keymap_size: 0,
            pointer_obj: None,
            keyboard_obj: None,
            text_input_obj: None,
            text_input_pending_enabled: false,
            text_input_enabled: false,
            text_input_commit_serial: 0,
            focused_surface: None,
            next_serial: 1,
            pointer_focus: None,
            keyboard_focus: None,
            actions: Vec::new(),
            dead: false,
        })
    });

    // Create the upstream registry and wait for the compositor's globals.
    let registry_data = Arc::new(UObj {
        conn: Arc::downgrade(&conn),
        role: Role::Registry,
    });
    let mut args: SmallVec<[Argument<CId, RawFd>; 4]> = SmallVec::new();
    args.push(Argument::NewId(CId::null()));
    let msg = Message {
        sender_id: upstream.display_id(),
        opcode: 1,
        args,
    };
    let upstream_registry = upstream
        .send_request(
            msg,
            Some(registry_data as Arc<dyn wayland_backend::client::ObjectData>),
            Some((WlRegistry::interface(), 1)),
        )
        .map_err(|e| format!("get_registry: {e}"))?;
    conn.lock().unwrap().upstream_registry = Some(upstream_registry);
    upstream
        .flush()
        .map_err(|e| format!("flush get_registry: {e}"))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        {
            let guard = conn.lock().unwrap();
            if !guard.globals.is_empty() {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            return Err("the compositor advertised no globals within 3s".into());
        }
        if let Some(read) = upstream.prepare_read() {
            match read.read() {
                Ok(_) => {}
                Err(e) if is_would_block(&e) => {}
                Err(_) => {
                    return Err(
                        "lost the compositor connection during the registry handshake".into(),
                    );
                }
            }
        }
        let _ = upstream.dispatch_inner_queue();
        std::thread::sleep(Duration::from_millis(5));
    }

    // Globals are registered: accept the app. Its initial roundtrip now sees
    // the full registry before the locally-answered sync completes.
    let mut handle = handle;
    let client_id = handle
        .insert_client(
            app_stream,
            Arc::new(ConnClientData {
                conn: Arc::downgrade(&conn),
            }),
        )
        .map_err(|e| format!("insert_client: {e}"))?;
    conn.lock().unwrap().client_id = Some(client_id.clone());

    // Map the two display objects.
    let server_display = handle
        .object_for_protocol_id(client_id, WlDisplay::interface(), 1)
        .map_err(|e| format!("display id: {e}"))?;
    let upstream_display = upstream.display_id();
    conn.lock().unwrap().link(server_display, upstream_display);

    Ok((server, conn))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_shm_frame_survives_protocol_buffer_destruction() {
        let file = std::fs::File::open("/dev/zero").expect("open harmless fd source");
        let buffer = BufferKind::Shm {
            fd: file.into(),
            pool_size: 4096,
            offset: 0,
            width: 16,
            height: 16,
            stride: 64,
            format: 1,
        };

        let retained = retain_shm_frame(&buffer).expect("shm buffer is retainable");
        drop(buffer); // mirrors wl_buffer/wl_shm_pool protocol destruction

        // SAFETY: retained owns a live descriptor and F_GETFD takes no third
        // variadic argument.
        assert!(unsafe { libc::fcntl(retained.fd.as_raw_fd(), libc::F_GETFD) } >= 0);
        assert_eq!(retained.pool_size, 4096);
        assert_eq!((retained.width, retained.height), (16, 16));
    }
}
