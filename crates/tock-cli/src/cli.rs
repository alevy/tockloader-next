//! The command line as the user types it: global flags, subcommands, and the
//! arguments each one takes. Nothing here does any work.

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use tock_board::OptionOverride;

/// Where a registered subcommand's untouched arguments are collected.
const ARGS: &str = "args";

const AFTER_HELP: &str = "Any command above that `tock` does not answer itself is a `tock-<name>`\n\
    binary, found next to `tock` or on PATH. Install one and it appears here.";

#[derive(Parser)]
#[command(
    name = "tock",
    version,
    about = "Install and manage apps on Tock boards",
    allow_external_subcommands = true,
    after_help = AFTER_HELP
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
    /// Named device from the user config: `[devices.<name>]`.
    #[arg(long, global = true)]
    pub(crate) device: Option<String>,
    /// Transport option, `[transport.]key=value`. Repeatable.
    ///
    /// `--opt serial.port=/dev/ttyUSB0` is `[transports.serial] port =
    /// "/dev/ttyUSB0"`, written on one line. Unqualified means the
    /// transport serving `default`.
    #[arg(long = "opt", global = true, value_parser = parse_opt)]
    pub(crate) options: Vec<OptionOverride>,
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

/// Only the commands `tock` answers itself. Everything else -- including
/// `list`, `install` and `uninstall` -- is a `tock-<name>` binary reached
/// through [`External`](Command::External), so there is one rule for
/// arguments rather than one per subcommand: `tock` never parses them.
#[derive(Subcommand)]
pub(crate) enum Command {
    /// Print the resolved board as a subcommand receives it, as JSON.
    Context,
    /// List the transports installed here, and the options each one reads.
    Transports,
    /// Show the resolved configuration and where each value came from.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Serve the device protocol on stdio, connected to the resolved
    /// transport. For subcommands written in any language.
    Broker,
    /// `tock foo --bar` is `tock-foo --bar`, arguments untouched.
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
pub(crate) enum ConfigCommand {
    Show,
}

/// `--opt [transport.]key=value`, which is the TOML path written flat.
///
/// The qualifier is the table header: `serial.port=X` says exactly what
/// `[transports.serial] port = X` says. Without one, it is the transport
/// serving `default` -- the obvious one, for the common case where there is
/// only ever one in play.
///
/// Only the first dot splits, so an option key may contain dots even though
/// none does today.
pub(crate) fn parse_opt(s: &str) -> Result<OptionOverride, String> {
    let (name, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected [transport.]key=value, got `{s}`"))?;
    let (transport, key) = match name.split_once('.') {
        Some((transport, key)) => (Some(transport.to_string()), key),
        None => (None, name),
    };
    if key.is_empty() {
        return Err(format!("`{s}` has no option name"));
    }
    Ok(OptionOverride {
        transport,
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// The parser, plus one subcommand per `tock-*` binary on this machine.
///
/// They are registered so that `tock --help` lists them where a user looks
/// for them, beside the commands `tock` answers itself. A user should not
/// have to know which is which, and on someone else's machine the answer
/// differs anyway.
///
/// Registration buys that line and nothing else. Each one disables `--help`
/// and swallows every argument verbatim, so `tock` still parses no
/// subcommand's flags -- it only now knows enough names to print them. We
/// can say which binary provides a command but not what it does, since a
/// description would mean running it.
fn command(installed: &[String]) -> clap::Command {
    let mut command = Cli::command();
    for name in installed {
        // A name `tock` already answers wins; so does `help`, which clap
        // generates after this runs and would otherwise be a duplicate.
        if name == "help" || command.find_subcommand(name.as_str()).is_some() {
            continue;
        }
        command = command.subcommand(
            clap::Command::new(name.clone())
                .about(format!("Provided by `tock-{name}`"))
                .disable_help_flag(true)
                .arg(
                    clap::Arg::new(ARGS)
                        .num_args(..)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true),
                ),
        );
    }
    command
}

/// What the user typed, in the two pieces [`run`](crate::run) wants.
///
/// A registered subcommand comes back as [`Command::External`], the same
/// arm as a name we have never heard of -- which is the point. Knowing a
/// name well enough to print it is not knowing anything about its
/// arguments.
pub(crate) fn parse(installed: &[String]) -> (Global, Command) {
    let matches = command(installed).get_matches();
    let global = Global::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let command = match matches.subcommand() {
        Some((name, rest)) if installed.iter().any(|n| n == name) => {
            let args = rest.get_many::<String>(ARGS).into_iter().flatten().cloned();
            Command::External(std::iter::once(name.to_string()).chain(args).collect())
        }
        _ => {
            Cli::from_arg_matches(&matches)
                .unwrap_or_else(|e| e.exit())
                .command
        }
    };
    (global, command)
}
