use serde::Serialize;
use tbf::Header;

use crate::{COALESCE_GAP, Flash, NeedRead, Plan, Region, Slot, coalesce};

/// A run of bytes to write. Not a page: erase geometry belongs to the
/// transport, which is the only thing that knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WriteRun {
    pub address: u64,
    #[serde(skip)]
    pub data: Vec<u8>,
}

/// Turn a plan into the byte runs that differ from what is already there.
///
/// Only bytes the plan defines are considered, and a run whose bytes already
/// match the device is dropped -- which is why this needs to know the current
/// contents, and asks for them when it does not.
pub fn render(region: &Region, plan: &Plan, flash: &Flash) -> Result<Vec<WriteRun>, NeedRead> {
    // Byte runs to write, all computed from the current (pre-write) contents.
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut need = Vec::new();
    for slot in &plan.slots {
        match slot {
            Slot::Keep { .. } | Slot::Padding { existing: true, .. } => {}
            Slot::Write { address, tbf, .. } => runs.push((*address, tbf.content_bytes())),
            Slot::Move { from, to, size, .. } => match flash.read(*from, *size) {
                Ok(bytes) => runs.push((*to, bytes)),
                Err(e) => need.extend(e.ranges),
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

    // Keep only the bytes that differ from what is there now. Asking for the
    // current contents is what makes that possible; it is an optimisation, not
    // a correctness requirement, and the alternative is writing more bytes.
    let mut changed: Vec<(u64, Vec<u8>)> = Vec::new();
    for (address, bytes) in &runs {
        match flash.read(*address, bytes.len() as u64) {
            Ok(current) => {
                let mut at = 0usize;
                while at < bytes.len() {
                    if bytes[at] == current[at] {
                        at += 1;
                        continue;
                    }
                    let from = at;
                    while at < bytes.len() && bytes[at] != current[at] {
                        at += 1;
                    }
                    changed.push((address + from as u64, bytes[from..at].to_vec()));
                }
            }
            // Unknown contents: ask, rather than writing bytes that may
            // already be correct.
            Err(e) => need.extend(e.ranges),
        }
    }

    if !need.is_empty() {
        return Err(NeedRead {
            ranges: coalesce(&need, COALESCE_GAP),
        });
    }

    // One write per contiguous changed region: a transport that erases before
    // writing would otherwise erase the same sector once per run.
    let spans = coalesce(
        &changed
            .iter()
            .map(|(a, d)| (*a, d.len() as u64))
            .collect::<Vec<_>>(),
        COALESCE_GAP,
    );
    let mut writes = Vec::with_capacity(spans.len());
    for (address, len) in spans {
        // A merged span may cover bytes no run touched; they must be preserved.
        let mut data = flash.read(address, len)?;
        for (at, bytes) in &changed {
            if *at >= address && at + bytes.len() as u64 <= address + len {
                let off = (at - address) as usize;
                data[off..off + bytes.len()].copy_from_slice(bytes);
            }
        }
        writes.push(WriteRun { address, data });
    }
    Ok(writes)
}
