//! `strobe up` / `strobe leave`: one-time setup and its undo (SPEC §5.3, §5.10).

use super::Output;
use crate::cli::Args;
use crate::model::Result;
use serde_json::json;

pub fn run(_args: &Args) -> Result<Output> {
    let exe = crate::engine::ensure()?;
    Ok(Output::new(format!("  ✓ engine     {}\n", exe.display()), json!({ "engine": exe })))
}

pub fn leave(_args: &Args) -> Result<Output> {
    Ok(Output::new("nothing to undo yet\n".into(), json!({})))
}
