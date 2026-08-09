#![cfg(feature = "real-display-smoke")]

use std::process::Command;

#[test]
#[ignore = "requires explicit opt-in and a real X11 or Wayland display"]
fn real_display_smoke_runs_on_the_subprocess_main_thread() {
    if std::env::var("WILDBUZZARD_REAL_DISPLAY_TEST").as_deref() != Ok("1") {
        eprintln!("skipped: set WILDBUZZARD_REAL_DISPLAY_TEST=1 to open a real display");
        return;
    }

    let executable = env!("CARGO_BIN_EXE_wild-buzzard-real-display-smoke");
    let status = Command::new(executable)
        .env("WILDBUZZARD_REAL_DISPLAY_TEST", "1")
        .status()
        .expect("failed to launch real-display smoke subprocess");
    assert!(
        status.success(),
        "real-display smoke subprocess failed: {status}"
    );
}
