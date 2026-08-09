//! A private Wayland socket through which an automation host launches the
//! apps it drives. Each connection is transparently proxied to the real
//! compositor, which keeps the app visible on the user's desktop while giving
//! the host frames and input at the app boundary.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use polling::Poller;
use wayland_backend::server::Backend as SBackend;

mod capture;
mod input;
mod interfaces;
mod proxy;

pub use capture::CapturedFrame;

use proxy::{Conn, ServerState};

use crate::SeatError;

/// Commands queued from tool threads, executed by the proxy loop.
#[allow(dead_code)]
pub(crate) enum Action {
    /// Placeholder until input injection lands.
    Ping,
}

/// One proxied app.
pub struct SeatApp {
    /// The app's pid (correlates with the computer-use bound target).
    #[allow(dead_code)]
    pub pid: u32,
    conn: Arc<Mutex<Conn>>,
    poller: Arc<Poller>,
    cleanup_paths: Mutex<CleanupPaths>,
}

/// One authenticated, rootful XWayland server connected through the private
/// agent seat. It is used as a compatibility bridge only when a native
/// Wayland client cannot expose a readable application frame.
pub struct XwaylandBridge {
    child: Option<Child>,
    runtime_dir: PathBuf,
    display: String,
    xauthority: PathBuf,
}

impl XwaylandBridge {
    /// The private X11 display assigned to this bridge.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// The owner-only Xauthority file clients must use.
    pub fn xauthority(&self) -> &std::path::Path {
        &self.xauthority
    }

    /// Configure a child process to connect exclusively to this XWayland
    /// bridge. Call [`AgentSeat::adopt_xwayland_bridge`] after the client has
    /// connected successfully.
    pub fn configure_command<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env("DISPLAY", &self.display)
            .env("XAUTHORITY", &self.xauthority)
            .env("XDG_SESSION_TYPE", "x11")
            .env_remove("WAYLAND_DISPLAY")
    }

    fn stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        if !self.runtime_dir.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.runtime_dir);
        }
    }
}

impl Drop for XwaylandBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct CleanupPaths {
    closed: bool,
    paths: Vec<PathBuf>,
}

impl CleanupPaths {
    fn register(&mut self, path: PathBuf) -> Option<PathBuf> {
        if self.closed {
            Some(path)
        } else {
            self.paths.push(path);
            None
        }
    }

    fn close(&mut self) -> Vec<PathBuf> {
        self.closed = true;
        std::mem::take(&mut self.paths)
    }
}

fn remove_cleanup_directories(paths: impl IntoIterator<Item = PathBuf>) {
    for path in paths {
        let _ = std::fs::remove_dir_all(path);
    }
}

impl SeatApp {
    /// Queue an action and wake the proxy loop (input-injection entry point).
    #[allow(dead_code)]
    pub(crate) fn send_action(&self, action: Action) {
        {
            let mut conn = self.conn.lock().unwrap();
            conn.actions.push(action);
        }
        let _ = self.poller.notify();
    }

    /// Read the app's current rendered frame (window-scoped capture).
    #[allow(dead_code)]
    pub fn capture_frame(&self) -> Result<CapturedFrame, SeatError> {
        let conn = self.conn.lock().unwrap();
        capture::capture_frame(&conn).map_err(SeatError::Capture)
    }

    /// True once the client has produced a window-sized frame rather than a
    /// startup icon or cursor-sized helper surface.
    pub fn has_interactive_frame(&self) -> bool {
        match self.capture_frame() {
            Ok(frame) => {
                if std::env::var_os("AGENT_SEAT_DEBUG").is_some() {
                    eprintln!(
                        "agent seat: candidate pid={} primary frame={}x{}",
                        self.pid, frame.width, frame.height
                    );
                }
                frame.width >= 160
                    && frame.height >= 120
                    && u64::from(frame.width) * u64::from(frame.height) >= 65_536
            }
            Err(_) => false,
        }
    }

    /// Click at surface-local (x, y). `button` is a Linux input code.
    #[allow(dead_code)]
    pub fn inject_click(&self, x: f64, y: f64, button: u32, count: u32) -> Result<(), SeatError> {
        {
            let mut conn = self.conn.lock().unwrap();
            input::inject_click(&mut conn, x, y, button, count).map_err(SeatError::Input)?;
        }
        let _ = self.poller.notify();
        Ok(())
    }

    /// Click while holding an optional `+`-separated modifier list.
    pub fn inject_click_with_modifiers(
        &self,
        x: f64,
        y: f64,
        button: u32,
        count: u32,
        modifiers: Option<&str>,
    ) -> Result<(), SeatError> {
        {
            let mut conn = self.conn.lock().unwrap();
            input::inject_click_with_modifiers(&mut conn, x, y, button, count, modifiers)
                .map_err(SeatError::Input)?;
        }
        let _ = self.poller.notify();
        Ok(())
    }

    /// Scroll at surface-local (x, y) by discrete notches.
    #[allow(dead_code)]
    pub fn inject_scroll(&self, x: f64, y: f64, dx: i32, dy: i32) -> Result<(), SeatError> {
        {
            let mut conn = self.conn.lock().unwrap();
            input::inject_scroll(&mut conn, x, y, dx, dy).map_err(SeatError::Input)?;
        }
        let _ = self.poller.notify();
        Ok(())
    }

    /// Press/release a raw keycode.
    #[allow(dead_code)]
    pub fn inject_key_raw(&self, keycode: u32, pressed: bool) -> Result<(), SeatError> {
        {
            let mut conn = self.conn.lock().unwrap();
            input::inject_key_raw(&mut conn, keycode, pressed).map_err(SeatError::Input)?;
        }
        let _ = self.poller.notify();
        Ok(())
    }

    /// Press a key or `+`-separated key combination in this app.
    pub fn inject_key_combo(&self, combination: &str) -> Result<(), SeatError> {
        {
            let mut conn = self.conn.lock().unwrap();
            input::inject_key_combo(&mut conn, combination).map_err(SeatError::Input)?;
        }
        let _ = self.poller.notify();
        Ok(())
    }

    /// Drag between two surface-local points in this app.
    pub fn inject_drag(
        &self,
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
    ) -> Result<(), SeatError> {
        {
            let mut conn = self.conn.lock().unwrap();
            input::inject_drag(&mut conn, from_x, from_y, to_x, to_y).map_err(SeatError::Input)?;
        }
        let _ = self.poller.notify();
        Ok(())
    }

    /// Type a string into the app (resolves characters via the app's keymap).
    #[allow(dead_code)]
    pub fn inject_text(&self, text: &str) -> Result<(), SeatError> {
        {
            let mut conn = self.conn.lock().unwrap();
            input::inject_text(&mut conn, text).map_err(SeatError::Input)?;
        }
        let _ = self.poller.notify();
        Ok(())
    }

    /// Remove this exact directory when the proxied connection closes.
    #[allow(dead_code)]
    pub(crate) fn add_cleanup_path(&self, path: PathBuf) {
        let remove_now = self.cleanup_paths.lock().unwrap().register(path);
        if let Some(path) = remove_now {
            remove_cleanup_directories([path]);
        }
    }

    fn cleanup_registered_paths(&self) {
        let paths = self.cleanup_paths.lock().unwrap().close();
        remove_cleanup_directories(paths);
    }
}

impl Drop for SeatApp {
    fn drop(&mut self) {
        self.cleanup_registered_paths();
    }
}

/// Private Wayland proxy that owns its socket, worker threads, and bridges.
pub struct AgentSeat {
    socket_name: String,
    socket_path: PathBuf,
    listener: UnixListener,
    upstream_socket: PathBuf,
    // A process may open several independent Wayland connections. Electron
    // does this for its browser/helper/renderer roles, sometimes under the
    // same pid, so a pid-keyed map would silently discard the visible one.
    apps: Mutex<Vec<Arc<SeatApp>>>,
    // The exact connection selected for a bound launch. Subsequent click/type
    // calls must not independently pick different Electron connections that
    // happen to share the same pid.
    bound_apps: Mutex<HashMap<u32, Weak<SeatApp>>>,
    stopping: Arc<AtomicBool>,
    socket_removed: AtomicBool,
    accept_thread: Mutex<Option<JoinHandle<()>>>,
    proxy_threads: Mutex<Vec<JoinHandle<()>>>,
    bridge_threads: Mutex<Vec<JoinHandle<()>>>,
}

static SEAT: OnceLock<Mutex<Option<Arc<AgentSeat>>>> = OnceLock::new();

fn seat_slot() -> &'static Mutex<Option<Arc<AgentSeat>>> {
    SEAT.get_or_init(|| Mutex::new(None))
}

/// The process-wide agent seat, created on first use. Fails when there is no
/// Wayland session to proxy into.
pub fn seat() -> Result<Arc<AgentSeat>, SeatError> {
    let mut slot = seat_slot().lock().unwrap();
    if let Some(existing) = slot.as_ref() {
        return Ok(existing.clone());
    }
    let new_seat = AgentSeat::create()?;
    *slot = Some(new_seat.clone());
    Ok(new_seat)
}

/// Stop the process-wide seat and release all of its sockets and threads.
/// A later call to [`seat`] creates a fresh seat.
pub fn shutdown() {
    // Keep the singleton lock until teardown finishes so a replacement cannot
    // bind the same path while the old seat is still removing it.
    let mut slot = seat_slot().lock().unwrap();
    if let Some(seat) = slot.take() {
        seat.shutdown_inner();
    }
}

/// True when an agent seat can be created in this environment (Wayland
/// session with a reachable compositor socket).
#[allow(dead_code)]
pub fn available() -> bool {
    seat().is_ok()
}

/// Remove `agent-seat-<pid>` sockets whose owning process has exited.
fn cleanup_stale_seat_sockets(runtime_dir: &str) {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return;
    };
    let my_pid = std::process::id();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(remainder) = name.strip_prefix("agent-seat-") else {
            continue;
        };
        let pid_str = remainder.split('-').next().unwrap_or_default();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == my_pid {
            continue;
        }
        // /proc/<pid> exists only while the process is alive.
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn set_owner_only_socket_permissions(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

impl AgentSeat {
    /// Create and start an independently owned agent seat.
    pub fn create() -> Result<Arc<Self>, SeatError> {
        let seat = Arc::new(Self::new_unstarted()?);
        seat.start()?;
        Ok(seat)
    }

    fn new_unstarted() -> Result<Self, SeatError> {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or(SeatError::MissingRuntimeDir)?;
        let upstream_display = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or(SeatError::NoWaylandSession)?;
        let upstream_socket = if upstream_display.starts_with('/') {
            PathBuf::from(&upstream_display)
        } else {
            PathBuf::from(&runtime_dir).join(&upstream_display)
        };
        if !upstream_socket.exists() {
            return Err(SeatError::NoWaylandSession);
        }

        // Remove seat sockets left behind by hosts that have since
        // exited (they otherwise accumulate in XDG_RUNTIME_DIR).
        cleanup_stale_seat_sockets(&runtime_dir);

        // Per-process socket name; remove a stale one from a crashed run.
        let socket_name = format!(
            "agent-seat-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let socket_path = PathBuf::from(&runtime_dir).join(&socket_name);
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).map_err(|e| {
            SeatError::SocketCreate(format!("could not bind the agent seat socket: {e}"))
        })?;
        if let Err(e) = set_owner_only_socket_permissions(&socket_path) {
            let _ = std::fs::remove_file(&socket_path);
            return Err(SeatError::SocketCreate(format!(
                "could not restrict the agent seat socket permissions: {e}"
            )));
        }
        if let Err(e) = listener.set_nonblocking(true) {
            let _ = std::fs::remove_file(&socket_path);
            return Err(SeatError::SocketCreate(format!(
                "could not make the seat socket nonblocking: {e}"
            )));
        }

        Ok(Self {
            socket_name,
            socket_path,
            listener,
            upstream_socket,
            apps: Mutex::new(Vec::new()),
            bound_apps: Mutex::new(HashMap::new()),
            stopping: Arc::new(AtomicBool::new(false)),
            socket_removed: AtomicBool::new(false),
            accept_thread: Mutex::new(None),
            proxy_threads: Mutex::new(Vec::new()),
            bridge_threads: Mutex::new(Vec::new()),
        })
    }

    fn start(self: &Arc<Self>) -> Result<(), SeatError> {
        let accept_seat = Arc::downgrade(self);
        let handle = std::thread::Builder::new()
            .name("agent-seat-accept".into())
            .spawn(move || {
                while let Some(seat) = accept_seat.upgrade() {
                    if seat.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    seat.accept_once();
                }
            })
            .map_err(|e| {
                SeatError::Process(format!("could not start the seat accept loop: {e}"))
            })?;
        *self.accept_thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    /// The WAYLAND_DISPLAY value agent-launched apps must use.
    pub fn socket_name(&self) -> String {
        self.socket_name.clone()
    }

    /// Configure a process to connect through this private Wayland seat.
    pub fn configure_command<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command.env("WAYLAND_DISPLAY", &self.socket_name)
    }

    /// Stop this seat, close every proxied connection, and reap bridge
    /// threads. Calling this more than once is safe.
    pub fn close(&self) {
        self.shutdown_inner();
    }

    /// Start an owner-authenticated XWayland server whose single root window
    /// is itself a client of this seat. `-shm` makes its pixels readable by the
    /// window-scoped capture path, while the Xauthority cookie prevents other
    /// local users from connecting to the temporary X socket.
    pub fn start_xwayland_bridge(
        &self,
        width: u16,
        height: u16,
    ) -> Result<XwaylandBridge, SeatError> {
        if width < 160 || height < 120 {
            return Err(SeatError::Xwayland(
                "bridge geometry must be at least 160x120".to_string(),
            ));
        }
        let xwayland = crate::process::find_executable("Xwayland").ok_or_else(|| {
            SeatError::Xwayland("XWayland is not installed for compatibility fallback".to_string())
        })?;
        let display_num = crate::process::pick_free_display(std::path::Path::new("/tmp/.X11-unix"))
            .ok_or_else(|| {
                SeatError::Xwayland(
                    "no free X display number for the compatibility bridge".to_string(),
                )
            })?;
        let display = format!(":{display_num}");
        let runtime_base = self.socket_path.parent().ok_or_else(|| {
            SeatError::Custom("agent seat socket has no runtime directory".to_string())
        })?;
        let runtime_dir = runtime_base.join(format!(
            "agent-seat-xwayland-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&runtime_dir).map_err(|e| {
            SeatError::Xwayland(format!("could not create XWayland runtime directory: {e}"))
        })?;
        if let Err(error) =
            std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
        {
            let _ = std::fs::remove_dir(&runtime_dir);
            return Err(SeatError::Xwayland(format!(
                "could not restrict XWayland runtime directory: {error}"
            )));
        }

        let xauthority = runtime_dir.join("Xauthority");
        let cookie = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let auth_status = match Command::new("xauth")
            .args(["-f", xauthority.to_string_lossy().as_ref(), "add"])
            .arg(&display)
            .args(["MIT-MAGIC-COOKIE-1", &cookie])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) => status,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&runtime_dir);
                return Err(SeatError::Xauth(format!(
                    "could not create XWayland authority file: {error}"
                )));
            }
        };
        if !auth_status.success() {
            let _ = std::fs::remove_dir_all(&runtime_dir);
            return Err(SeatError::Xauth(format!(
                "xauth could not create credentials for display {display}"
            )));
        }
        if let Err(error) =
            std::fs::set_permissions(&xauthority, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_dir_all(&runtime_dir);
            return Err(SeatError::Xauth(format!(
                "could not restrict XWayland authority file: {error}"
            )));
        }

        let mut command = Command::new(xwayland);
        let stderr = if std::env::var_os("AGENT_SEAT_DEBUG").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let geometry = format!("{width}x{height}");
        command
            .arg(&display)
            .args([
                "-auth",
                xauthority.to_string_lossy().as_ref(),
                "-nolisten",
                "tcp",
                "-terminate",
                "10",
                "-shm",
                "-geometry",
                &geometry,
            ])
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        let mut child = match crate::process::spawn_owned_child(&mut command) {
            Ok(child) => child,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&runtime_dir);
                return Err(SeatError::Xwayland(format!(
                    "could not start XWayland compatibility bridge: {error}"
                )));
            }
        };
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{display_num}"));
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if socket.exists() {
                return Ok(XwaylandBridge {
                    child: Some(child),
                    runtime_dir,
                    display,
                    xauthority,
                });
            }
            if child.try_wait().ok().flatten().is_some() {
                let _ = std::fs::remove_dir_all(&runtime_dir);
                return Err(SeatError::Xwayland(
                    "XWayland compatibility bridge exited during startup".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&runtime_dir);
        Err(SeatError::Xwayland(
            "XWayland compatibility bridge did not create its X socket".to_string(),
        ))
    }

    /// Transfer a successful bridge to the seat. Closing the seat connection
    /// makes XWayland exit; the owned reaper then removes only this bridge's
    /// credential directory.
    pub fn adopt_xwayland_bridge(&self, mut bridge: XwaylandBridge) -> Result<(), SeatError> {
        let child = bridge.child.take().ok_or_else(|| {
            SeatError::Xwayland("XWayland bridge process was already transferred".to_string())
        })?;
        let runtime_dir = bridge.runtime_dir.clone();
        let shared = Arc::new(Mutex::new(Some(child)));
        let reaper_child = shared.clone();
        let handle = match std::thread::Builder::new()
            .name("agent-seat-xwayland-reaper".to_string())
            .spawn(move || {
                if let Some(mut child) = reaper_child.lock().unwrap().take() {
                    let _ = child.wait();
                }
                let _ = std::fs::remove_dir_all(runtime_dir);
            }) {
            Ok(handle) => handle,
            Err(error) => {
                if let Some(mut child) = shared.lock().unwrap().take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(SeatError::Xwayland(format!(
                    "could not start XWayland reaper: {error}"
                )));
            }
        };
        // The reaper now owns cleanup. Keep the path on `bridge` until thread
        // creation succeeds so Drop removes it on every failure path.
        bridge.runtime_dir.clear();
        self.bridge_threads.lock().unwrap().push(handle);
        Ok(())
    }

    /// The proxied app with the given pid, when connected.
    #[allow(dead_code)]
    pub fn app(&self, pid: u32) -> Option<Arc<SeatApp>> {
        if let Some(bound) = self
            .bound_apps
            .lock()
            .unwrap()
            .get(&pid)
            .and_then(Weak::upgrade)
        {
            return Some(bound);
        }
        let candidates: Vec<_> = self
            .apps
            .lock()
            .unwrap()
            .iter()
            .filter(|app| app.pid == pid)
            .cloned()
            .collect();
        // Prefer the connection that owns a visible frame. `capture_frame`
        // is non-consuming, so the later screenshot still receives it.
        candidates
            .iter()
            .rev()
            .find(|app| app.has_interactive_frame())
            .cloned()
            .or_else(|| candidates.last().cloned())
    }

    /// Pin subsequent lookups for the app's pid to this exact connection.
    pub fn bind_app(&self, app: &Arc<SeatApp>) {
        self.bind_app_for_pid(app.pid, app);
    }

    /// Bind an application pid to the seat connection that transports its
    /// pixels and input. Normally both pids are identical; an XWayland bridge
    /// deliberately transports an X11 client's window on its behalf.
    pub fn bind_app_for_pid(&self, application_pid: u32, app: &Arc<SeatApp>) {
        self.bound_apps
            .lock()
            .unwrap()
            .insert(application_pid, Arc::downgrade(app));
    }

    /// The most recently connected proxied app. Used as a fallback when the
    /// bound target's pid does not match the connected peer's pid (apps that
    /// re-exec or connect from a child process).
    pub fn most_recent_app(&self) -> Option<Arc<SeatApp>> {
        self.apps.lock().unwrap().last().cloned()
    }

    /// The set of currently connected app pids.
    pub fn connected_pids(&self) -> std::collections::HashSet<u32> {
        self.apps
            .lock()
            .unwrap()
            .iter()
            .map(|app| app.pid)
            .collect()
    }

    /// Number of connected proxied apps.
    pub fn app_count(&self) -> usize {
        self.apps.lock().unwrap().len()
    }

    /// Wait until a connection from a pid NOT in `before` has a readable
    /// frame. Electron can open multiple Wayland connections under one pid;
    /// binding only the first one can select a helper with no visible surface.
    pub fn new_capturable_app(
        &self,
        before: &std::collections::HashSet<u32>,
    ) -> Option<Arc<SeatApp>> {
        let candidates: Vec<_> = self
            .apps
            .lock()
            .unwrap()
            .iter()
            .filter(|app| !before.contains(&app.pid))
            .cloned()
            .collect();
        candidates
            .into_iter()
            .rev()
            .find(|app| app.has_interactive_frame())
    }

    /// Wait until a connection from a pid not present in `before` has a
    /// readable application-sized frame.
    pub fn wait_new_capturable_app(
        &self,
        before: &std::collections::HashSet<u32>,
        timeout: Duration,
    ) -> Option<Arc<SeatApp>> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if let Some(app) = self.new_capturable_app(before) {
                return Some(app);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn accept_once(self: &Arc<Self>) {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                let pid = match trusted_peer_pid(&stream) {
                    Ok(pid) => pid,
                    Err(e) => {
                        eprintln!("agent seat: rejected connection: {e}");
                        return;
                    }
                };
                let upstream = match std::os::unix::net::UnixStream::connect(&self.upstream_socket)
                {
                    Ok(upstream) => upstream,
                    Err(e) => {
                        eprintln!("agent seat: cannot reach compositor: {e}");
                        return;
                    }
                };
                match proxy::setup(stream, upstream) {
                    Ok((server, conn)) if !self.stopping.load(Ordering::Acquire) => {
                        self.spawn_proxy_loop(server, conn, pid);
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("agent seat: proxy setup failed: {e}"),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                eprintln!("agent seat: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn spawn_proxy_loop(
        self: &Arc<Self>,
        mut server: SBackend<ServerState>,
        conn: Arc<Mutex<Conn>>,
        pid: u32,
    ) {
        let Ok(poller) = Poller::new().map(Arc::new) else {
            return;
        };
        let server_fd = server.poll_fd().as_raw_fd();
        let upstream_fd = conn.lock().unwrap().upstream.poll_fd().as_raw_fd();
        // Safety: server_fd remains owned by server for the lifetime of the
        // proxy thread and is removed only after the poller stops using it.
        let _ = unsafe {
            poller.add_with_mode(
                server_fd,
                polling::Event::readable(1),
                polling::PollMode::Level,
            )
        };
        // Safety: upstream_fd remains owned by conn for the lifetime of the
        // proxy thread and is removed only after the poller stops using it.
        let _ = unsafe {
            poller.add_with_mode(
                upstream_fd,
                polling::Event::readable(2),
                polling::PollMode::Level,
            )
        };

        let app = Arc::new(SeatApp {
            pid,
            conn: conn.clone(),
            poller: poller.clone(),
            cleanup_paths: Mutex::new(CleanupPaths::default()),
        });
        self.apps.lock().unwrap().push(app.clone());

        let upstream = conn.lock().unwrap().upstream.clone();
        let stopping = self.stopping.clone();
        let cleanup = ProxyLoopCleanup {
            seat: Arc::downgrade(self),
            app: app.clone(),
            conn: conn.clone(),
            poller: poller.clone(),
        };
        match std::thread::Builder::new()
            .name(format!("agent-seat-proxy-{pid}"))
            .spawn(move || {
                let cleanup = cleanup;
                run_loop(
                    &mut server,
                    &cleanup.conn,
                    &upstream,
                    &cleanup.poller,
                    &stopping,
                );
            }) {
            Ok(handle) => self.proxy_threads.lock().unwrap().push(handle),
            Err(e) => {
                eprintln!("agent seat: could not start proxy loop for pid {pid}: {e}");
                close_connection(&conn, &poller);
                app.cleanup_registered_paths();
                self.remove_app(&app);
            }
        }
    }

    fn remove_app(&self, expected: &Arc<SeatApp>) {
        remove_same_arc(&mut self.apps.lock().unwrap(), expected);
        self.bound_apps.lock().unwrap().retain(|_, bound| {
            bound
                .upgrade()
                .is_some_and(|current| !Arc::ptr_eq(&current, expected))
        });
    }

    fn remove_socket_path(&self) {
        if self.socket_removed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.socket_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("agent seat: could not remove socket: {e}");
            }
        }
    }

    fn shutdown_inner(&self) {
        self.stopping.store(true, Ordering::Release);
        self.remove_socket_path();

        // Wake and close current proxies before waiting for an in-flight
        // accept/setup operation to finish.
        self.close_active_connections();
        if let Some(handle) = self.accept_thread.lock().unwrap().take() {
            join_thread(handle, "accept");
        }

        // The accept thread is now gone, so no more proxy handles can appear.
        self.close_active_connections();
        let handles = std::mem::take(&mut *self.proxy_threads.lock().unwrap());
        for handle in handles {
            join_thread(handle, "proxy");
        }
        let bridge_handles = std::mem::take(&mut *self.bridge_threads.lock().unwrap());
        for handle in bridge_handles {
            join_thread(handle, "XWayland bridge");
        }
        self.apps.lock().unwrap().clear();
        self.bound_apps.lock().unwrap().clear();
    }

    fn close_active_connections(&self) {
        let apps = self.apps.lock().unwrap().clone();
        for app in apps {
            close_connection(&app.conn, &app.poller);
        }
    }
}

impl Drop for AgentSeat {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerCredentials {
    pid: u32,
    uid: libc::uid_t,
}

/// Kernel-authenticated peer credentials of a connecting app.
fn peer_credentials(stream: &std::os::unix::net::UnixStream) -> Result<PeerCredentials, SeatError> {
    use std::os::fd::AsFd;
    // SAFETY: libc::ucred is a plain C output structure and all-zero is a
    // valid initialization before getsockopt overwrites it.
    let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the stream fd is live, ucred is writable and correctly sized,
    // and len points to its initialized size as required by getsockopt.
    let ret = unsafe {
        libc::getsockopt(
            stream.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut ucred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Err(SeatError::PeerCredential(format!(
            "could not read SO_PEERCRED: {}",
            std::io::Error::last_os_error()
        )));
    }
    if len as usize != std::mem::size_of::<libc::ucred>() || ucred.pid <= 0 {
        return Err(SeatError::PeerCredential(
            "SO_PEERCRED returned invalid credentials".to_string(),
        ));
    }
    Ok(PeerCredentials {
        pid: ucred.pid as u32,
        uid: ucred.uid,
    })
}

fn validate_peer(
    credentials: PeerCredentials,
    expected_uid: libc::uid_t,
) -> Result<u32, SeatError> {
    if credentials.uid != expected_uid {
        return Err(SeatError::PeerCredential(format!(
            "peer uid {} does not match agent-seat owner uid {expected_uid}",
            credentials.uid
        )));
    }
    Ok(credentials.pid)
}

fn trusted_peer_pid(stream: &std::os::unix::net::UnixStream) -> Result<u32, SeatError> {
    let credentials = peer_credentials(stream)?;
    validate_peer(credentials, effective_uid())
}

fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid takes no arguments, has no memory preconditions, and
    // simply returns the effective uid of the calling process.
    unsafe { libc::geteuid() }
}

fn remove_same_arc<V>(items: &mut Vec<Arc<V>>, expected: &Arc<V>) -> bool {
    let before = items.len();
    items.retain(|current| !Arc::ptr_eq(current, expected));
    items.len() != before
}

fn close_connection(conn: &Arc<Mutex<Conn>>, poller: &Poller) {
    let (server_handle, client_id) = {
        let mut conn = conn.lock().unwrap();
        conn.dead = true;
        // End the compositor side even when another component still holds a
        // SeatApp/Conn Arc after the proxy thread exits.
        // SAFETY: the fd belongs to the live upstream backend. shutdown does
        // not take ownership; errors are intentionally ignored during close.
        let _ = unsafe { libc::shutdown(conn.upstream.poll_fd().as_raw_fd(), libc::SHUT_RDWR) };
        (conn.server_handle.clone(), conn.client_id.clone())
    };
    if let Some(client_id) = client_id {
        server_handle.kill_client(
            client_id,
            wayland_backend::server::DisconnectReason::ConnectionClosed,
        );
    }
    let _ = poller.notify();
}

fn join_thread(handle: JoinHandle<()>, kind: &str) {
    if handle.thread().id() == std::thread::current().id() {
        return;
    }
    if handle.join().is_err() {
        eprintln!("agent seat: {kind} thread panicked during shutdown");
    }
}

struct ProxyLoopCleanup {
    seat: Weak<AgentSeat>,
    app: Arc<SeatApp>,
    conn: Arc<Mutex<Conn>>,
    poller: Arc<Poller>,
}

impl Drop for ProxyLoopCleanup {
    fn drop(&mut self) {
        close_connection(&self.conn, &self.poller);
        self.app.cleanup_registered_paths();
        if let Some(seat) = self.seat.upgrade() {
            seat.remove_app(&self.app);
        }
    }
}

fn run_loop(
    server: &mut SBackend<ServerState>,
    conn: &Arc<Mutex<Conn>>,
    upstream: &wayland_backend::client::Backend,
    poller: &Poller,
    stopping: &AtomicBool,
) {
    let mut events = polling::Events::new();
    let dbg = std::env::var("AGENT_SEAT_DEBUG").is_ok();
    while !stopping.load(Ordering::Acquire) {
        {
            let guard = conn.lock().unwrap();
            if dbg {
                eprintln!("seat LOOP top dead={}", guard.dead);
            }
            if guard.dead {
                break;
            }
        }
        events.clear();
        if dbg {
            eprintln!("seat LOOP poll wait");
        }
        if poller
            .wait(&mut events, Some(Duration::from_millis(200)))
            .is_err()
        {
            break;
        }
        let mut server_ready = false;
        let mut upstream_ready = false;
        for event in events.iter() {
            match event.key {
                1 => server_ready = true,
                2 => upstream_ready = true,
                _ => {} // NOTIFY_KEY: actions drained below
            }
        }
        if dbg {
            eprintln!("seat LOOP ready server={server_ready} upstream={upstream_ready}");
        }

        if server_ready {
            let mut state = ServerState;
            if let Err(e) = server.dispatch_all_clients(&mut state) {
                if !stopping.load(Ordering::Acquire) {
                    eprintln!("agent seat: server dispatch error: {e}");
                }
                conn.lock().unwrap().dead = true;
            }
        }

        if upstream_ready {
            if let Some(guard) = upstream.prepare_read() {
                match guard.read() {
                    Ok(_) => {}
                    Err(e) if proxy::is_would_block(&e) => {}
                    Err(e) => {
                        if !stopping.load(Ordering::Acquire) {
                            eprintln!("agent seat: upstream read error: {e}");
                        }
                        conn.lock().unwrap().dead = true;
                    }
                }
            }
            if let Err(e) = upstream.dispatch_inner_queue() {
                if !stopping.load(Ordering::Acquire) {
                    eprintln!("agent seat: upstream dispatch error: {e}");
                }
            }
        }

        // Drain queued actions (Phase B/C executes them here).
        {
            let mut guard = conn.lock().unwrap();
            guard.actions.clear();
        }

        if dbg {
            eprintln!("seat LOOP flush");
        }
        // Flush both directions.
        let _ = server.flush(None);
        let _ = upstream.flush();
    }
    if dbg {
        eprintln!("seat LOOP exited");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_socket(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agent-seat-{label}-{}-{nonce}.sock",
            std::process::id()
        ))
    }

    #[test]
    fn socket_permissions_are_owner_only() {
        let path = unique_test_socket("permissions");
        let listener = UnixListener::bind(&path).expect("bind test socket");
        set_owner_only_socket_permissions(&path).expect("restrict test socket");

        let mode = std::fs::metadata(&path)
            .expect("socket metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        drop(listener);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn peer_credentials_match_the_connecting_process() {
        let path = unique_test_socket("credentials");
        let listener = UnixListener::bind(&path).expect("bind test socket");
        let client = std::os::unix::net::UnixStream::connect(&path).expect("connect test socket");
        let (server, _) = listener.accept().expect("accept test socket");

        let credentials = peer_credentials(&server).expect("read peer credentials");
        assert_eq!(credentials.pid, std::process::id());
        assert_eq!(credentials.uid, effective_uid());
        assert_eq!(
            validate_peer(credentials, effective_uid()).unwrap(),
            std::process::id()
        );

        drop((client, server, listener));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn peer_uid_mismatch_is_rejected() {
        let uid = effective_uid();
        let credentials = PeerCredentials {
            pid: std::process::id(),
            uid,
        };
        assert!(validate_peer(credentials, uid.wrapping_add(1)).is_err());
    }

    #[test]
    fn one_connection_cannot_remove_another_from_the_same_pid() {
        let stale = Arc::new("stale");
        let replacement = Arc::new("replacement");
        let mut apps = vec![stale.clone(), replacement.clone()];

        assert!(remove_same_arc(&mut apps, &stale));
        assert_eq!(apps.len(), 1);
        assert!(Arc::ptr_eq(&apps[0], &replacement));
        assert!(!remove_same_arc(&mut apps, &stale));
    }

    #[test]
    fn cleanup_paths_are_exact_and_late_paths_are_returned() {
        let root = unique_test_socket("cleanup-root");
        let profile = root.join("profile");
        let sibling = root.join("keep");
        std::fs::create_dir_all(&profile).expect("create profile");
        std::fs::create_dir_all(&sibling).expect("create sibling");

        let mut cleanup = CleanupPaths::default();
        assert!(cleanup.register(profile.clone()).is_none());
        remove_cleanup_directories(cleanup.close());
        assert!(!profile.exists());
        assert!(sibling.exists());

        let late = root.join("late-profile");
        std::fs::create_dir_all(&late).expect("create late profile");
        let remove_now = cleanup.register(late.clone()).expect("closed registry");
        remove_cleanup_directories([remove_now]);
        assert!(!late.exists());
        assert!(sibling.exists());

        let _ = std::fs::remove_dir_all(root);
    }

    /// Dump the globals advertised through the seat proxy, using a raw
    /// wayland-backend client (the same view a real app gets).
    #[test]
    #[ignore]
    fn dump_seat_registry() {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("skipping: no Wayland session");
            return;
        }
        let seat = seat().expect("seat must be creatable");
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
        let stream =
            std::os::unix::net::UnixStream::connect(format!("{runtime}/{}", seat.socket_name()))
                .expect("connect to seat");
        let backend = wayland_backend::client::Backend::connect(stream).expect("backend");

        struct Dump;
        impl wayland_backend::client::ObjectData for Dump {
            fn event(
                self: Arc<Self>,
                _b: &wayland_backend::client::Backend,
                msg: wayland_backend::protocol::Message<
                    wayland_backend::client::ObjectId,
                    std::os::fd::OwnedFd,
                >,
            ) -> Option<Arc<dyn wayland_backend::client::ObjectData>> {
                // wl_registry.global(name, interface, version)
                if msg.opcode == 0 {
                    if let (
                        Some(wayland_backend::protocol::Argument::Uint(name)),
                        Some(wayland_backend::protocol::Argument::Str(iface)),
                        Some(wayland_backend::protocol::Argument::Uint(version)),
                    ) = (msg.args.first(), msg.args.get(1), msg.args.get(2))
                    {
                        eprintln!(
                            "GLOBAL {name}: {} v{version}",
                            iface
                                .as_ref()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        );
                    }
                }
                None
            }
            fn destroyed(&self, _id: wayland_backend::client::ObjectId) {}
        }

        let mut args = smallvec::SmallVec::new();
        args.push(wayland_backend::protocol::Argument::NewId(
            wayland_backend::client::ObjectId::null(),
        ));
        let msg = wayland_backend::protocol::Message {
            sender_id: backend.display_id(),
            opcode: 1,
            args,
        };
        use wayland_client::protocol::wl_registry::WlRegistry;
        use wayland_client::Proxy;
        backend
            .send_request(
                msg,
                Some(Arc::new(Dump) as Arc<dyn wayland_backend::client::ObjectData>),
                Some((WlRegistry::interface(), 1)),
            )
            .expect("get_registry");
        backend.flush().expect("flush");

        // Pump events for a couple of seconds.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Some(guard) = backend.prepare_read() {
                let _ = guard.read();
            }
            let _ = backend.dispatch_inner_queue();
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// End-to-end proxy smoke test on a live Wayland session: launch a real
    /// GTK app through the seat and verify it completes the protocol handshake
    /// (registry -> binds -> surface creation) without dying. Requires a
    /// running graphical session, so it is #[ignore]d in normal runs:
    /// `cargo test agent_seat -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn proxy_forwards_a_real_gtk_app() {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("skipping: no Wayland session");
            return;
        }
        let seat = seat().expect("seat must be creatable in a Wayland session");

        let app_bin =
            std::env::var("AGENT_SEAT_TEST_APP").unwrap_or_else(|_| "gnome-calculator".into());
        let mut cmd = std::process::Command::new(&app_bin);
        cmd.env("WAYLAND_DISPLAY", seat.socket_name())
            .env("GDK_BACKEND", "wayland")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|_| panic!("{app_bin} must launch through the seat"));
        let pid = child.id();

        // Give the app time to connect and build its window.
        let mut app = None;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(a) = seat.app(pid) {
                app = Some(a);
                break;
            }
        }
        let app = app.expect("the app's connection must reach the seat proxy");

        // Wait for at least one surface to be created through the proxy.
        let mut surfaces = 0;
        let mut dead = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            let conn = app.conn.lock().unwrap();
            surfaces = conn.surfaces.len();
            dead = conn.dead;
            if surfaces > 0 || dead {
                break;
            }
        }
        let _ = child.kill();
        let status = child.wait().ok();
        eprintln!("test: child exit status = {status:?}, dead={dead}, surfaces={surfaces}");
        assert!(!dead, "the proxied connection must stay alive");
        assert!(
            surfaces > 0,
            "the app must create at least one wl_surface through the proxy"
        );
    }

    /// Capture test: launch a software-rendered GTK app through the seat and
    /// read its rendered frame back (window-scoped capture, no portal).
    #[test]
    #[ignore]
    fn proxy_captures_app_frame() {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("skipping: no Wayland session");
            return;
        }
        let seat = seat().expect("seat must be creatable");
        let app_bin =
            std::env::var("AGENT_SEAT_TEST_APP").unwrap_or_else(|_| "gnome-calculator".into());
        let mut cmd = std::process::Command::new(&app_bin);
        cmd.env("WAYLAND_DISPLAY", seat.socket_name())
            .env("GDK_BACKEND", "wayland")
            // Software rendering => wl_shm buffers, readable without EGL.
            .env("GSK_RENDERER", "cairo")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|_| panic!("{app_bin} must launch through the seat"));
        let pid = child.id();

        let mut app = None;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(a) = seat.app(pid) {
                app = Some(a);
                break;
            }
        }
        let app = app.expect("the app's connection must reach the seat proxy");

        // Give the app time to render, then capture.
        let mut captured = None;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            match app.capture_frame() {
                Ok(frame) => {
                    captured = Some(frame);
                    break;
                }
                Err(_) => continue,
            }
        }
        let _ = child.kill();
        let _ = child.wait();

        let frame = captured.expect("must capture a rendered frame from the app");
        eprintln!(
            "test: captured frame {}x{}",
            frame.image.width(),
            frame.image.height()
        );
        assert!(frame.image.width() > 0 && frame.image.height() > 0);
        // Save for visual inspection.
        let out = std::env::temp_dir().join("agent-seat-capture.png");
        let _ = frame.image.save(&out);
        eprintln!("test: saved capture to {}", out.display());
    }

    /// Input test: click 7 + 3 = on the calculator and confirm the display
    /// shows 10. Coordinates are surface-local (match the captured buffer).
    #[test]
    #[ignore]
    fn proxy_injects_clicks_that_the_app_receives() {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("skipping: no Wayland session");
            return;
        }
        let seat = seat().expect("seat must be creatable");
        let app_bin =
            std::env::var("AGENT_SEAT_TEST_APP").unwrap_or_else(|_| "gnome-calculator".into());
        let mut cmd = std::process::Command::new(&app_bin);
        cmd.env("WAYLAND_DISPLAY", seat.socket_name())
            .env("GDK_BACKEND", "wayland")
            .env("GSK_RENDERER", "cairo")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|_| panic!("{app_bin} must launch through the seat"));
        let pid = child.id();

        let mut app = None;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(a) = seat.app(pid) {
                app = Some(a);
                break;
            }
        }
        let app = app.expect("the app's connection must reach the seat proxy");

        // Wait until a frame is available (app has rendered + input objects).
        let mut ready = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            if app.capture_frame().is_ok() {
                ready = true;
                break;
            }
        }
        assert!(ready, "app must render a frame before injecting input");

        // Button centers (surface-local), detected from the rendered buffer:
        // columns x=104,172,240,308,376 ; rows y=379(7s) 427(4s) 475(1s) 523(0s);
        // "=" is the tall orange button at column 4 (x=376) spanning rows 475-523.
        let clicks: [(f64, f64, &str); 4] = [
            (104.0, 379.0, "7"),
            (308.0, 523.0, "+"),
            (240.0, 475.0, "3"),
            (376.0, 500.0, "="),
        ];
        for (x, y, label) in clicks {
            app.inject_click(x, y, input::BTN_LEFT, 1)
                .unwrap_or_else(|e| panic!("click {label} failed: {e}"));
            std::thread::sleep(Duration::from_millis(500));
        }
        // Let the app settle, then capture the result.
        std::thread::sleep(Duration::from_millis(500));
        let frame = app.capture_frame().expect("capture after clicks");
        let out = std::env::temp_dir().join("agent-seat-after-clicks.png");
        let _ = frame.image.save(&out);
        eprintln!("test: saved post-click capture to {}", out.display());

        let _ = child.kill();
        let _ = child.wait();
    }

    /// Text-injection test: type "12+34" then Enter into the calculator and
    /// confirm the display shows 46.
    #[test]
    #[ignore]
    fn proxy_injects_typed_text() {
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("skipping: no Wayland session");
            return;
        }
        let seat = seat().expect("seat must be creatable");
        let app_bin =
            std::env::var("AGENT_SEAT_TEST_APP").unwrap_or_else(|_| "gnome-calculator".into());
        let mut cmd = std::process::Command::new(&app_bin);
        cmd.env("WAYLAND_DISPLAY", seat.socket_name())
            .env("GDK_BACKEND", "wayland")
            .env("GSK_RENDERER", "cairo")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|_| panic!("{app_bin} must launch through the seat"));
        let pid = child.id();

        let mut app = None;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            if let Some(a) = seat.app(pid) {
                app = Some(a);
                break;
            }
        }
        let app = app.expect("the app's connection must reach the seat proxy");

        // Wait until a frame is available.
        let mut ready = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(200));
            if app.capture_frame().is_ok() {
                ready = true;
                break;
            }
        }
        assert!(ready, "app must render a frame before typing");
        std::thread::sleep(Duration::from_millis(400));

        let text = std::env::var("AGENT_SEAT_TYPE_TEXT").unwrap_or_else(|_| "12+34\n".into());
        app.inject_text(&text)
            .unwrap_or_else(|e| panic!("typing failed: {e}"));
        std::thread::sleep(Duration::from_millis(700));

        let frame = app.capture_frame().expect("capture after typing");
        let out = std::env::temp_dir().join("agent-seat-after-typing.png");
        let _ = frame.image.save(&out);
        eprintln!("test: saved post-typing capture to {}", out.display());

        let _ = child.kill();
        let _ = child.wait();
    }
}
