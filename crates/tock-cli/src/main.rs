//! `tock`: the core command.

mod device;
mod error;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde_json::json;
use tock_board::{Context, FileSource, Inputs, Layer, ProjectFile, UserConfig};
use tock_image::{Discovered, Flash, NeedRead, NewApp, Request};
use tock_proto::Capability;

use device::Device;
use error::{CliError, Code};

#[derive(Parser)]
#[command(
    name = "tock",
    version,
    about = "Install and manage apps on Tock boards",
    allow_external_subcommands = true
)]
struct Cli {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Global {
    /// Bundled board name or path to a board file.
    #[arg(long, global = true)]
    board: Option<String>,
    /// Transport to reach the board with (e.g. file, openocd).
    #[arg(long, global = true)]
    transport: Option<String>,
    /// Device alias from the user config, or a transport-specific selector.
    #[arg(long, global = true)]
    device: Option<String>,
    /// Transport option, `key=value`. Repeatable.
    #[arg(long = "opt", global = true, value_parser = parse_kv)]
    options: Vec<(String, String)>,
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Human)]
    format: Format,
    /// Log every protocol message to stderr.
    #[arg(long, global = true)]
    trace_protocol: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Format {
    Human,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// List the apps on the board.
    List,
    /// Install apps (TAB files). With no TABs, installs the project's apps.
    Install {
        tabs: Vec<PathBuf>,
        #[command(flatten)]
        change: ChangeArgs,
    },
    /// Remove apps by name.
    Uninstall {
        #[arg(required = true)]
        names: Vec<String>,
        #[command(flatten)]
        change: ChangeArgs,
    },
    /// Show the resolved configuration and where each value came from.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Serve the device protocol on stdio, connected to the resolved
    /// transport. For subcommands written in any language.
    Broker,
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum ConfigCommand {
    Show,
}

#[derive(Args)]
struct ChangeArgs {
    /// Show the plan without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Allow moving apps that were not named, to make room.
    #[arg(long)]
    allow_repack: bool,
    /// Allow removing or replacing sticky apps.
    #[arg(long)]
    include_sticky: bool,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got `{s}`"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = cli.global.format;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if format == Format::Json {
                println!(
                    "{}",
                    json!({ "error": e.message, "hints": e.hints, "code": e.code as u8 })
                );
            } else {
                eprintln!("error: {}", e.message);
                for h in &e.hints {
                    eprintln!("  try: {h}");
                }
            }
            ExitCode::from(e.code as u8)
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let g = cli.global;
    match cli.command {
        Command::Config {
            command: ConfigCommand::Show,
        } => config_show(&g),
        Command::List => list(&g),
        Command::Install { tabs, change } => install(&g, tabs, change),
        Command::Uninstall { names, change } => {
            let request = Request {
                uninstall: names,
                ..request(&change)
            };
            let ctx = load_context(&g)?;
            change_apps(&g, &ctx, request, &change)
        }
        Command::Broker => broker(&g),
        Command::External(args) => external(&g, args),
    }
}

// ---------------------------------------------------------------------------
// Configuration

struct Fs;

impl FileSource for Fs {
    fn read(
        &self,
        reference: &str,
        relative_to: Option<&str>,
    ) -> Result<(String, String), tock_board::Error> {
        let mut path = PathBuf::from(reference);
        if path.is_relative() {
            if let Some(dir) = relative_to.and_then(|r| Path::new(r).parent()) {
                path = dir.join(path);
            }
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| tock_board::Error::Read(path.display().to_string(), e.to_string()))?;
        Ok((path.display().to_string(), text))
    }
}

/// `TOCK_PROJECT` names the project file outright, the way `TOCK_CONFIG` does
/// for the user config; a value that is not a file means there is no project
/// file, which is how a test or a CI job opts out of the walk up.
fn find_project() -> Option<PathBuf> {
    if let Some(pinned) = std::env::var_os("TOCK_PROJECT") {
        return Some(PathBuf::from(pinned)).filter(|p| p.is_file());
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("Tock.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn user_config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TOCK_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("tock").join("config.toml"))
}

fn load_project() -> Result<Option<(String, ProjectFile)>, CliError> {
    let Some(path) = find_project() else {
        return Ok(None);
    };
    let origin = path.display().to_string();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| CliError::new(Code::Config, format!("{origin}: {e}")))?;
    Ok(Some((origin.clone(), ProjectFile::parse(&origin, &text)?)))
}

fn load_context(g: &Global) -> Result<Context, CliError> {
    let project = load_project()?;
    let user = match user_config_path().filter(|p| p.is_file()) {
        Some(p) => {
            let origin = p.display().to_string();
            let text = std::fs::read_to_string(&p)
                .map_err(|e| CliError::new(Code::Config, format!("{origin}: {e}")))?;
            Some((origin.clone(), UserConfig::parse(&origin, &text)?))
        }
        None => None,
    };
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let inputs = Inputs {
        flags: Layer {
            board: g.board.clone(),
            transport: g.transport.clone(),
            device: g.device.clone(),
            options: g.options.clone(),
        },
        env: Layer {
            board: env("TOCK_BOARD"),
            transport: env("TOCK_TRANSPORT"),
            device: env("TOCK_DEVICE"),
            // `vars_os`, not `vars`: the latter panics on any variable in the
            // environment that is not valid Unicode, including ones that have
            // nothing to do with us.
            options: std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .filter(|(_, v)| !v.is_empty())
                .filter_map(|(k, v)| Some((tock_board::env_option_key(&k)?, v)))
                .collect(),
        },
        project: project.as_ref().map(|(o, p)| (o.clone(), p)),
        user: user.as_ref().map(|(o, u)| (o.clone(), u)),
    };
    Ok(tock_board::resolve(&inputs, &Fs)?)
}

fn config_show(g: &Global) -> Result<(), CliError> {
    let ctx = load_context(g)?;
    if g.format == Format::Json {
        println!("{}", serde_json::to_string_pretty(&ctx).unwrap());
        return Ok(());
    }
    let b = &ctx.board;
    let l = &b.layout;
    println!("board      {}  ({})", b.board.name, ctx.board_file.value);
    println!("           from {}", ctx.board_file.source);
    println!("arch       {}", b.board.arch);
    println!("flash      {:#x} +{:#x}", l.flash.start.0, l.flash.size.0);
    println!("apps       {:#x} +{:#x}", l.apps.start.0, l.apps.size.0);
    println!("placement  {:?}", l.placement);
    println!("transport  {}", ctx.transport.name.value);
    println!("           from {}", ctx.transport.name.source);
    for (k, v) in &ctx.transport.options {
        println!("  {k} = {}    ({})", v.value, v.source);
    }
    match &ctx.device {
        Some(d) => println!("device     {}  (from {})", d.value, d.source),
        None => println!("device     (none)"),
    }
    if let Some(p) = &ctx.project {
        println!("project    {p}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// App management

/// Run `step` until it stops asking for bytes, reading them from the device.
fn with_reads<T>(
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

fn discover(ctx: &Context, device: &mut Device) -> Result<(Flash, Discovered), CliError> {
    device.require(Capability::MemoryRead, "reading apps")?;
    let region = ctx.board.app_region();
    let mut flash = Flash::new(&region);
    let found = with_reads(device, &mut flash, |f| tock_image::discover(&region, f))?;
    Ok((flash, found))
}

fn list(g: &Global) -> Result<(), CliError> {
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
            json!({ "board": ctx.board.board.name, "apps": apps, "end": found.end, "list_end": found.how })
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

fn request(change: &ChangeArgs) -> Request {
    Request {
        allow_repack: change.allow_repack,
        include_sticky: change.include_sticky,
        ..Default::default()
    }
}

fn install(g: &Global, mut tabs: Vec<PathBuf>, change: ChangeArgs) -> Result<(), CliError> {
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

fn load_tab(ctx: &Context, path: &Path) -> Result<NewApp, CliError> {
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

fn change_apps(
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
    let writes = with_reads(&mut device, &mut flash, |f| {
        tock_image::render(&region, &plan, f)
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

fn plan_error(e: tock_image::PlanError) -> CliError {
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

// ---------------------------------------------------------------------------
// Broker and external subcommands

/// Answer Hello and Open ourselves (the broker has already opened the
/// transport with the resolved context) and forward everything else.
fn broker(g: &Global) -> Result<(), CliError> {
    let ctx = load_context(g)?;
    struct Broker {
        device: Device,
        board: String,
        flash: tock_proto::FlashInfo,
    }
    impl Broker {
        /// External subcommands reach the device through here, so this is
        /// where their requests are checked. The transport cannot do it as
        /// well: it is never told which board it is talking to, so it could
        /// not say which one the address is outside of.
        fn check(&self, request: &tock_proto::Request, payload: &[u8]) -> Option<String> {
            let (address, len) = match *request {
                tock_proto::Request::Read { address, len } => (address, len),
                tock_proto::Request::Write { address } => (address, payload.len() as u64),
                _ => return None,
            };
            let (start, size) = (self.flash.start, self.flash.size);
            let board = &self.board;
            if address < start || address + len > start + size {
                return Some(format!(
                    "{address:#x}+{len} is outside flash on {board} ({start:#x} +{size:#x})"
                ));
            }
            None
        }
    }
    impl tock_proto::Handler for Broker {
        fn handle(
            &mut self,
            request: tock_proto::Request,
            payload: Vec<u8>,
        ) -> (tock_proto::Response, Vec<u8>) {
            use tock_proto::{Request as Rq, Response as Rs};
            if let Some(problem) = self.check(&request, &payload) {
                return (
                    Rs::error(tock_proto::ErrorKind::InvalidRequest, problem),
                    vec![],
                );
            }
            match request {
                Rq::Hello { .. } => (
                    Rs::Hello {
                        protocol: tock_proto::PROTOCOL_VERSION,
                        transport: self.device.transport.clone(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        capabilities: self.device.capabilities.clone(),
                    },
                    vec![],
                ),
                Rq::Open { .. } | Rq::Close => (Rs::Ok, vec![]),
                other => self.device.forward(&other, &payload).unwrap_or_else(|e| {
                    (
                        Rs::error(tock_proto::ErrorKind::Device, e.to_string()),
                        vec![],
                    )
                }),
            }
        }
    }
    let layout = &ctx.board.layout;
    let mut broker = Broker {
        board: ctx.board.board.name.clone(),
        flash: tock_proto::FlashInfo {
            start: layout.flash.start.0,
            size: layout.flash.size.0,
        },
        device: Device::connect(&ctx, g.trace_protocol)?,
    };
    tock_proto::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &mut broker,
    )
    .map_err(device::proto)
}

pub fn find_executable(name: &str) -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(name)));
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(name));
    beside.into_iter().chain(on_path).find(|p| p.is_file())
}

/// `tock foo ...` runs `tock-foo ...` with the resolved context in
/// `TOCK_CONTEXT` (JSON), if it resolves.
fn external(g: &Global, args: Vec<String>) -> Result<(), CliError> {
    let name = &args[0];
    let exe = format!("tock-{name}");
    let path = find_executable(&exe).ok_or_else(|| {
        CliError::new(
            Code::Failed,
            format!("unknown command `{name}`, and no `{exe}` next to tock or on PATH"),
        )
        .hint("tock --help")
    })?;
    let mut cmd = std::process::Command::new(path);
    cmd.args(&args[1..]);
    match load_context(g) {
        Ok(ctx) => {
            cmd.env("TOCK_CONTEXT", serde_json::to_string(&ctx).unwrap());
        }
        Err(e) => {
            cmd.env("TOCK_CONTEXT_ERROR", e.message);
        }
    }
    // Pass global flags through so the subcommand can run `tock broker`.
    for (flag, v) in [
        ("--board", &g.board),
        ("--transport", &g.transport),
        ("--device", &g.device),
    ] {
        if let Some(v) = v {
            cmd.env(
                format!("TOCK_{}", flag.trim_start_matches("--").to_uppercase()),
                v,
            );
        }
    }
    let status = cmd
        .status()
        .map_err(|e| CliError::new(Code::Failed, format!("{exe}: {e}")))?;
    std::process::exit(status.code().unwrap_or(1));
}
