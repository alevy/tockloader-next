//! Board files, project files, user config, and resolving them into one
//! [`Context`] that records where every value came from.
//!
//! No I/O: files are loaded by the caller through [`FileSource`], so this
//! builds for WASM and WASI.

mod board;
mod config;
mod resolve;
mod size;

pub use board::{BUNDLED, Board, BoardInfo, Layout, Range, bundled};
pub use config::{DeviceAlias, ProjectApp, ProjectFile, ProjectKernel, UserConfig};
pub use resolve::{
    Context, Inputs, Layer, Resolved, Source, Sourced, TransportContext, env_option_key, resolve,
};
pub use size::Size;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{origin}: {message}")]
    Parse { origin: String, message: String },
    #[error("board `{0}` is not bundled; pass a path to its board file instead (bundled boards: {})", BUNDLED.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "))]
    UnknownBoard(String),
    #[error("cannot read {0}: {1}")]
    Read(String, String),
    #[error("`extends` chain is too deep or circular at {0}")]
    ExtendsLoop(String),
    #[error("no board selected")]
    NoBoard,
    #[error("no transport selected and board `{0}` has no [connection] default")]
    NoTransport(String),
    #[error("{0}")]
    Invalid(String),
}

/// How the caller provides file contents. `reference` is a path as written
/// in a file or flag; `relative_to` is the origin of the file that mentioned
/// it, if any. Returns the canonical origin (used for provenance and further
/// relative lookups) and the contents.
pub trait FileSource {
    fn read(&self, reference: &str, relative_to: Option<&str>) -> Result<(String, String), Error>;
}
