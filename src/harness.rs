use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crate::{AgentSeat, CapturedFrame, SeatApp};

/// Error returned by the high-level computer-use API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Result type used by the high-level API.
pub type Result<T> = std::result::Result<T, Error>;

/// Pointer buttons understood by [`ControlledApp::click`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// Primary pointer button.
    Left,
    /// Secondary/context pointer button.
    Right,
    /// Middle pointer button, commonly used for autoscroll or paste.
    Middle,
}

impl PointerButton {
    fn evdev_code(self) -> u32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
        }
    }
}

/// Display transport selected for a controlled application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Application connected directly through the private Wayland socket.
    NativeWayland,
    /// Application connected through the authenticated XWayland bridge.
    Xwayland,
}

/// Cloneable description of an application to launch.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    program: OsString,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, Option<OsString>)>,
    current_dir: Option<PathBuf>,
    inherit_diagnostics: bool,
}

impl LaunchConfig {
    /// Create a launch configuration for an executable name or path.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: Vec::new(),
            current_dir: None,
            inherit_diagnostics: false,
        }
    }

    /// Append one command-line argument.
    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    /// Append several command-line arguments.
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Set one environment variable for the application.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.push((key.into(), Some(value.into())));
        self
    }

    /// Remove one inherited environment variable from the application.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.environment.push((key.into(), None));
        self
    }

    /// Set the application's working directory.
    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(directory.into());
        self
    }

    /// Inherit stdout and stderr, which is useful while integrating a new
    /// toolkit. They are null by default so controlled apps do not block on
    /// closed harness pipes.
    pub fn inherit_diagnostics(mut self, inherit: bool) -> Self {
        self.inherit_diagnostics = inherit;
        self
    }

    /// Executable configured for this launch.
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    fn environment_value(&self, key: &str) -> Option<OsString> {
        self.environment
            .iter()
            .rev()
            .find(|(candidate, _)| candidate == OsStr::new(key))
            .and_then(|(_, value)| value.clone())
            .or_else(|| std::env::var_os(key))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments).stdin(Stdio::null());
        if self.inherit_diagnostics {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        if let Some(directory) = &self.current_dir {
            command.current_dir(directory);
        }
        for (key, value) in &self.environment {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    }
}

/// Builder for a standalone Linux computer-use harness.
#[derive(Debug, Clone)]
pub struct ComputerUseBuilder {
    native_timeout: Duration,
    fallback_timeout: Duration,
    bridge_width: u16,
    bridge_height: u16,
}

impl Default for ComputerUseBuilder {
    fn default() -> Self {
        Self {
            native_timeout: Duration::from_secs(8),
            fallback_timeout: Duration::from_secs(30),
            bridge_width: 1280,
            bridge_height: 800,
        }
    }
}

impl ComputerUseBuilder {
    /// Change how long native Wayland gets to produce a readable frame.
    pub fn native_timeout(mut self, timeout: Duration) -> Self {
        self.native_timeout = timeout;
        self
    }

    /// Change how long the XWayland fallback gets to produce a frame.
    pub fn fallback_timeout(mut self, timeout: Duration) -> Self {
        self.fallback_timeout = timeout;
        self
    }

    /// Set the compatibility bridge canvas size.
    pub fn bridge_geometry(mut self, width: u16, height: u16) -> Self {
        self.bridge_width = width;
        self.bridge_height = height;
        self
    }

    /// Start the private seat and return a ready harness.
    pub fn build(self) -> Result<ComputerUse> {
        if self.native_timeout.is_zero() || self.fallback_timeout.is_zero() {
            return Err(Error::new("launch timeouts must be greater than zero"));
        }
        let seat = AgentSeat::create().map_err(Error::from)?;
        Ok(ComputerUse {
            seat,
            native_timeout: self.native_timeout,
            fallback_timeout: self.fallback_timeout,
            bridge_width: self.bridge_width,
            bridge_height: self.bridge_height,
        })
    }
}

/// Complete app-scoped computer-use harness.
pub struct ComputerUse {
    seat: Arc<AgentSeat>,
    native_timeout: Duration,
    fallback_timeout: Duration,
    bridge_width: u16,
    bridge_height: u16,
}

impl ComputerUse {
    /// Start a harness with production defaults.
    pub fn new() -> Result<Self> {
        ComputerUseBuilder::default().build()
    }

    /// Configure launch timeouts and bridge geometry.
    pub fn builder() -> ComputerUseBuilder {
        ComputerUseBuilder::default()
    }

    /// The private `WAYLAND_DISPLAY` socket, for advanced integrations.
    pub fn socket_name(&self) -> String {
        self.seat.socket_name()
    }

    /// Launch and bind an application. Native Wayland is tried first. If the
    /// application does not expose a readable app-sized frame, the same launch
    /// is retried through an authenticated XWayland bridge automatically.
    pub fn launch(&self, config: LaunchConfig) -> Result<ControlledApp> {
        let before = self.seat.connected_pids();
        let mut command = config.command();
        configure_native_wayland(&self.seat, &mut command);
        let mut child = crate::process::spawn_owned_child(&mut command)
            .map_err(|error| launch_error(&config, "native Wayland", error))?;
        let pid = child.id();

        let native_started = std::time::Instant::now();
        while native_started.elapsed() < self.native_timeout {
            if let Some(app) = self.seat.new_capturable_app(&before) {
                self.seat.bind_app_for_pid(pid, &app);
                return Ok(ControlledApp::new(child, app, Transport::NativeWayland));
            }
            if child.try_wait().map_err(Error::from)?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        kill_and_wait(&mut child);
        self.launch_xwayland(config)
    }

    fn launch_xwayland(&self, config: LaunchConfig) -> Result<ControlledApp> {
        let before = self.seat.connected_pids();
        let bridge = self
            .seat
            .start_xwayland_bridge(self.bridge_width, self.bridge_height)
            .map_err(Error::from)?;
        let mut command = config.command();
        configure_xwayland(&config, &bridge, &mut command);
        let mut child = crate::process::spawn_owned_child(&mut command)
            .map_err(|error| launch_error(&config, "XWayland", error))?;
        let pid = child.id();

        let fallback_started = std::time::Instant::now();
        let Some(app) = self
            .seat
            .wait_new_capturable_app(&before, self.fallback_timeout)
        else {
            kill_and_wait(&mut child);
            return Err(Error::new(format!(
                "{} did not expose a readable frame through native Wayland or XWayland",
                Path::new(config.program()).display()
            )));
        };

        let content_deadline = fallback_started + self.fallback_timeout;
        while std::time::Instant::now() < content_deadline {
            if app
                .capture_frame()
                .is_ok_and(|frame| frame_has_visible_content(&frame))
            {
                self.seat.bind_app_for_pid(pid, &app);
                if let Err(error) = self.seat.adopt_xwayland_bridge(bridge) {
                    kill_and_wait(&mut child);
                    return Err(Error::from(error));
                }
                return Ok(ControlledApp::new(child, app, Transport::Xwayland));
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        kill_and_wait(&mut child);
        Err(Error::new(format!(
            "{} connected through XWayland but did not render visible application content",
            Path::new(config.program()).display()
        )))
    }

    /// Explicitly stop the seat and every compatibility bridge.
    pub fn close(&self) {
        self.seat.close();
    }
}

impl Drop for ComputerUse {
    fn drop(&mut self) {
        self.seat.close();
    }
}

/// A launched application with capture and input methods bound to its exact
/// seat connection.
pub struct ControlledApp {
    child: Option<Child>,
    app: Arc<SeatApp>,
    transport: Transport,
}

impl ControlledApp {
    fn new(child: Child, app: Arc<SeatApp>, transport: Transport) -> Self {
        Self {
            child: Some(child),
            app,
            transport,
        }
    }

    /// Spawned application process identifier.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().map_or(self.app.pid, Child::id)
    }

    /// Transport selected after launch probing.
    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// Capture the application's current window-scoped frame.
    pub fn capture(&self) -> Result<CapturedFrame> {
        self.app.capture_frame().map_err(Error::from)
    }

    /// Click at frame-local coordinates.
    pub fn click(&self, x: f64, y: f64, button: PointerButton, count: u32) -> Result<()> {
        self.app
            .inject_click(x, y, button.evdev_code(), count)
            .map_err(Error::from)
    }

    /// Click while holding a `+`-separated modifier list such as `ctrl+shift`.
    pub fn click_with_modifiers(
        &self,
        x: f64,
        y: f64,
        button: PointerButton,
        count: u32,
        modifiers: &str,
    ) -> Result<()> {
        self.app
            .inject_click_with_modifiers(x, y, button.evdev_code(), count, Some(modifiers))
            .map_err(Error::from)
    }

    /// Inject a pixel scroll delta at frame-local coordinates.
    pub fn scroll(&self, x: f64, y: f64, dx: i32, dy: i32) -> Result<()> {
        self.app.inject_scroll(x, y, dx, dy).map_err(Error::from)
    }

    /// Press a key combination such as `ctrl+shift+a`.
    pub fn press_key(&self, combination: &str) -> Result<()> {
        self.app.inject_key_combo(combination).map_err(Error::from)
    }

    /// Type Unicode text through the application's private seat.
    pub fn type_text(&self, text: &str) -> Result<()> {
        self.app.inject_text(text).map_err(Error::from)
    }

    /// Drag between two frame-local points.
    pub fn drag(&self, from_x: f64, from_y: f64, to_x: f64, to_y: f64) -> Result<()> {
        self.app
            .inject_drag(from_x, from_y, to_x, to_y)
            .map_err(Error::from)
    }

    /// Kill and reap the controlled process. Calling this twice is safe.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_and_wait(&mut child);
        }
    }
}

impl Drop for ControlledApp {
    fn drop(&mut self) {
        self.stop();
    }
}

fn configure_native_wayland(seat: &AgentSeat, command: &mut Command) {
    seat.configure_command(command)
        .env("GDK_BACKEND", "wayland")
        .env("GSK_RENDERER", "cairo")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env_remove("DISPLAY");
}

fn configure_xwayland(
    config: &LaunchConfig,
    bridge: &crate::XwaylandBridge,
    command: &mut Command,
) {
    let mut java_options = config
        .environment_value("_JAVA_OPTIONS")
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if !java_options.is_empty() {
        java_options.push(' ');
    }
    java_options.push_str("-Dawt.toolkit.name=XToolkit");

    bridge
        .configure_command(command)
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "x11")
        .env("SDL_VIDEODRIVER", "x11")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("_JAVA_OPTIONS", java_options);
}

fn launch_error(config: &LaunchConfig, transport: &str, error: std::io::Error) -> Error {
    Error::new(format!(
        "could not launch {} through {transport}: {error}",
        Path::new(config.program()).display()
    ))
}

fn kill_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn frame_has_visible_content(frame: &CapturedFrame) -> bool {
    let image = frame.image.to_rgb8();
    let Some(first) = image.pixels().next().copied() else {
        return false;
    };
    image.pixels().step_by(64).any(|pixel| *pixel != first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_config_is_framework_neutral_and_cloneable() {
        let config = LaunchConfig::new("demo")
            .arg("--flag")
            .env("DEMO", "1")
            .env_remove("REMOVE_ME")
            .current_dir("/tmp")
            .inherit_diagnostics(true);
        let cloned = config.clone();
        assert_eq!(cloned.program(), OsStr::new("demo"));
        assert_eq!(cloned.arguments, [OsString::from("--flag")]);
        assert_eq!(cloned.current_dir, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn builder_rejects_zero_timeouts_before_touching_wayland() {
        let error = ComputerUse::builder()
            .native_timeout(Duration::ZERO)
            .build()
            .err()
            .expect("zero timeout must fail");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn visible_content_rejects_uniform_bridge_roots() {
        let blank = CapturedFrame {
            image: image::DynamicImage::ImageRgb8(image::RgbImage::new(1280, 800)),
            width: 1280,
            height: 800,
        };
        assert!(!frame_has_visible_content(&blank));

        let mut image = image::RgbImage::new(128, 128);
        image.put_pixel(64, 64, image::Rgb([255, 255, 255]));
        let visible = CapturedFrame {
            image: image::DynamicImage::ImageRgb8(image),
            width: 128,
            height: 128,
        };
        assert!(frame_has_visible_content(&visible));
    }
}
