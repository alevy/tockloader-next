//! What our app-changing subcommands share.
//!
//! A module, not a crate: `tock-list`, `tock-install` and `tock-uninstall`
//! each `#[path]` it in. The split that matters is the one between this and
//! [`tock_client`] -- that crate is the contract every subcommand has with
//! `tock`, whatever it does, and none of this is. A `tock-listen` or a
//! `tock-tickv` needs a connection and a board; it does not need a planner.
//!
//! This is the device-facing loop [`tock_image`] says lives in the caller.
//! That crate is pure: it walks, plans and renders against whatever bytes
//! are known, and asks for more by returning rather than reading. This is
//! what answers -- so the two read as parallel, and are not: there is one
//! walk, one planner and one renderer, each called from here exactly once.
//!
//! Only what is genuinely shared lives here. `load_tab` does not: `install`
//! is the only command that reads a TAB.
//!
//! The cost of a module over a crate: each binary compiles all of it and
//! uses part, so unused warnings here are expected rather than a signal.
#![allow(dead_code)]

use tock_board::Context;
use tock_client::{CliError, Code, proto};
use tock_image::kernel::{self, Attributes};
use tock_image::{Change, Discovered, Flash, NeedRead, Plan, PlanError, Region, WriteRun};
use tock_proto::{Capability, Client, Request};

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
    device: &mut Client,
    flash: &mut Flash,
    mut step: impl FnMut(&Flash) -> Result<T, NeedRead>,
) -> Result<T, CliError> {
    loop {
        match step(flash) {
            Ok(value) => return Ok(value),
            Err(NeedRead { ranges }) => {
                let merged = tock_image::coalesce(&ranges, READ_GAP);
                for (address, data) in device.read_ranges(&merged).map_err(proto)? {
                    flash.insert(address, &data);
                }
            }
        }
    }
}

/// The board as the device actually describes it: what is installed, and
/// what the kernel says about itself.
///
/// Named for the answer rather than the verb, because
/// [`tock_image::discover`] is the verb and this calls it. What it adds is
/// everything that is not the walk: the capability check, the bytes, and the
/// kernel's own account of the board, which takes the board file's place
/// wherever it has something to say.
pub struct Survey {
    /// The resolved context, corrected by anything the kernel reported. Plan
    /// against this, not against what `tock` handed over: a board file is a
    /// declaration, and this is the device in front of us.
    pub context: Context,
    /// What the kernel said, where there was a kernel to ask.
    pub kernel: Option<Attributes>,
    pub flash: Flash,
    pub found: Discovered,
}

impl Survey {
    /// Where `app_ram` came from, for a caller that reports it.
    pub fn app_ram_from(&self) -> &'static str {
        match self.kernel.as_ref().map(|k| k.app_ram) {
            Some(Some(_)) => "kernel",
            _ => "board file",
        }
    }
}

/// Ask the device what it is, then what is on it.
pub fn survey(device: &mut Client, ctx: &Context) -> Result<Survey, CliError> {
    device
        .require(Capability::MemoryRead, "reading apps")
        .map_err(proto)?;

    // The attribute table ends where the app region begins, so the two reads
    // abut: issue them as one request and both walks find their bytes
    // already known. Without `flash` we do not know how far down we may
    // read, and an access outside it is refused by the transport and fails
    // the command -- so in that case we do not look.
    let window = ctx
        .flash
        .and_then(|flash| kernel::window(ctx.apps.start, flash.start));
    let span = window.unwrap_or(Region {
        start: ctx.apps.start,
        size: 0,
    });
    let mut flash = Flash::new();
    for (address, data) in device
        .read_ranges(&[(span.start, span.size + tock_image::HEADER_PROBE)])
        .map_err(proto)?
    {
        flash.insert(address, &data);
    }

    let kernel = match window {
        Some(_) => with_reads(device, &mut flash, |f| {
            kernel::attributes(ctx.apps.start, span.start, f)
        })?,
        None => None,
    };

    let mut context = ctx.clone();
    // The kernel's figure replaces the board file's. The board file declares
    // the whole of RAM; the kernel has already taken its own data out of the
    // bottom, and how much it took is a fact about this build.
    if let Some(app_ram) = kernel.as_ref().and_then(|k| k.app_ram) {
        context.app_ram = Some(app_ram);
    }

    let found = with_reads(device, &mut flash, |f| tock_image::discover(&ctx.apps, f))?;
    Ok(Survey {
        context,
        kernel,
        flash,
        found,
    })
}

/// Plan a change, make the device match, and check that it did.
///
/// The whole of what `install` and `uninstall` have in common. They differ
/// only in the [`Change`] they build and the arguments they parse;
/// everything from here is the same work, and sharing it is what keeps two
/// separate binaries behaving like one tool.
///
/// Takes the [`Survey`] rather than a [`Context`] so that the context it
/// plans against is the one the caller can see.
pub fn apply(
    device: &mut Client,
    survey: &mut Survey,
    change: &Change,
    dry_run: bool,
) -> Result<Applied, CliError> {
    if !dry_run {
        device
            .require(Capability::MemoryWrite, "changing apps")
            .map_err(proto)?;
    }
    // `survey.context`, not whatever the caller was handed: where the kernel
    // contradicted the board file, the kernel is right.
    let ctx = &survey.context;
    let app_ram = ctx.app_ram.map(|r| (r.start, r.size));
    let plan = tock_image::plan(&ctx.apps, &ctx.placement, app_ram, &survey.found, change)
        .map_err(|e| plan_error(e, survey))?;
    let writes = with_reads(device, &mut survey.flash, |f| {
        tock_image::render(&ctx.apps, &plan, f, WRITE_GAP)
    })?;
    if !dry_run {
        write_runs(device, &writes)?;
        verify(device, &writes)?;
    }
    Ok(Applied { plan, writes })
}

/// Send each changed run. A loop, not a discipline -- which is why it is
/// here and not on [`Client`]: `render` already merged these, and erase
/// granularity is the transport's business.
fn write_runs(device: &mut Client, runs: &[WriteRun]) -> Result<(), CliError> {
    for run in runs {
        device
            .call(
                Request::Write {
                    address: run.address,
                },
                run.data.clone(),
            )
            .map_err(proto)?;
    }
    Ok(())
}

/// Read back what was written and compare. Reads coalesce, so one reply can
/// cover several runs: find the one containing each.
fn verify(device: &mut Client, writes: &[WriteRun]) -> Result<(), CliError> {
    let ranges: Vec<(u64, u64)> = writes
        .iter()
        .map(|w| (w.address, w.data.len() as u64))
        .collect();
    let merged = tock_image::coalesce(&ranges, READ_GAP);
    let read_back = device.read_ranges(&merged).map_err(proto)?;
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
///
/// `NoCandidate` is the one that needs the survey: an app rejected for its
/// fixed RAM address was measured against a number, and which number it was
/// -- the board file's declaration or the kernel's own -- is the difference
/// between an error a user can act on and one that just says no.
pub fn plan_error(e: PlanError, survey: &Survey) -> CliError {
    use PlanError::*;
    let error = CliError::new(Code::Failed, e.to_string());
    match e {
        NoCandidate { .. } => match survey.context.app_ram {
            Some(ram) => error.hint(format!(
                "this board's app RAM is {:#x} +{:#x}, according to the {}",
                ram.start,
                ram.size,
                survey.app_ram_from()
            )),
            None => error,
        },
        NeedsRepack { .. } => error
            .hint("rerun with --allow-repack to let tock move other apps")
            .hint("add --dry-run first to see what would move"),
        Sticky { .. } => error.hint("rerun with --include-sticky"),
        NotInstalled { .. } => error.hint("tock list"),
        _ => error,
    }
}
