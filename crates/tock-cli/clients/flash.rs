//! `tock-flash`: write a raw binary straight to flash.
//!
//! No format, no planner, no opinion about what the bytes are -- usually a
//! kernel, and the whole point is that this does not need to know. Where
//! `tock install` reads a TAB, works out where an app can go and writes only
//! what differs, this writes what it is given where it is told.
//!
//! The default address is the start of the board's flash, which is where a
//! kernel goes. Anywhere else has to be said outright, because an address
//! typed by hand is the only thing standing between a kernel image and the
//! middle of someone's apps.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tock_client::{CliError, Code, connect, context, finish, proto};
use tock_proto::{Capability, Client, Request};

#[derive(Parser)]
#[command(name = "tock-flash", about = "Write a raw binary to flash")]
struct Args {
    /// The binary to write. Raw bytes, not an ELF and not a TAB.
    image: PathBuf,
    /// Where to write it. Defaults to the start of the board's flash.
    #[arg(long, value_parser = parse_address)]
    address: Option<u64>,
    /// Write without reading back to check.
    #[arg(long)]
    no_verify: bool,
}

/// An address as anyone writes one: `0x08000000`, or plain decimal.
fn parse_address(given: &str) -> Result<u64, String> {
    let given = given.replace('_', "");
    let parsed = match given
        .strip_prefix("0x")
        .or_else(|| given.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => given.parse(),
    };
    parsed.map_err(|_| format!("expected an address like 0x08000000, got `{given}`"))
}

fn main() -> ExitCode {
    finish(run(&Args::parse()))
}

fn run(args: &Args) -> Result<(), CliError> {
    let ctx = context()?;
    let shown = args.image.display();
    let image = std::fs::read(&args.image)
        .map_err(|e| CliError::new(Code::Failed, format!("{shown}: {e}")))?;
    if image.is_empty() {
        return Err(CliError::new(Code::Failed, format!("{shown} is empty")));
    }

    // The start of flash is where a kernel goes, and is the only default
    // that is a fact rather than a guess.
    let address = match (args.address, ctx.flash) {
        (Some(address), _) => address,
        (None, Some(flash)) => flash.start,
        (None, None) => {
            return Err(CliError::new(
                Code::Config,
                "no address given, and this board's flash start is not known",
            )
            .hint("tock flash --address 0x08000000 <image>"));
        }
    };

    // The transport refuses an access outside the flash it was opened for,
    // which is the check that matters because it is the one nothing can go
    // around. This one is here only to fail before writing anything, and to
    // say by how much.
    if let Some(flash) = ctx.flash {
        let end = address + image.len() as u64;
        if address < flash.start || end > flash.end() {
            return Err(CliError::new(
                Code::Failed,
                format!(
                    "{shown} is {} bytes at {address:#x}, which runs past this board's \
                     flash ({:#x} +{:#x}) by {} bytes",
                    image.len(),
                    flash.start,
                    flash.size,
                    end.saturating_sub(flash.end())
                ),
            ));
        }
    }

    // Worth saying before rather than after: a kernel over a debug probe is
    // seconds of silence otherwise.
    if !tock_client::json_output() {
        eprintln!("writing {} bytes to {address:#x}", image.len());
    }

    // Both, even with `--no-verify`: declaring one or two depending on a
    // flag would route the command differently depending on a flag, which
    // is a worse surprise than opening a transport and not reading from it.
    let mut device = connect(&[Capability::MemoryRead, Capability::MemoryWrite])?;
    device
        .call(Request::Write { address }, image.clone())
        .map_err(proto)?;
    let verified = if args.no_verify {
        false
    } else {
        verify(&mut device, address, &image)?;
        true
    };

    if tock_client::json_output() {
        println!(
            "{}",
            serde_json::json!({
                "address": address,
                "bytes": image.len(),
                "verified": verified,
            })
        );
    } else {
        let checked = if verified { ", verified" } else { "" };
        println!("wrote {} byte(s) at {address:#x}{checked}", image.len());
    }
    Ok(())
}

/// Read back what was written and compare.
///
/// Separate from the write so `--no-verify` is a choice rather than a
/// different code path: a transport that erases before writing can destroy
/// a neighbour, and on a kernel that is not a failure anyone wants to find
/// out about by watching the board not boot.
fn verify(device: &mut Client, address: u64, image: &[u8]) -> Result<(), CliError> {
    device
        .require(Capability::MemoryRead, "checking what was written")
        .map_err(proto)?;
    let read_back = device
        .read_ranges(&[(address, image.len() as u64)])
        .map_err(proto)?;
    let got = read_back.first().map(|(_, data)| data.as_slice());
    if got == Some(image) {
        return Ok(());
    }
    // Where it first differs, because "did not verify" on a hundred
    // kilobytes says nothing about whether it was the erase, the write, or
    // the image.
    let at = got.and_then(|got| {
        image
            .iter()
            .zip(got)
            .position(|(wanted, found)| wanted != found)
    });
    Err(CliError::new(
        Code::Verify,
        match at {
            Some(offset) => format!(
                "flash does not match what was written, from {:#x}",
                address + offset as u64
            ),
            None => format!("flash does not match what was written, at {address:#x}"),
        },
    )
    .hint("rerun, or pass --no-verify if the read-back itself is the problem"))
}
