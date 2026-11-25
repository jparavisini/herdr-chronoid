//! Where agent runtime is recorded.
//!
//! Chronoid refuses overlapping manual entries, of any kind, in any project.
//! Agent hours overlap by definition: that is the whole point of running four
//! panes at once, and of counting hours bought with a subscription. So they are
//! kept here, one JSON object per block, append-only, for your own reports.
//! Chronoid keeps the attention that can be billed.

use crate::state::Block;
use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

pub fn path() -> PathBuf {
    crate::paths::state_dir().join("agent-hours.jsonl")
}

#[derive(Serialize)]
struct Record<'a> {
    start: String,
    end: String,
    seconds: i64,
    project: &'a str,
    linear_project_id: Option<&'a str>,
    titles: &'a [String],
    agents: &'a [String],
    sources: &'a [String],
    origin: &'a str,
}

pub fn append(block: &Block) -> Result<()> {
    std::fs::create_dir_all(crate::paths::state_dir())?;
    let record = Record {
        start: crate::clock::to_iso(block.start)?,
        end: crate::clock::to_iso(block.end)?,
        seconds: block.seconds(),
        project: &block.project,
        linear_project_id: block.linear_id.as_deref(),
        titles: &block.titles,
        agents: &block.agents,
        sources: &block.sources,
        origin: match block.origin {
            crate::state::Origin::Herdr => "herdr",
            crate::state::Origin::Transcripts => "transcripts",
        },
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path())
        .context("open the agent hours ledger")?;
    writeln!(file, "{}", serde_json::to_string(&record)?).context("write an agent hours record")
}
