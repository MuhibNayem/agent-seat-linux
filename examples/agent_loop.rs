//! A minimal perception→action loop: launch an app, capture a frame, act on
//! it, and capture the result. Demonstrates transport reporting, capture,
//! pointer/keyboard input, and clean teardown.
//!
//! Run: `cargo run --example agent_loop -- <executable> [args...]`

use agent_seat_linux::{ComputerUse, LaunchConfig, PointerButton};
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let program = args
        .next()
        .unwrap_or_else(|| "gnome-calculator".to_string());

    let mut config = LaunchConfig::new(&program);
    for extra in args {
        config = config.arg(extra);
    }

    let harness = ComputerUse::new()?;
    let mut app = harness.launch(config)?;
    println!("launched {program} via {:?}", app.transport());

    // 1. Perceive: capture the initial frame.
    let before = app.capture()?;
    before.image.save("agent-loop-before.png")?;
    println!(
        "captured {}x{} -> agent-loop-before.png",
        before.image.width(),
        before.image.height()
    );

    // 2. Act: focus, select-all, and type, exercising keyboard + text paths.
    app.click(
        before.image.width() as f64 / 2.0,
        before.image.height() as f64 / 2.0,
        PointerButton::Left,
        1,
    )?;
    app.press_key("ctrl+a")?;
    app.type_text("agent-seat-linux")?;

    // 3. Perceive again: capture the post-action frame.
    let after = app.capture()?;
    after.image.save("agent-loop-after.png")?;
    println!(
        "captured {}x{} -> agent-loop-after.png",
        after.image.width(),
        after.image.height()
    );

    // 4. Teardown: stop the app, then close the seat.
    app.stop();
    harness.close();
    println!("done");
    Ok(())
}
