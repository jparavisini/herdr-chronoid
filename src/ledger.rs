use crate::chronoid::{Chronoid, Entry, TimeEntry};
use crate::clock;
use crate::map::Map;
use crate::state::{Block, Clock};
use anyhow::Result;

/// How an entry sits against the activity Chronoid tracked on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Claims the tracked activity underneath it, and nothing more. Billable
    /// attention on days Chronoid was tracking.
    Overlay,
    /// Stands in for time Chronoid never saw: attention from before it was
    /// running, or from days it was not.
    OfflineGap,
    /// Records the span without touching tracked time. Agent runtime overlaps
    /// itself across panes, so it can only ever be an annotation.
    Annotation,
}

impl Placement {
    fn entry_kind(self) -> (&'static str, &'static str) {
        match self {
            Placement::Overlay => ("project_overlay", "unassigned_only"),
            Placement::OfflineGap => ("offline_gap", "none"),
            Placement::Annotation => ("annotation_only", "none"),
        }
    }
}

/// What became of a block handed to the ledger. Every outcome settles the
/// block; only an error, when Chronoid could not be reached, leaves it to try
/// again.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Written,
    /// Chronoid already holds an entry for some of those minutes. That is a
    /// fact about the day, not a failure: something accounts for them.
    Covered,
    /// The block cannot be written, now or later.
    Dropped(String),
}

/// Writes attention to Chronoid, with the project id from the map. Agent runtime never goes to Chronoid: it refuses
/// overlapping entries and agent hours overlap, so they go to the agent hours
/// ledger instead.
#[derive(Default)]
pub struct Ledger {
    chronoid: Option<Chronoid>,
}

impl Ledger {
    pub fn write(&mut self, map: &Map, block: &Block, placement: Placement) -> Result<Outcome> {
        if block.clock == Clock::Agent {
            crate::agent_hours::append(block)?;
            return Ok(Outcome::Written);
        }
        let Some(project) = block.linear_id.as_deref().and_then(|id| map.project(id)) else {
            return Ok(Outcome::Dropped(format!("{} is not a project in the map", block.project)));
        };
        let (kind, policy) = placement.entry_kind();
        let entry = self.chronoid()?.create_time_entry(&TimeEntry {
            title: &block.title(),
            notes: &block.notes(),
            start: clock::to_iso(block.start)?,
            end: clock::to_iso(block.end)?,
            project_id: project.chronoid_id,
            kind,
            assignment_policy: policy,
        })?;
        Ok(match entry {
            Entry::Created => Outcome::Written,
            Entry::Overlaps => Outcome::Covered,
            Entry::Refused(reason) => Outcome::Dropped(reason),
        })
    }

    fn chronoid(&mut self) -> Result<&Chronoid> {
        if self.chronoid.is_none() {
            self.chronoid = Some(Chronoid::connect()?);
        }
        Ok(self.chronoid.as_ref().expect("just connected"))
    }
}
