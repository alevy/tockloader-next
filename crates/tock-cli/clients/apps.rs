//! What our app-changing subcommands share.
//!
//! A module, not a crate: `tock-list`, `tock-install` and `tock-uninstall`
//! each `#[path]` it in. The split that matters is the one between this and
//! [`tock_client`] -- that crate is the contract every subcommand has with
//! `tock`, whatever it does, and none of this is. A `tock-listen` or a
//! `tock-tickv` needs a connection and a board; it does not need a planner.
//!
//! Only what is genuinely shared lives here. `load_tab` does not: `install`
//! is the only command that reads a TAB.
//!
//! The cost of a module over a crate: each binary compiles all of it and
//! uses part, so unused warnings here are expected rather than a signal.
#![allow(dead_code)]

use tock_board::Resolved;
use tock_client::{CliError, Code, Session};
use tock_image::{Discovered, Flash, NeedRead, Plan, PlanError, Request, WriteRun};
use tock_proto::Capability;

/// How far apart two reads may be before issuing them separately beats
/// reading the bytes between them.
pub const READ_GAP: u64 = 256;

/// How far apart two changed runs may be before writing them separately
/// beats bridging the bytes between them. For a transport that erases before
/// writing, the answer is one byte less than its erase block; until a
/// transport declares its own, this assumes 4 KiB.
///
/// Both numbers are the same question -- round-trip cost against per-byte
/// cost -- which is why they are side by side. Neither belongs to the
/// planner (it cannot know the transport) or to the protocol (batching is a
/// performance choice, not a rule); they belong to whoever knows the
/// workload, which is this.
pub const WRITE_GAP: u64 = 4095;

/// Run `step` until it stops asking for bytes, fetching what it asks for.
///
/// The planner in [`tock_image`] is pure: when it needs flash it does not
/// have, it says so rather than reading. This is the loop that answers, and
/// the one place reads are merged before they are issued.
pub fn with_reads<T>(
    session: &mut Session,
    flash: &mut Flash,
    mut step: impl FnMut(&Flash) -> Result<T, NeedRead>,
) -> Result<T, CliError> {
    loop {
        match step(flash) {
            Ok(value) => return Ok(value),
            Err(NeedRead { ranges }) => {
                for (address, data) in
                    session.read_ranges(&tock_image::coalesce(&ranges, READ_GAP))?
                {
                    flash.insert(address, &data);
                }
            }
        }
    }
}

/// Walk the app list on the device.
pub fn discover(session: &mut Session, board: &Resolved) -> Result<(Flash, Discovered), CliError> {
    session.require(Capability::MemoryRead, "reading apps")?;
    let mut flash = Flash::new(&board.apps);
    let found = with_reads(session, &mut flash, |f| {
        tock_image::discover(&board.apps, f)
    })?;
    Ok((flash, found))
}

/// Plan a change, make the device match, and check that it did.
///
/// The whole of what `install` and `uninstall` have in common. They differ
/// only in the [`Request`] they build and the arguments they parse;
/// everything from here is the same work, and sharing it is what keeps two
/// separate binaries behaving like one tool.
pub fn apply(
    session: &mut Session,
    board: &Resolved,
    request: &Request,
    dry_run: bool,
) -> Result<Applied, CliError> {
    if !dry_run {
        session.require(Capability::MemoryWrite, "changing apps")?;
    }
    let (mut flash, found) = discover(session, board)?;
    let app_ram = board.app_ram.map(|r| (r.start, r.size));
    let plan = tock_image::plan(&board.apps, &board.placement, app_ram, &found, request)
        .map_err(plan_error)?;
    let writes = with_reads(session, &mut flash, |f| {
        tock_image::render(&board.apps, &plan, f, WRITE_GAP)
    })?;
    if !dry_run {
        session.write_runs(&writes)?;
        verify(session, &writes)?;
    }
    Ok(Applied { plan, writes })
}

/// Read back what was written and compare. Reads coalesce, so one reply can
/// cover several runs: find the one containing each.
fn verify(session: &mut Session, writes: &[WriteRun]) -> Result<(), CliError> {
    let ranges: Vec<(u64, u64)> = writes
        .iter()
        .map(|w| (w.address, w.data.len() as u64))
        .collect();
    let read_back = session.read_ranges(&tock_image::coalesce(&ranges, READ_GAP))?;
    for run in writes {
        let end = run.address + run.data.len() as u64;
        let got = read_back
            .iter()
            .find(|(at, data)| *at <= run.address && at + data.len() as u64 >= end)
            .map(|(at, data)| {
                let offset = (run.address - at) as usize;
                &data[offset..offset + run.data.len()]
            });
        if got != Some(run.data.as_slice()) {
            return Err(CliError::new(
                Code::Verify,
                format!("{:#x} did not verify after writing", run.address),
            ));
        }
    }
    Ok(())
}

/// What a change came to: where things ended up, and what it cost to say so.
pub struct Applied {
    pub plan: Plan,
    pub writes: Vec<WriteRun>,
}

impl Applied {
    /// Say what happened, the way every app-changing subcommand says it.
    /// Shared so two binaries cannot drift into two output formats.
    pub fn report(&self, dry_run: bool) {
        if tock_client::json_output() {
            let runs: Vec<_> = self
                .writes
                .iter()
                .map(|w| serde_json::json!({ "address": w.address, "bytes": w.data.len() }))
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "dry_run": dry_run,
                    "plan": self.plan,
                    "writes": runs,
                })
            );
            return;
        }
        for action in &self.plan.actions {
            println!("{action}");
        }
        let bytes: usize = self.writes.iter().map(|w| w.data.len()).sum();
        let verb = if dry_run { "would" } else { "did" };
        println!(
            "{verb} write {bytes} byte(s) in {} run(s)",
            self.writes.len()
        );
    }
}

/// Turn a planning failure into something with a next step in it.
pub fn plan_error(e: PlanError) -> CliError {
    use PlanError::*;
    let error = CliError::new(Code::Failed, e.to_string());
    match e {
        NeedsRepack { .. } => error
            .hint("rerun with --allow-repack to let tock move other apps")
            .hint("add --dry-run first to see what would move"),
        Sticky { .. } => error.hint("rerun with --include-sticky"),
        NotInstalled { .. } => error.hint("tock list"),
        _ => error,
    }
}
