#[path = "main.rs"]
mod app;

use std::process::ExitCode;

fn main() -> ExitCode {
    app::run()
}
