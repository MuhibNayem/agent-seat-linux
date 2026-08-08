use std::time::Duration;

use agent_seat_linux::{ComputerUse, LaunchConfig, PointerButton};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let program = arguments
        .next()
        .unwrap_or_else(|| "gnome-calculator".into());
    let harness = ComputerUse::builder()
        .native_timeout(Duration::from_secs(8))
        .fallback_timeout(Duration::from_secs(30))
        .bridge_geometry(1280, 800)
        .build()?;
    let mut app = harness.launch(LaunchConfig::new(program).args(arguments))?;

    let frame = app.capture()?;
    println!(
        "controlled pid={} transport={:?} frame={}x{}",
        app.pid(),
        app.transport(),
        frame.width,
        frame.height
    );
    frame.image.save("agent-seat-frame.png")?;

    // Harmlessly focus the center of the captured app to prove pointer input.
    app.click(
        f64::from(frame.width) / 2.0,
        f64::from(frame.height) / 2.0,
        PointerButton::Left,
        1,
    )?;

    app.stop();
    Ok(())
}
