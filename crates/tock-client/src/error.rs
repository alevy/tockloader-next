/// Exit codes are part of the CLI's contract: a script may branch on them, so
/// they are as much an interface as the output is. `tock` reads a
/// subcommand's code and exits with it unchanged, which is why there is one
/// list here rather than one per binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    /// Plan could not be made (doesn't fit, unknown app, ...).
    Failed = 1,
    // 2 is clap's usage error.
    /// Missing or ambiguous configuration.
    Config = 3,
    /// The transport lacks a needed capability.
    Capability = 4,
    Device = 5,
    /// Read-back after writing did not match.
    Verify = 6,
}

/// An error plus the commands the user could run next.
#[derive(Debug)]
pub struct CliError {
    pub code: Code,
    pub message: String,
    pub hints: Vec<String>,
}

impl CliError {
    pub fn new(code: Code, message: impl Into<String>) -> CliError {
        CliError {
            code,
            message: message.into(),
            hints: vec![],
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> CliError {
        self.hints.push(hint.into());
        self
    }
}

impl From<tock_board::Error> for CliError {
    fn from(e: tock_board::Error) -> CliError {
        let mut err = CliError::new(Code::Config, e.to_string());
        match e {
            tock_board::Error::NoBoard => {
                let names: Vec<_> = tock_board::BUNDLED.iter().map(|(n, _)| *n).collect();
                err = err
                    .hint(format!(
                        "tock --board <{}|path/to/board.toml> ...",
                        names.join("|")
                    ))
                    .hint("or add `board = \"...\"` to a Tock.toml in this project");
            }
            tock_board::Error::NoTransport(_) => {
                err = err.hint("tock --transport <name> ...");
            }
            _ => {}
        }
        err
    }
}
