use std::collections::BTreeMap;

use serde::Serialize;
use tbf::Header;

use crate::{Flash, NeedRead, Plan, Region, Slot};

/// One page of flash to write, in full.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PageWrite {
    pub address: u64,
    #[serde(skip)]
    pub data: Vec<u8>,
}

/// Turn a plan into page writes.
///
/// Only bytes the plan defines are changed; everything else in a touched page
/// keeps its current contents, so partially covered pages must be known.
/// Pages whose new contents equal what is known to be there are skipped.
pub fn render(region: &Region, plan: &Plan, flash: &Flash) -> Result<Vec<PageWrite>, NeedRead> {
    // Byte runs to write, all computed from the current (pre-write) contents.
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut need = Vec::new();
    for slot in &plan.slots {
        match slot {
            Slot::Keep { .. } | Slot::Padding { existing: true, .. } => {}
            Slot::Write { address, tbf, .. } => runs.push((*address, tbf.content_bytes())),
            Slot::Move { from, to, size, .. } => match flash.read(*from, *size) {
                Ok(bytes) => runs.push((*to, bytes)),
                Err(e) => need.extend(e.pages),
            },
            Slot::Padding {
                address,
                size,
                existing: false,
            } => runs.push((*address, Header::padding(*size as u32).to_bytes())),
        }
    }
    if plan.end + 8 <= region.end() {
        // Anything that is not a version 2 header ends the list.
        runs.push((plan.end, vec![0xFF; 8]));
    }

    // Overlay the runs page by page.
    let ps = flash.page_size();
    let mut pages: BTreeMap<u64, (Vec<u8>, Vec<bool>)> = BTreeMap::new();
    for (address, bytes) in &runs {
        for (i, b) in bytes.iter().enumerate() {
            let a = address + i as u64;
            let page = a / ps * ps;
            let entry = pages
                .entry(page)
                .or_insert_with(|| (vec![0xFF; ps as usize], vec![false; ps as usize]));
            entry.0[(a - page) as usize] = *b;
            entry.1[(a - page) as usize] = true;
        }
    }

    let mut writes = Vec::new();
    for (page, (mut data, covered)) in pages {
        let current = flash.page(page);
        if covered.iter().any(|c| !c) {
            match current {
                Some(cur) => {
                    for (i, c) in covered.iter().enumerate() {
                        if !c {
                            data[i] = cur[i];
                        }
                    }
                }
                None => {
                    need.push(page);
                    continue;
                }
            }
        }
        if current != Some(data.as_slice()) {
            writes.push(PageWrite {
                address: page,
                data,
            });
        }
    }
    if !need.is_empty() {
        need.sort();
        need.dedup();
        return Err(NeedRead { pages: need });
    }
    Ok(writes)
}
