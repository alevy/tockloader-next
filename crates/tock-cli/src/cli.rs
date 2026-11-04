//! The command line as the user types it: global flags, subcommands, and the
//! arguments each one takes. Nothing here does any work.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tock",
    version,
    about = "Install and manage apps on Tock boards",
    allow_external_subcommands = true
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: Global,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Args, Clone)]
pub(crate) struct Global {
    /// Bundled board name or path to a board file.
    #[arg(long, global = true)]
    pub(crate) board: Option<String>,
    /// Transport to reach the board with (e.g. file, openocd).
    #[arg(long, global = true)]
    pub(crate) transport: Option<String>,
    /// Device alias from the user config, or a transport-specific selector.
    #[arg(long, global = true)]
    pub(crate) device: Option<String>,
    /// Transport option, `key=value`. Repeatable.
    #[arg(long = "opt", global = true, value_parser = parse_kv)]
    pub(crate) options: Vec<(String, String)>,
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Human)]
    pub(crate) format: Format,
    /// Log every protocol message to stderr.
    #[arg(long, global = true)]
    pub(crate) trace_protocol: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Format {
    Human,
    Json,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Print the resolved board as a subcommand receives it, as JSON.
    Context,
    /// List the apps on the board.
    List,
    /// Install apps (TAB files). With no TABs, installs the project's apps.
    Install {
        tabs: Vec<PathBuf>,
        #[command(flatten)]
        change: ChangeArgs,
    },
    /// Remove apps by name.
    Uninstall {
        #[arg(required = true)]
        names: Vec<String>,
        #[command(flatten)]
        change: ChangeArgs,
    },
    /// Show the resolved configuration and where each value came from.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Serve the device protocol on stdio, connected to the resolved
    /// transport. For subcommands written in any language.
    Broker,
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
pub(crate) enum ConfigCommand {
    Show,
}

#[derive(Args)]
pub(crate) struct ChangeArgs {
    /// Show the plan without writing anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Allow moving apps that were not named, to make room.
    #[arg(long)]
    pub(crate) allow_repack: bool,
    /// Allow removing or replacing sticky apps.
    #[arg(long)]
    pub(crate) include_sticky: bool,
}

pub(crate) fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got `{s}`"))
}
