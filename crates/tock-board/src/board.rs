use serde::{Deserialize, Serialize};
use tock_image::{Placement, Region};

use crate::{Error, FileSource, Size};

/// Boards shipped with the tool.
///
/// These are snapshots. The intent is that a board file lives beside that
/// board's linker script in the kernel tree, where the addresses come from,
/// and this table is generated from them -- so a layout cannot drift from the
/// kernel it describes without someone noticing.
pub const BUNDLED: &[(&str, &str)] = &[
    (
        "nrf52840dk",
        include_str!("../../../boards/nrf52840dk.toml"),
    ),
    (
        "raspberry_pi_pico_2",
        include_str!("../../../boards/raspberry_pi_pico_2.toml"),
    ),
];

pub fn bundled(name: &str) -> Option<&'static str> {
    BUNDLED.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct BoardFile {
    /// The `[board]` section: what this board is called and what it is.
    #[serde(rename = "board")]
    pub info: BoardInfo,
    pub layout: Layout,
    /// Keyed by transport name, like the same table in a project file or a
    /// user config: `[transports.openocd]`. Only the selected transport's
    /// entry is consulted.
    #[serde(default)]
    pub transports: toml::Table,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct BoardInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Architecture name used to pick TBFs from TABs.
    pub arch: String,
    /// How this board is normally reached. A fact about the board, like its
    /// architecture -- and the same key `Tock.toml` and the user config use,
    /// either of which may override it.
    pub transport: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Layout {
    pub flash: Range,
    pub kernel: Range,
    pub apps: Range,
    /// RAM available to apps; used to discard fixed-address builds that
    /// cannot run on this board.
    ///
    /// A fallback. The real figure is smaller and is not a fact about the
    /// board at all: the kernel's own data and bss come out of the bottom of
    /// this, and how much depends on the build. A caller that can reach the
    /// device should prefer the kernel's attribute table, which says.
    pub app_ram: Option<Range>,
    #[serde(default)]
    pub placement: Placement,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Range {
    pub start: Size,
    pub size: Size,
}

impl BoardFile {
    /// Load a board by bundled name or by path. A reference is a path if it
    /// contains `/` or ends in `.toml`; otherwise it must be a bundled name.
    /// There is no fallback from one to the other.
    ///
    /// `extends = "<reference>"` at the top level merges this file over
    /// another board file, table by table.
    pub fn load(
        reference: &str,
        relative_to: Option<&str>,
        files: &dyn FileSource,
    ) -> Result<(BoardFile, String), Error> {
        let (origin, table) = load_merged(reference, relative_to, files, 0)?;
        let board: BoardFile =
            toml::Value::Table(table)
                .try_into()
                .map_err(|e: toml::de::Error| Error::Parse {
                    origin: origin.clone(),
                    message: e.to_string(),
                })?;
        board.validate(&origin)?;
        Ok((board, origin))
    }

    pub fn parse(origin: &str, text: &str) -> Result<BoardFile, Error> {
        let board: BoardFile = toml::from_str(text).map_err(|e| Error::Parse {
            origin: origin.into(),
            message: e.to_string(),
        })?;
        board.validate(origin)?;
        Ok(board)
    }

    fn validate(&self, origin: &str) -> Result<(), Error> {
        let l = &self.layout;
        let invalid = |m: String| {
            Err(Error::Parse {
                origin: origin.into(),
                message: m,
            })
        };
        let (fs, fe) = (l.flash.start.0, l.flash.start.0 + l.flash.size.0);
        let (as_, ae) = (l.apps.start.0, l.apps.start.0 + l.apps.size.0);
        let (ks, ke) = (l.kernel.start.0, l.kernel.start.0 + l.kernel.size.0);
        if as_ < fs || ae > fe {
            return invalid(format!(
                "layout.apps ({as_:#x}..{ae:#x}) is outside layout.flash ({fs:#x}..{fe:#x})"
            ));
        }
        if as_ < ke && ks < ae {
            return invalid("layout.apps overlaps layout.kernel".into());
        }
        Ok(())
    }

    pub fn app_region(&self) -> Region {
        Region {
            start: self.layout.apps.start.0,
            size: self.layout.apps.size.0,
        }
    }

    pub fn app_ram(&self) -> Option<(u64, u64)> {
        self.layout.app_ram.map(|r| (r.start.0, r.size.0))
    }

    pub fn default_transport(&self) -> Option<&str> {
        self.info.transport.as_deref()
    }

    pub fn transport_options(&self, transport: &str) -> Option<&toml::Table> {
        self.transports.get(transport).and_then(|v| v.as_table())
    }
}

fn load_merged(
    reference: &str,
    relative_to: Option<&str>,
    files: &dyn FileSource,
    depth: u32,
) -> Result<(String, toml::Table), Error> {
    if depth > 8 {
        return Err(Error::ExtendsLoop(reference.into()));
    }
    let is_path = reference.contains('/') || reference.ends_with(".toml");
    let (origin, text) = if is_path {
        files.read(reference, relative_to)?
    } else {
        let text = bundled(reference).ok_or_else(|| Error::UnknownBoard(reference.into()))?;
        (format!("bundled:{reference}"), text.to_string())
    };
    let mut table: toml::Table = text.parse().map_err(|e: toml::de::Error| Error::Parse {
        origin: origin.clone(),
        message: e.to_string(),
    })?;
    let Some(parent) = table.remove("extends") else {
        return Ok((origin, table));
    };
    let parent = parent.as_str().ok_or_else(|| Error::Parse {
        origin: origin.clone(),
        message: "`extends` must be a string".into(),
    })?;
    let parent_rel = if origin.starts_with("bundled:") {
        None
    } else {
        Some(origin.as_str())
    };
    let (_, mut base) = load_merged(parent, parent_rel, files, depth + 1)?;
    merge(&mut base, table);
    Ok((origin, base))
}

/// Overlay `top` onto `base`: tables merge recursively, anything else
/// replaces.
fn merge(base: &mut toml::Table, top: toml::Table) {
    for (k, v) in top {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(t)) => merge(b, t),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}
