//! The built-in clients: `install` and `uninstall`.
//!
//! `list` is no longer here: it is `tock-list`, reached the same way a
//! third-party subcommand is. These two are next.
//!
//! Each one plans against a local image with [`tock_image`] and then drives a
//! connection to make the device match. The loop that fetches bytes the
//! planner asks for lives here too, in [`with_reads`].

use std::path::{Path, PathBuf};

use serde_json::json;
use tock_board::Context;
use tock_client::{CliError, Code, Session};
use tock_image::{Discovered, Flash, NewApp, Request};
use tock_proto::Capability;

use crate::cli::{ChangeArgs, Format, Global};
use crate::config::{load_context, project_apps};
use crate::device;

pub(crate) fn discover(
    ctx: &Context,
    session: &mut Session,
) -> Result<(Flash, Discovered), CliError> {
    session.require(Capability::MemoryRead, "reading apps")?;
    let region = ctx.board.app_region();
    let mut flash = Flash::new(&region);
    let found = session.with_reads(&mut flash, |f| tock_image::discover(&region, f))?;
    Ok((flash, found))
}

pub(crate) fn request(change: &ChangeArgs) -> Request {
    Request {
        allow_repack: change.allow_repack,
        include_sticky: change.include_sticky,
        ..Default::default()
    }
}

pub(crate) fn install(
    g: &Global,
    mut tabs: Vec<PathBuf>,
    change: ChangeArgs,
) -> Result<(), CliError> {
    let ctx = load_context(g)?;
    if tabs.is_empty() {
        tabs = project_apps()?;
        if tabs.is_empty() {
            return Err(CliError::new(
                Code::Failed,
                "no TABs given and no [[app]] entries in a Tock.toml",
            )
            .hint("tock install path/to/app.tab")
            .hint("or list apps in Tock.toml: [[app]] tab = \"build/app.tab\""));
        }
    }
    let mut apps = Vec::new();
    for path in &tabs {
        apps.push(load_tab(&ctx, path)?);
    }
    change_apps(
        g,
        &ctx,
        Request {
            install: apps,
            ..request(&change)
        },
        &change,
    )
}

pub(crate) fn load_tab(ctx: &Context, path: &Path) -> Result<NewApp, CliError> {
    let shown = path.display();
    let bytes =
        std::fs::read(path).map_err(|e| CliError::new(Code::Failed, format!("{shown}: {e}")))?;
    let tab = tbf::Tab::parse(&bytes)
        .map_err(|e| CliError::new(Code::Failed, format!("{shown}: {e}")))?;
    let board = &ctx.board.board;
    if let Some(only) = tab.only_for_boards() {
        if !only.contains(&board.name) {
            return Err(CliError::new(
                Code::Failed,
                format!("{shown} is only for boards: {}", only.join(", ")),
            ));
        }
    }
    let tbfs = tab
        .tbfs_for_arch(&board.arch)
        .map_err(|e| CliError::new(Code::Failed, format!("{shown}: {e}")))?;
    if tbfs.is_empty() {
        return Err(CliError::new(
            Code::Failed,
            format!(
                "{shown} has no build for `{}` (it has: {})",
                board.arch,
                tab.architectures().join(", ")
            ),
        ));
    }
    let name = tab
        .name()
        .map(str::to_string)
        .or_else(|| tbfs[0].tbf.header.package_name().map(str::to_string))
        .unwrap_or_else(|| shown.to_string());
    Ok(NewApp {
        name,
        candidates: tbfs.into_iter().map(|t| t.tbf).collect(),
    })
}

pub(crate) fn uninstall(
    g: &Global,
    names: Vec<String>,
    change: ChangeArgs,
) -> Result<(), CliError> {
    let request = Request {
        uninstall: names,
        ..request(&change)
    };
    let ctx = load_context(g)?;
    change_apps(g, &ctx, request, &change)
}

pub(crate) fn change_apps(
    g: &Global,
    ctx: &Context,
    request: Request,
    change: &ChangeArgs,
) -> Result<(), CliError> {
    let region = ctx.board.app_region();
    let placement = &ctx.board.layout.placement;
    let mut session = device::open(ctx, g.trace_protocol)?;
    if !change.dry_run {
        session.require(Capability::MemoryWrite, "changing apps")?;
    }
    let (mut flash, found) = discover(ctx, &mut session)?;
    let plan = tock_image::plan(&region, placement, ctx.board.app_ram(), &found, &request)
        .map_err(plan_error)?;
    let write_gap = device::WRITE_GAP;
    let writes = session.with_reads(&mut flash, |f| {
        tock_image::render(&region, &plan, f, write_gap)
    })?;

    if !change.dry_run {
        session.write_runs(&writes)?;
        // Verify by reading back. Reads coalesce, so a reply can cover several
        // runs; find the one that contains each and compare that slice.
        let ranges: Vec<(u64, u64)> = writes
            .iter()
            .map(|w| (w.address, w.data.len() as u64))
            .collect();
        let read_back = session.read_ranges(&ranges)?;
        for w in &writes {
            let end = w.address + w.data.len() as u64;
            let got = read_back
                .iter()
                .find(|(at, data)| *at <= w.address && at + data.len() as u64 >= end)
                .map(|(at, data)| {
                    let off = (w.address - at) as usize;
                    &data[off..off + w.data.len()]
                });
            if got != Some(w.data.as_slice()) {
                return Err(CliError::new(
                    Code::Verify,
                    format!("{:#x} did not verify after writing", w.address),
                ));
            }
        }
    }

    if g.format == Format::Json {
        let runs: Vec<_> = writes
            .iter()
            .map(|w| json!({ "address": w.address, "bytes": w.data.len() }))
            .collect();
        println!(
            "{}",
            json!({ "dry_run": change.dry_run, "plan": plan, "writes": runs })
        );
    } else {
        let verb = if change.dry_run { "would" } else { "did" };
        for a in &plan.actions {
            println!("{a}");
        }
        let bytes: usize = writes.iter().map(|w| w.data.len()).sum();
        println!("{verb} write {} byte(s) in {} run(s)", bytes, writes.len());
    }
    Ok(())
}

pub(crate) fn plan_error(e: tock_image::PlanError) -> CliError {
    use tock_image::PlanError::*;
    let err = CliError::new(Code::Failed, e.to_string());
    match e {
        NeedsRepack { .. } => err
            .hint("rerun with --allow-repack to let tock move other apps")
            .hint("add --dry-run first to see what would move"),
        Sticky { .. } => err.hint("rerun with --include-sticky"),
        NotInstalled { .. } => err.hint("tock list"),
        _ => err,
    }
}
