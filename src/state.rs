use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The two clocks this plugin keeps. They answer different questions and must
/// never land in the same Chronoid project: attention is what you can bill, and
/// agent runtime is subscription time that overlaps itself across panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clock {
    Attention,
    Agent,
}

impl Clock {
    pub fn label(self) -> &'static str {
        match self {
            Clock::Attention => "attention",
            Clock::Agent => "agent",
        }
    }
}

/// Where a block's time was observed: live from Herdr, or reconstructed after
/// the fact from agent transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Herdr,
    Transcripts,
}

/// Intervals on one project that are this close together become one entry.
pub const COALESCE_GAP: i64 = 5 * 60;

/// A closed interval waiting to be written to Chronoid. Consecutive intervals
/// on the same project merge into one block so a day of pane hopping does not
/// become three hundred one-minute entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub clock: Clock,
    pub origin: Origin,
    /// The map's project key this time belongs to, or None for business time
    /// with no project yet.
    pub linear_id: Option<String>,
    /// The project's name, for the entry and for reading the state file.
    pub project: String,
    pub start: i64,
    pub end: i64,
    /// Task titles that fed this block, in the order they were seen.
    pub titles: Vec<String>,
    /// Herdr pane ids, or agent session ids for reconstructed time.
    pub sources: Vec<String>,
    pub agents: Vec<String>,
}

impl Block {
    pub fn seconds(&self) -> i64 {
        (self.end - self.start).max(0)
    }

    pub fn absorb(&mut self, other: &Block) {
        self.end = self.end.max(other.end);
        for (mine, theirs) in [
            (&mut self.titles, &other.titles),
            (&mut self.sources, &other.sources),
            (&mut self.agents, &other.agents),
        ] {
            for value in theirs {
                if !mine.contains(value) {
                    mine.push(value.clone());
                }
            }
        }
    }

    pub fn title(&self) -> String {
        match self.titles.split_first() {
            None => self.project.clone(),
            Some((first, [])) => first.clone(),
            Some((first, rest)) => format!("{first} +{} more", rest.len()),
        }
    }

    pub fn notes(&self) -> String {
        let origin = match self.origin {
            Origin::Herdr => "Herdr",
            Origin::Transcripts => "Backfilled from agent transcripts:",
        };
        let mut lines = vec![format!(
            "{origin} {} time · {} · sources: {}",
            self.clock.label(),
            crate::clock::duration(self.seconds()),
            self.sources.join(", ")
        )];
        if !self.agents.is_empty() {
            lines.push(format!("agents: {}", self.agents.join(", ")));
        }
        if self.titles.len() > 1 {
            lines.push(String::from("tasks:"));
            lines.extend(self.titles.iter().map(|title| format!("- {title}")));
        }
        lines.join("\n")
    }
}

/// Add a closed interval to a pending list, cut at local midnight and merged
/// into a neighbouring block on the same project when they are close enough
/// together to be one sitting.
pub fn bank(pending: &mut Vec<Block>, block: Block) {
    let mut cursor = block.start;
    while cursor < block.end {
        let boundary = crate::clock::next_midnight(cursor).min(block.end);
        let piece = Block {
            start: cursor,
            end: boundary,
            ..block.clone()
        };
        cursor = boundary;
        // Only the most recent block of this clock can absorb the piece.
        // Merging with an older one would swallow the interval another project
        // holds in between, and Chronoid refuses overlapping entries.
        let neighbour = pending
            .iter_mut()
            .filter(|existing| existing.clock == piece.clock)
            .max_by_key(|existing| existing.end)
            .filter(|existing| {
                existing.linear_id == piece.linear_id
                    // A block that ends at midnight belongs to the day it
                    // started, so judge the day by its start.
                    && crate::clock::same_local_day(existing.start, piece.start)
                    // Only sequential time merges. Two agents working the same
                    // repository at once are two blocks of runtime, not one.
                    && piece.start >= existing.end
                    && piece.start - existing.end <= COALESCE_GAP
            });
        match neighbour {
            Some(existing) => existing.absorb(&piece),
            None => pending.push(piece),
        }
    }
}

/// A pane's currently running intervals. `since` is when the clock started;
/// `None` means that clock is not running for this pane.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OpenPane {
    pub project: String,
    pub linear_id: Option<String>,
    pub title: Option<String>,
    pub agent: Option<String>,
    pub attention_since: Option<i64>,
    pub agent_since: Option<i64>,
    /// What Herdr last said about the pane. A clock can be stopped while these
    /// hold, when you are away or the agent's transcript has gone quiet, and
    /// they say whether to start it again when that ends.
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub working: bool,
    #[serde(default)]
    pub session: Option<crate::activity::AgentSession>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Chronoid project ids, keyed by `<clock>:<project>` plus the agent tree's
    /// root. Kept because Chronoid's MCP surface can create projects but not
    /// list them, so a lost cache means a duplicate project.
    pub projects: BTreeMap<String, i64>,
    pub open: BTreeMap<String, OpenPane>,
    pub pending: Vec<Block>,
    /// When the daemon last processed an event, used to close intervals that
    /// were still open when it stopped.
    pub last_seen: Option<i64>,
}

impl State {
    pub fn load() -> Result<Self> {
        let path = crate::paths::state_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).context("parse the saved state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("read the saved state"),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = crate::paths::state_file();
        std::fs::create_dir_all(crate::paths::state_dir()).context("create the state directory")?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?).context("write the state")?;
        std::fs::rename(&temporary, &path).context("replace the state file")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_across_midnight_is_cut_into_two_days() {
        let start = 1_790_000_000;
        let midnight = crate::clock::next_midnight(start);
        let block = Block {
            clock: Clock::Attention,
            origin: Origin::Herdr,
            linear_id: Some("p".into()),
            project: "P".into(),
            start: midnight - 3600,
            end: midnight + 3600,
            titles: vec![],
            sources: vec![],
            agents: vec![],
        };
        let mut pending = Vec::new();
        bank(&mut pending, block);
        let spans: Vec<(i64, i64)> = pending.iter().map(|block| (block.start, block.end)).collect();
        assert_eq!(spans, vec![(midnight - 3600, midnight), (midnight, midnight + 3600)]);
    }
}
