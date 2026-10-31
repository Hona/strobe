//! The MCP server (SPEC §17): same tools, same JSON as the CLI.

use crate::cli::Args;
use crate::model::Result;

pub fn stdio() -> Result<()> {
    Ok(())
}

pub fn daemon(_args: &Args) -> Result<()> {
    Ok(())
}
