use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `Tock.toml`: what a project builds for and installs. Found by walking up
/// from the current directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProjectFile {
    /// Bundled board name or path to a board file (relative to this file).
    pub board: Option<String>,
    pub transport: Option<String>,
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
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProjectKernel {
    pub image: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProjectApp {
    pub tab: String,
}

/// `~/.config/tock/config.toml`: things specific to this machine.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct UserConfig {
    /// Named devices, e.g. `[devices.lab-3]`, selectable with `--device lab-3`.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceAlias>,
    /// Per-transport option overrides, e.g. `[transports.openocd] command = ...`.
    #[serde(default)]
    pub transports: BTreeMap<String, toml::Table>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DeviceAlias {
    pub board: Option<String>,
    pub transport: Option<String>,
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
