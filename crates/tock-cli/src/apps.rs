//! The built-in clients: `list`, `install`, `uninstall`.
//!
//! Each one plans against a local image with [`tock_image`] and then drives a
//! connection to make the device match. The loop that fetches bytes the
//! planner asks for lives here too, in [`with_reads`].

use std::path::{Path, PathBuf};

use serde_json::json;
use tock_board::Context;
use tock_image::{Discovered, Flash, NeedRead, NewApp, Request};
use tock_proto::Capability;

use crate::cli::{ChangeArgs, Format, Global};
use crate::config::{load_context, load_project};
use crate::device::Device;
use crate::error::{CliError, Code};

/// Run `step` until it stops asking for bytes, reading them from the device.
pub(crate) fn with_reads<T>(
    device: &mut Device,
    flash: &mut Flash,
    mut step: impl FnMut(&Flash) -> Result<T, NeedRead>,
) -> Result<T, CliError> {
    loop {
        match step(flash) {
            Ok(v) => return Ok(v),
            Err(NeedRead { ranges }) => {
                for (address, data) in device.read_ranges(&ranges)? {
                    flash.insert(address, &data);
                }
            }
        }
    }
}

pub(crate) fn discover(
    ctx: &Context,
    device: &mut Device,
) -> Result<(Flash, Discovered), CliError> {
    device.require(Capability::MemoryRead, "reading apps")?;
    let region = ctx.board.app_region();
    let mut flash = Flash::new(&region);
    let found = with_reads(device, &mut flash, |f| tock_image::discover(&region, f))?;
    Ok((flash, found))
}

pub(crate) fn list(g: &Global) -> Result<(), CliError> {
    let ctx = load_context(g)?;
    let mut device = Device::connect(&ctx, g.trace_protocol)?;
    if device.capabilities.contains(&Capability::Apps)
        && !device.capabilities.contains(&Capability::MemoryRead)
    {
        return Err(CliError::new(
            Code::Capability,
            "app-level transports are not wired up in the spike yet",
        ));
    }
    let (_, found) = discover(&ctx, &mut device)?;
    let region = ctx.board.app_region();
    let apps: Vec<_> = found
        .apps()
        .map(|(address, h)| {
            json!({
                "name": h.package_name().unwrap_or(""),
                "address": address,
                "size": h.total_size,
                "version": h.app_version(),
                "enabled": h.is_enabled(),
                "sticky": h.is_sticky(),
                "fixed_flash": h.fixed_addresses().1,
                "checksum_ok": h.checksum_ok(),
            })
        })
        .collect();
    if g.format == Format::Json {
        println!(
            "{}",
            json!({ "board": ctx.board.board.name, "apps": apps, "end": found.end(&region) })
        );
        return Ok(());
    }
    if apps.is_empty() {
        println!("no apps installed");
    }
    for a in &apps {
        let mut flags = vec![];
        if !a["enabled"].as_bool().unwrap() {
            flags.push("disabled");
        }
        if a["sticky"].as_bool().unwrap() {
            flags.push("sticky");
        }
        if !a["checksum_ok"].as_bool().unwrap() {
            flags.push("BAD CHECKSUM");
        }
        println!(
            "{:<20} {:#010x}  {:>7} bytes  {}",
            a["name"].as_str().unwrap(),
            a["address"].as_u64().unwrap(),
            a["size"].as_u64().unwrap(),
            flags.join(", ")
        );
    }
    Ok(())
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
        let project = load_project()?;
        let Some((origin, p)) = project.filter(|(_, p)| !p.apps.is_empty()) else {
            return Err(CliError::new(
                Code::Failed,
                "no TABs given and no [[app]] entries in a Tock.toml",
            )
            .hint("tock install path/to/app.tab")
            .hint("or list apps in Tock.toml: [[app]] tab = \"build/app.tab\""));
        };
        let dir = Path::new(&origin)
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        tabs = p.apps.iter().map(|a| dir.join(&a.tab)).collect();
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
    let mut device = Device::connect(ctx, g.trace_protocol)?;
    if !change.dry_run {
        device.require(Capability::MemoryWrite, "changing apps")?;
    }
    let (mut flash, found) = discover(ctx, &mut device)?;
    let plan = tock_image::plan(&region, placement, ctx.board.app_ram(), &found, &request)
        .map_err(plan_error)?;
    let write_gap = device.write_gap();
    let writes = with_reads(&mut device, &mut flash, |f| {
        tock_image::render(&region, &plan, f, write_gap)
    })?;

    if !change.dry_run {
        device.write_runs(&writes)?;
        // Verify by reading back. Reads coalesce, so a reply can cover several
        // runs; find the one that contains each and compare that slice.
        let ranges: Vec<(u64, u64)> = writes
            .iter()
            .map(|w| (w.address, w.data.len() as u64))
            .collect();
        let read_back = device.read_ranges(&ranges)?;
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
