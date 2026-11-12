use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Which transport to use, and for what.
///
/// A board is not always reachable one way. Flash may arrive over a debug
/// probe while the console arrives over a USB-to-UART bridge soldered beside
/// it -- two devices, neither able to do the other's job -- so a single name
/// cannot describe how to reach the board, only how to reach part of it.
///
/// The simple form stays simple:
///
/// ```toml
/// transport = "openocd"
/// ```
///
/// and the routed form names a transport per capability, with `default` for
/// everything not named:
///
/// ```toml
/// [transport]
/// default = "openocd"
/// console = "serial"
/// ```
///
/// Routes layer per key like transport options do, so a board file can give
/// a sensible `default` and `console`, and someone whose console comes over
/// RTT instead overrides that one entry without restating the rest.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Transport {
    /// One transport for everything.
    One(String),
    /// Capability name, or `default`, to transport name.
    Routed(BTreeMap<String, String>),
}

/// The route key standing for every capability not named outright.
pub const DEFAULT_ROUTE: &str = "default";

impl From<&str> for Transport {
    fn from(name: &str) -> Transport {
        Transport::One(name.to_string())
    }
}

impl From<String> for Transport {
    fn from(name: String) -> Transport {
        Transport::One(name)
    }
}

impl Transport {
    /// This choice as a route table.
    pub fn routes(&self) -> BTreeMap<String, String> {
        match self {
            Transport::One(name) => BTreeMap::from([(DEFAULT_ROUTE.to_string(), name.clone())]),
            Transport::Routed(routes) => routes.clone(),
        }
    }
}

/// `Tock.toml`: what a project builds for and installs. Found by walking up
/// from the current directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProjectFile {
    /// Bundled board name or path to a board file (relative to this file).
    pub board: Option<String>,
    pub transport: Option<Transport>,
    pub device: Option<String>,
    pub kernel: Option<ProjectKernel>,
    /// Per-transport option overrides for this project, e.g.
    /// `[transports.file] path = "flash.bin"`. Keyed by transport name, so a
    /// table for a transport that is not selected is simply not consulted.
    #[serde(default)]
    pub transports: BTreeMap<String, toml::Table>,
    #[serde(default, rename = "app")]
    pub apps: Vec<ProjectApp>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProjectKernel {
    pub image: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProjectApp {
    pub tab: String,
}

/// `~/.config/tock/config.toml`: things specific to this machine.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct UserConfig {
    /// How this machine reaches a board, in whole or per capability.
    ///
    /// Which bridge a console landed on is a fact about a desk, not about a
    /// board, so this is where it belongs: a board file can say its console
    /// is on `serial` while this says which `serial` port, or override the
    /// route outright for someone whose console comes over RTT.
    pub transport: Option<Transport>,
    /// Named devices, e.g. `[devices.lab-3]`, selectable with `--device lab-3`.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceAlias>,
    /// Per-transport option overrides, e.g. `[transports.openocd] command = ...`.
    #[serde(default)]
    pub transports: BTreeMap<String, toml::Table>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DeviceAlias {
    pub board: Option<String>,
    pub transport: Option<Transport>,
    /// Transport options for this device (serial number, port, path, ...).
    #[serde(flatten)]
    pub options: toml::Table,
}

impl ProjectFile {
    pub fn parse(origin: &str, text: &str) -> Result<ProjectFile, crate::Error> {
        toml::from_str(text).map_err(|e| crate::Error::Parse {
            origin: origin.into(),
            message: e.to_string(),
        })
    }
}

impl UserConfig {
    pub fn parse(origin: &str, text: &str) -> Result<UserConfig, crate::Error> {
        toml::from_str(text).map_err(|e| crate::Error::Parse {
            origin: origin.into(),
            message: e.to_string(),
        })
    }
}
