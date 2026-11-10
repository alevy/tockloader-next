use serde::Serialize;
use tbf::{Header, Tbf};

use crate::{Alignment, Discovered, Entry, Order, Placement, Region};

/// Smallest gap that can hold a padding header.
const MIN_PADDING: u64 = tbf::BASE_HEADER_SIZE as u64;

/// An app to install, with every TBF the TAB has for the board's
/// architecture. More than one candidate means the app was built for several
/// fixed addresses and the planner picks one.
#[derive(Clone, Debug)]
pub struct NewApp {
    pub name: String,
    pub candidates: Vec<Tbf>,
}

impl NewApp {
    fn is_fixed(&self) -> bool {
        self.candidates
            .iter()
            .any(|t| t.header.fixed_addresses().1.is_some())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Change {
    /// Apps to add. An installed app with the same name is replaced.
    pub install: Vec<NewApp>,
    /// Names of apps to remove.
    pub uninstall: Vec<String>,
    /// Allow moving apps the change did not name, laying out the whole
    /// region from scratch like Python tockloader does.
    pub allow_repack: bool,
    /// Allow removing or replacing sticky apps.
    pub include_sticky: bool,
}

/// One element of the planned app list, in address order.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Slot {
    /// An installed app left exactly where it is.
    Keep {
        address: u64,
        size: u64,
        name: String,
    },
    /// A new TBF written at `address` (already adjusted for that address).
    Write {
        address: u64,
        size: u64,
        name: String,
        #[serde(skip)]
        tbf: Tbf,
    },
    /// An installed app copied to a new address. Only with `allow_repack`.
    Move {
        from: u64,
        to: u64,
        size: u64,
        name: String,
    },
    /// A gap the kernel skips. `existing` means an identical padding header is
    /// already there.
    Padding {
        address: u64,
        size: u64,
        existing: bool,
    },
}

impl Slot {
    pub fn address(&self) -> u64 {
        match self {
            Slot::Keep { address, .. }
            | Slot::Write { address, .. }
            | Slot::Padding { address, .. } => *address,
            Slot::Move { to, .. } => *to,
        }
    }
    pub fn size(&self) -> u64 {
        match self {
            Slot::Keep { size, .. }
            | Slot::Write { size, .. }
            | Slot::Move { size, .. }
            | Slot::Padding { size, .. } => *size,
        }
    }
}

/// What the plan does, for humans and for `--format json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Install {
        name: String,
        address: u64,
        size: u64,
    },
    Replace {
        name: String,
        old_address: u64,
        address: u64,
        size: u64,
    },
    Remove {
        name: String,
        address: u64,
        size: u64,
    },
    Move {
        name: String,
        from: u64,
        to: u64,
        size: u64,
    },
    Pad {
        address: u64,
        size: u64,
    },
    EndList {
        address: u64,
    },
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Install {
                name,
                address,
                size,
            } => {
                write!(f, "install `{name}` at {address:#x} ({size} bytes)")
            }
            Action::Replace {
                name,
                old_address,
                address,
                size,
            } if old_address == address => {
                write!(f, "replace `{name}` at {address:#x} ({size} bytes)")
            }
            Action::Replace {
                name,
                old_address,
                address,
                size,
            } => write!(
                f,
                "replace `{name}` (was at {old_address:#x}) at {address:#x} ({size} bytes)"
            ),
            Action::Remove {
                name,
                address,
                size,
            } => {
                write!(f, "remove `{name}` from {address:#x} ({size} bytes freed)")
            }
            Action::Move {
                name,
                from,
                to,
                size,
            } => {
                write!(f, "move `{name}` from {from:#x} to {to:#x} ({size} bytes)")
            }
            Action::Pad { address, size } => write!(f, "pad {address:#x}..{:#x}", address + size),
            Action::EndList { address } => write!(f, "end app list at {address:#x}"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub slots: Vec<Slot>,
    /// First address after the app list.
    pub end: u64,
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("nothing to do")]
    NothingToDo,
    #[error("no installed app named `{name}` (installed: {})", installed.join(", "))]
    NotInstalled {
        name: String,
        installed: Vec<String>,
    },
    #[error(
        "`{name}` is sticky; it is only removed or replaced when sticky apps are explicitly included"
    )]
    Sticky { name: String },
    #[error("`{name}` has no usable TBF for this board: {reason}")]
    NoCandidate { name: String, reason: String },
    #[error("`{name}` ({size} bytes) does not fit without moving other apps")]
    NeedsRepack { name: String, size: u64 },
    #[error("`{name}` ({size} bytes) does not fit in the app region")]
    DoesNotFit { name: String, size: u64 },
}

/// A placed app, before gaps are filled in.
#[derive(Clone, Debug)]
enum Placed {
    Existing {
        address: u64,
        from: u64,
        header: Header,
    },
    New {
        address: u64,
        name: String,
        tbf: Tbf,
        replaces: Option<u64>,
    },
}

impl Placed {
    fn range(&self) -> (u64, u64) {
        match self {
            Placed::Existing {
                address, header, ..
            } => (*address, *address + header.total_size as u64),
            Placed::New { address, tbf, .. } => (*address, *address + tbf.total_size() as u64),
        }
    }
}

/// An app that still needs an address.
#[derive(Clone, Debug)]
enum Pending {
    Existing { from: u64, header: Header },
    New { app: NewApp, replaces: Option<u64> },
}

impl Pending {
    fn size(&self) -> u64 {
        match self {
            Pending::Existing { header, .. } => header.total_size as u64,
            // Candidates are already rounded; they share a size in practice.
            Pending::New { app, .. } => app
                .candidates
                .iter()
                .map(|t| t.total_size() as u64)
                .max()
                .unwrap_or(0),
        }
    }
    fn is_fixed(&self) -> bool {
        match self {
            Pending::Existing { header, .. } => header.fixed_addresses().1.is_some(),
            Pending::New { app, .. } => app.is_fixed(),
        }
    }
    fn name(&self) -> String {
        match self {
            Pending::Existing { header, .. } => header.package_name().unwrap_or("").to_string(),
            Pending::New { app, .. } => app.name.clone(),
        }
    }
}

/// Decide where every app goes.
///
/// By default apps the change does not name stay exactly where they are:
/// new and replaced apps go into gaps (including space freed by removed apps)
/// or after the last app. With `allow_repack` the whole region is laid out
/// again with Python tockloader's algorithm, which may move apps.
///
/// `app_ram` is the board's RAM region for apps, used to discard TBFs built
/// for fixed RAM addresses the board does not have.
pub fn plan(
    region: &Region,
    placement: &Placement,
    app_ram: Option<(u64, u64)>,
    found: &Discovered,
    change: &Change,
) -> Result<Plan, PlanError> {
    if change.install.is_empty() && change.uninstall.is_empty() {
        return Err(PlanError::NothingToDo);
    }
    let installed: Vec<(u64, &Header)> = found.apps().collect();
    let name_of = |h: &Header| h.package_name().unwrap_or("").to_string();

    for name in &change.uninstall {
        if !installed
            .iter()
            .any(|(_, h)| h.package_name() == Some(name))
        {
            return Err(PlanError::NotInstalled {
                name: name.clone(),
                installed: installed.iter().map(|(_, h)| name_of(h)).collect(),
            });
        }
    }

    let mut install: Vec<NewApp> = Vec::new();
    for app in &change.install {
        install.push(prepare(app, placement, app_ram)?);
    }

    // The resulting list, in the order Python tockloader would use: installed
    // apps in flash order (replacements take their place), then new apps.
    let mut actions = Vec::new();
    let mut pending: Vec<Pending> = Vec::new();
    for (address, header) in &installed {
        let name = name_of(header);
        let replacement = install.iter().position(|a| a.name == name);
        let removing = change.uninstall.contains(&name);
        if (replacement.is_some() || removing) && header.is_sticky() && !change.include_sticky {
            return Err(PlanError::Sticky { name });
        }
        if let Some(i) = replacement {
            pending.push(Pending::New {
                app: install.remove(i),
                replaces: Some(*address),
            });
        } else if removing {
            actions.push(Action::Remove {
                name,
                address: *address,
                size: header.total_size as u64,
            });
        } else {
            pending.push(Pending::Existing {
                from: *address,
                header: (*header).clone(),
            });
        }
    }
    pending.extend(install.into_iter().map(|app| Pending::New {
        app,
        replaces: None,
    }));

    let placed = if change.allow_repack {
        place_repack(region, placement, pending)?
    } else {
        place_stable(region, placement, pending)?
    };

    Ok(finish(region, found, placed, actions))
}

/// Pick usable candidates and apply the size rule.
fn prepare(
    app: &NewApp,
    placement: &Placement,
    app_ram: Option<(u64, u64)>,
) -> Result<NewApp, PlanError> {
    let mut candidates = Vec::new();
    for tbf in &app.candidates {
        if let (Some(ram), Some((start, size))) = (tbf.header.fixed_addresses().0, app_ram) {
            if (ram as u64) < start || (ram as u64) >= start + size {
                continue;
            }
        }
        let mut tbf = tbf.clone();
        let size = placement.size.apply(tbf.total_size() as u64);
        tbf.set_total_size(size as u32);
        candidates.push(tbf);
    }
    if candidates.is_empty() {
        return Err(PlanError::NoCandidate {
            name: app.name.clone(),
            reason: "every TBF is built for a fixed RAM address outside this board's app RAM"
                .into(),
        });
    }
    let fixed = candidates
        .iter()
        .filter(|t| t.header.fixed_addresses().1.is_some())
        .count();
    if fixed == 0 && candidates.len() > 1 {
        candidates.truncate(1);
    }
    Ok(NewApp {
        name: app.name.clone(),
        candidates,
    })
}

fn alignment_for(placement: &Placement, size: u64, fixed: bool) -> u64 {
    match placement.alignment {
        // Fixed-address apps were linked for their address; the build already
        // chose it, as in Python tockloader.
        Alignment::Size if !fixed && size > 0 => size,
        _ => 4,
    }
}

/// Where a fixed-address TBF's header should start: on a 1 KiB boundary
/// below the binary's address, as Python tockloader does.
fn fixed_start(tbf: &Tbf) -> Option<u64> {
    let flash = tbf.header.fixed_addresses().1? as u64;
    let before = tbf.header.size_before_app() as u64;
    let mut start = flash - flash % 1024;
    if before > flash - start {
        start = flash.checked_sub(before)?;
    }
    Some(start)
}

/// Is `[start, start + size)` usable given what is already placed? Gaps left
/// on either side must be empty or big enough for a padding header (the gap
/// after the last app is just the end of the list).
fn fits(region: &Region, placed: &[Placed], start: u64, size: u64) -> bool {
    let end = start + size;
    if start < region.start || end > region.end() {
        return false;
    }
    let mut prev_end = region.start;
    let mut next_start = None;
    for p in placed {
        let (s, e) = p.range();
        if s < end && start < e {
            return false;
        }
        if e <= start {
            prev_end = prev_end.max(e);
        }
        if s >= end {
            next_start = Some(next_start.map_or(s, |n: u64| n.min(s)));
        }
    }
    let lead = start - prev_end;
    let trail_ok = next_start.is_none_or(|n| n - end == 0 || n - end >= MIN_PADDING);
    (lead == 0 || lead >= MIN_PADDING) && trail_ok
}

/// Lowest address at or after `from` where an app fits.
fn first_fit(region: &Region, placed: &[Placed], from: u64, size: u64, align: u64) -> Option<u64> {
    let mut a = from.div_ceil(align) * align;
    while a + size <= region.end() {
        if fits(region, placed, a, size) {
            return Some(a);
        }
        // Skip past whatever is in the way, or step to the next aligned spot.
        let blocker = placed
            .iter()
            .map(Placed::range)
            .filter(|(s, e)| *s < a + size && a < *e)
            .map(|(_, e)| e)
            .max();
        let next = blocker.unwrap_or(a + 1).max(a + 1);
        a = next.div_ceil(align) * align;
    }
    None
}

fn place_new(
    region: &Region,
    placement: &Placement,
    placed: &[Placed],
    app: NewApp,
    replaces: Option<u64>,
    from: u64,
) -> Option<Placed> {
    let mut best: Option<(u64, Tbf)> = None;
    for tbf in &app.candidates {
        let size = tbf.total_size() as u64;
        let address = if tbf.header.fixed_addresses().1.is_some() {
            let s = fixed_start(tbf)?;
            if s < from || !fits(region, placed, s, size) {
                continue;
            }
            s
        } else {
            match first_fit(
                region,
                placed,
                from,
                size,
                alignment_for(placement, size, false),
            ) {
                Some(a) => a,
                None => continue,
            }
        };
        if best.as_ref().is_none_or(|(b, _)| address < *b) {
            best = Some((address, tbf.clone()));
        }
    }
    let (address, mut tbf) = best?;
    tbf.place_fixed_at(address as u32).ok()?;
    Some(Placed::New {
        address,
        name: app.name,
        tbf,
        replaces,
    })
}

fn size_order(placement: &Placement, items: &mut [Pending]) {
    if placement.order == Order::SizeDescending {
        items.sort_by_key(|p| std::cmp::Reverse(p.size()));
    }
}

fn place_stable(
    region: &Region,
    placement: &Placement,
    pending: Vec<Pending>,
) -> Result<Vec<Placed>, PlanError> {
    let mut placed = Vec::new();
    let mut new = Vec::new();
    for p in pending {
        match p {
            Pending::Existing { from, header } => placed.push(Placed::Existing {
                address: from,
                from,
                header,
            }),
            n => new.push(n),
        }
    }
    // Most constrained first: fixed-address apps, then largest first if the
    // board wants that.
    size_order(placement, &mut new);
    new.sort_by_key(|p| !p.is_fixed());
    for p in new {
        let (name, size) = (p.name(), p.size());
        let Pending::New { app, replaces } = p else {
            unreachable!()
        };
        let Some(spot) = place_new(region, placement, &placed, app, replaces, region.start) else {
            let used: u64 = placed.iter().map(|p| p.range().1 - p.range().0).sum();
            return Err(if used + size <= region.size {
                PlanError::NeedsRepack { name, size }
            } else {
                PlanError::DoesNotFit { name, size }
            });
        };
        placed.push(spot);
    }
    Ok(placed)
}

/// Python tockloader's layout: fixed-address apps where they must be, then
/// everything else packed from the start of the region in board order, each
/// aligned as required. Unlike Python, a mix of fixed and position-independent
/// apps is allowed.
fn place_repack(
    region: &Region,
    placement: &Placement,
    pending: Vec<Pending>,
) -> Result<Vec<Placed>, PlanError> {
    let mut placed = Vec::new();
    let mut movable = Vec::new();
    let mut fixed_new = Vec::new();
    for p in pending {
        match p {
            // Installed fixed-address apps cannot move.
            Pending::Existing { from, header } if header.fixed_addresses().1.is_some() => placed
                .push(Placed::Existing {
                    address: from,
                    from,
                    header,
                }),
            p if p.is_fixed() => fixed_new.push(p),
            p => movable.push(p),
        }
    }
    for p in fixed_new {
        let (name, size) = (p.name(), p.size());
        let Pending::New { app, replaces } = p else {
            unreachable!()
        };
        placed.push(
            place_new(region, placement, &placed, app, replaces, region.start)
                .ok_or(PlanError::DoesNotFit { name, size })?,
        );
    }
    size_order(placement, &mut movable);
    let mut cursor = region.start;
    for p in movable {
        let (name, size) = (p.name(), p.size());
        let align = alignment_for(placement, size, false);
        let address = first_fit(region, &placed, cursor, size, align)
            .ok_or(PlanError::DoesNotFit { name, size })?;
        cursor = address + size;
        placed.push(match p {
            Pending::Existing { from, header } => Placed::Existing {
                address,
                from,
                header,
            },
            Pending::New { app, replaces } => {
                let mut tbf = app.candidates[0].clone();
                tbf.place_fixed_at(address as u32).ok();
                Placed::New {
                    address,
                    name: app.name,
                    tbf,
                    replaces,
                }
            }
        });
    }
    Ok(placed)
}

/// Fill gaps with padding, and describe the result.
fn finish(
    region: &Region,
    found: &Discovered,
    mut placed: Vec<Placed>,
    mut actions: Vec<Action>,
) -> Plan {
    placed.sort_by_key(|p| p.range().0);
    let existing_padding = |address: u64, size: u64| {
        found.entries.iter().any(
            |e| matches!(e, Entry::Padding { address: a, size: s } if *a == address && *s == size),
        )
    };
    let mut slots = Vec::new();
    let mut cursor = region.start;
    for p in placed {
        let (start, end) = p.range();
        if start > cursor {
            let size = start - cursor;
            let existing = existing_padding(cursor, size);
            if !existing {
                actions.push(Action::Pad {
                    address: cursor,
                    size,
                });
            }
            slots.push(Slot::Padding {
                address: cursor,
                size,
                existing,
            });
        }
        let size = end - start;
        slots.push(match p {
            Placed::Existing {
                address,
                from,
                header,
            } => {
                let name = header.package_name().unwrap_or("").to_string();
                if address == from {
                    Slot::Keep {
                        address,
                        size,
                        name,
                    }
                } else {
                    actions.push(Action::Move {
                        name: name.clone(),
                        from,
                        to: address,
                        size,
                    });
                    Slot::Move {
                        from,
                        to: address,
                        size,
                        name,
                    }
                }
            }
            Placed::New {
                address,
                name,
                tbf,
                replaces,
            } => {
                actions.push(match replaces {
                    Some(old_address) => Action::Replace {
                        name: name.clone(),
                        old_address,
                        address,
                        size,
                    },
                    None => Action::Install {
                        name: name.clone(),
                        address,
                        size,
                    },
                });
                Slot::Write {
                    address,
                    size,
                    name,
                    tbf,
                }
            }
        });
        cursor = end;
    }
    if cursor < region.end() {
        actions.push(Action::EndList { address: cursor });
    }
    Plan {
        slots,
        end: cursor,
        actions,
    }
}
