//! Presentation layer: command-line interface and progress display.

mod cli;
mod progress;

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
