mod cmd;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "saw")]
#[command(version)]
#[command(about = "Session activity watcher")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Watch(cmd::watch::WatchArgs),
    Status(cmd::status::StatusArgs),
    Hook(cmd::hook::HookArgs),
    Tui(cmd::tui::TuiArgs),
    Config(cmd::config::ConfigArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Watch(args) => cmd::watch::run(args),
        Command::Status(args) => cmd::status::run(args),
        Command::Hook(args) => cmd::hook::run(args),
        Command::Tui(args) => cmd::tui::run(args),
        Command::Config(args) => cmd::config::run(args),
    }
}
