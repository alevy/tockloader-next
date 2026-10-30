//! Tock Application Bundles: a tar archive containing `metadata.toml` and one
//! TBF per architecture (or per fixed address), named
//! `<arch>[.<anything>].tbf`.

use crate::{Error, Tbf};
use std::io::Read;

#[derive(Clone, Debug)]
pub struct TabTbf {
    pub filename: String,
    pub tbf: Tbf,
}

#[derive(Clone, Debug)]
pub struct Tab {
    pub metadata: toml::Table,
    files: Vec<(String, Vec<u8>)>,
}

impl Tab {
    pub fn parse(bytes: &[u8]) -> Result<Tab, Error> {
        let mut archive = tar::Archive::new(bytes);
        let mut files = Vec::new();
        let entries = archive
            .entries()
            .map_err(|e| Error::InvalidTab(e.to_string()))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| Error::InvalidTab(e.to_string()))?;
            let path = entry
                .path()
                .map_err(|e| Error::InvalidTab(e.to_string()))?
                .to_string_lossy()
                .into_owned();
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .map_err(|e| Error::InvalidTab(e.to_string()))?;
            files.push((path, data));
        }
        let metadata = files
            .iter()
            .find(|(n, _)| n == "metadata.toml")
            .ok_or_else(|| Error::InvalidTab("missing metadata.toml".into()))?;
        let metadata: toml::Table = std::str::from_utf8(&metadata.1)
            .map_err(|e| Error::InvalidTab(e.to_string()))?
            .parse()
            .map_err(|e: toml::de::Error| Error::InvalidTab(e.to_string()))?;
        Ok(Tab { metadata, files })
    }

    pub fn name(&self) -> Option<&str> {
        self.metadata
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
    }

    /// Boards this TAB is restricted to, if any (`only-for-boards`).
    pub fn only_for_boards(&self) -> Option<Vec<String>> {
        let v = self.metadata.get("only-for-boards")?.as_str()?;
        Some(
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        )
    }

    pub fn architectures(&self) -> Vec<String> {
        let mut archs: Vec<String> = self
            .tbf_names()
            .filter_map(|n| n.split('.').next().map(str::to_string))
            .collect();
        archs.sort();
        archs.dedup();
        archs
    }

    fn tbf_names(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| n.ends_with(".tbf") && n.split('.').count() >= 2)
    }

    /// All TBFs built for `arch`. More than one means the app was built for
    /// several fixed addresses.
    pub fn tbfs_for_arch(&self, arch: &str) -> Result<Vec<TabTbf>, Error> {
        let mut out = Vec::new();
        for (name, data) in &self.files {
            if !name.ends_with(".tbf") {
                continue;
            }
            let file_arch = name.split('.').next().unwrap_or("");
            if !arch_matches(file_arch, arch) {
                continue;
            }
            let tbf = Tbf::parse(data)?;
            if data.len() > tbf.header.total_size as usize {
                return Err(Error::TbfTooLong {
                    name: name.clone(),
                    len: data.len(),
                    total_size: tbf.header.total_size,
                });
            }
            out.push(TabTbf {
                filename: name.clone(),
                tbf,
            });
        }
        Ok(out)
    }

    /// Build a TAB in memory. Used by tests and by tools that repackage apps.
    pub fn build(metadata: &str, tbfs: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |name: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append_data(&mut h, name, data).unwrap();
        };
        add("metadata.toml", metadata.as_bytes());
        for (name, data) in tbfs {
            add(name, data);
        }
        builder.into_inner().unwrap()
    }
}

/// Explicit aliases only (no prefix matching or guessing).
fn arch_matches(candidate: &str, arch: &str) -> bool {
    const ALIASES: &[(&str, &str)] = &[("rv32imc", "riscv32imc"), ("rv32imac", "riscv32imac")];
    candidate == arch
        || ALIASES
            .iter()
            .any(|(a, b)| (arch == *a && candidate == *b) || (arch == *b && candidate == *a))
}
