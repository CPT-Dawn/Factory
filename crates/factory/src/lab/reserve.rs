//! `devlab reserve`: a lead sets stack slots aside for the implementers it dispatches.
//!
//! Each reservation is an id the lead hands to one child as `DEVLAB_RESERVATION`; the child's
//! acquire turns it into its own lease without a turn in the queue. A lead with no room left under
//! its cap is refused at once (exit 75, naming the cap and an ETA); one that waits for a free slot
//! prints one line when it starts to wait and one when it is answered.

use factory_core::lab::args::wait_span;
use factory_core::lab::reserve::{ReserveOpts, parse_reserve};
use serde_json::json;

use super::cli::note;
use super::context::{Ctx, context};
use super::lease::{ANSWER_GRACE, paths, project_ask};
use super::wire::{RESERVE, ReserveParams, Reserved, UNRESERVE, UnreserveParams};
use crate::client;
use crate::output::Failure;

pub(super) const USAGE: &str = "usage: factory lab reserve [--slots N] [--wait SEC] [--json]\n       factory lab reserve --release [--json]\n\nreserve sets N stack slots of this project aside for implementers (up to your per-owner cap)\nand prints their ids; give each implementer one as DEVLAB_RESERVATION=<id> and its devlab\nacquire takes the slot at once. An owner with no room under its cap exits 75 with an ETA.\n--release gives every reservation of yours back. --wait takes seconds, or 90s, 9m or 1h.\nA reservation no lease has used for 20 min (since it was granted or its last lease ended)\ngoes back to the pool; devlab status shows when.\n";

pub fn main(args: &[String]) -> Result<u8, Failure> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(0);
    }
    let ctx = &context()?;
    let opts = parse_reserve(args).map_err(Failure::usage)?;
    match run(ctx, &opts) {
        Ok((line, value)) => {
            println!("{}", if opts.json { value.to_string() } else { line });
            Ok(0)
        }
        // With --json a failure is one object on stdout, as everywhere else.
        Err(failure) if opts.json => {
            println!(
                "{}",
                json!({"exit": failure.exit.code(), "error": failure.message})
            );
            Ok(failure.exit.code())
        }
        Err(failure) => Err(failure),
    }
}

/// What a reserve came to: its line, and the same as JSON.
fn run(ctx: &Ctx, opts: &ReserveOpts) -> Result<(String, serde_json::Value), Failure> {
    let Some(project) = &ctx.project else {
        return Err(Failure::usage(format!(
            "no devlab.json in {}: nothing to reserve",
            ctx.worktree
        )));
    };
    let ask = project_ask(ctx, project, false)?;
    if opts.release {
        let released: usize = client::call(
            &paths()?,
            UNRESERVE,
            &UnreserveParams {
                owner: ctx.owner.clone(),
                project: project.name.clone(),
            },
        )?;
        return Ok((
            format!(
                "released {released} {} reservation{}",
                project.name,
                if released == 1 { "" } else { "s" }
            ),
            json!({"project": project.name, "released": released}),
        ));
    }
    let wait_sec = opts.wait_sec.unwrap_or(ctx.cfg.wait_sec);
    let params = ReserveParams {
        worktree: ctx.worktree.clone(),
        owner: ctx.owner.clone(),
        project: ask,
        slots: opts.slots,
        wait_sec,
    };
    let listen = wait_span(wait_sec).checked_add(ANSWER_GRACE);
    let reserved: Reserved =
        client::call_streaming(&paths()?, RESERVE, &params, listen, &mut note)?;
    let value = json!({
        "project": reserved.project,
        "requested": reserved.requested,
        "granted": reserved.reservations.len(),
        "cap": reserved.cap,
        "reservations": reserved.reservations.iter()
            .map(|r| json!({"id": r.id, "slot": r.slot}))
            .collect::<Vec<_>>(),
    });
    let line = {
        let ids: Vec<String> = reserved
            .reservations
            .iter()
            .map(|r| r.id.to_string())
            .collect();
        format!(
            "granted {} of {} {} slots (cap {}): reservations {}; pass each to an implementer as DEVLAB_RESERVATION=<id>",
            reserved.reservations.len(),
            reserved.requested,
            reserved.project,
            reserved.cap,
            ids.join(", ")
        )
    };
    Ok((line, value))
}

/// `factory lab reserve` under the output contract: the line, and the same as JSON.
pub fn done(args: &[String]) -> Result<crate::output::Done, Failure> {
    let opts = parse_reserve(args).map_err(Failure::usage)?;
    let (line, value) = run(&context()?, &opts)?;
    Ok(crate::output::Done::new(line, value))
}
