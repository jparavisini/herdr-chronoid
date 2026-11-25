use crate::activity::Transcripts;
use crate::clock;
use crate::herdr::Pane;
use crate::ledger::{Ledger, Outcome, Placement};
use crate::log;
use crate::map::{Attribution, Map};
use crate::state::{bank, Block, Clock, Origin, State, COALESCE_GAP};

/// A merged block shorter than this is noise, not a time entry.
pub const MINIMUM_BLOCK: i64 = 60;
/// Shown on agent runtime that belongs to no project yet.
pub const UNASSIGNED: &str = "Unassigned";
/// No input for this long means you have left the machine. Attention ends at
/// your last input. The same five minutes the backfill credits between prompts.
pub const AWAY_AFTER: i64 = 5 * 60;
/// A working agent whose transcript has been quiet this long has stopped,
/// whatever Herdr says. The backfill ends a turn at the same silence.
pub const AGENT_SILENCE: i64 = 15 * 60;

pub struct Tracker {
    pub state: State,
    map: Map,
    ledger: Ledger,
    transcripts: Transcripts,
    /// Your last input, while you are away.
    away_since: Option<i64>,
}

impl Tracker {
    pub fn new(state: State, map: Map) -> Self {
        Self {
            state,
            map,
            ledger: Ledger::default(),
            transcripts: Transcripts::new(&crate::paths::home()),
            away_since: None,
        }
    }

    /// Fold a pane's current state into the open intervals. Herdr resends a
    /// pane on every change, so only transitions start or stop a clock.
    pub fn apply(&mut self, pane: &Pane, now: i64) {
        // Focus is exclusive: the pane that gains it ends everyone else's
        // attention, whether or not Herdr resends the pane that lost it, and
        // whatever the focused pane turns out to be.
        if pane.focused {
            let others: Vec<String> = self
                .state
                .open
                .keys()
                .filter(|id| id.as_str() != pane.pane_id)
                .cloned()
                .collect();
            for id in others {
                if let Some(open) = self.state.open.get_mut(&id) {
                    open.focused = false;
                }
                self.close(&id, Clock::Attention, now);
            }
        }

        let (project, linear_id) = match pane.directory().map(|dir| self.map.attribute(dir, now)) {
            Some(Attribution::Project { linear_id, project }) => (project.name, Some(linear_id)),
            // Personal panes are not tracked at all.
            Some(Attribution::Personal) | None => {
                self.forget(&pane.pane_id, now);
                return;
            }
            // Business time with no project still spends agent hours worth
            // reporting. Its attention is not recorded: Chronoid keeps the
            // tracked activity, and the weekly review triages it.
            Some(Attribution::Unmapped) => (UNASSIGNED.to_string(), None),
        };

        if self
            .state
            .open
            .get(&pane.pane_id)
            .is_some_and(|open| open.linear_id != linear_id)
        {
            // The pane moved to another project. Bank what it did in the old
            // one before the new project takes over.
            self.close(&pane.pane_id, Clock::Attention, now);
            self.close(&pane.pane_id, Clock::Agent, now);
        }

        let open = self.state.open.entry(pane.pane_id.clone()).or_default();
        open.project = project;
        open.linear_id = linear_id;
        if let Some(title) = pane.title() {
            open.title = Some(title.to_string());
        }
        if let Some(agent) = &pane.agent {
            open.agent = Some(agent.clone());
        }
        open.focused = pane.focused;
        open.working = pane.is_working();
        open.session = pane.agent_session.clone();
        self.settle(&pane.pane_id, now);
    }

    /// Start or stop a pane's clocks to match what is known about it: attention
    /// runs while it is focused on a project and you are here, agent runtime
    /// while Herdr says the agent is working.
    fn settle(&mut self, pane_id: &str, at: i64) {
        let Some(open) = self.state.open.get_mut(pane_id) else {
            return;
        };
        let attending = open.focused && open.linear_id.is_some() && self.away_since.is_none();
        let attention_running = open.attention_since.is_some();
        let agent_running = open.agent_since.is_some();
        if attending && !attention_running {
            open.attention_since = Some(at);
        }
        if open.working && !agent_running {
            open.agent_since = Some(at);
        }
        let working = open.working;
        if !attending && attention_running {
            self.close(pane_id, Clock::Attention, at);
        }
        if !working && agent_running {
            self.close(pane_id, Clock::Agent, at);
        }
    }

    /// The periodic pass: notice you leaving and coming back, and stop agents
    /// whose transcripts went quiet. Writing is `flush`'s job.
    pub fn tick(&mut self, now: i64, input_idle: i64) {
        let last_input = now - input_idle;
        let panes: Vec<String> = self.state.open.keys().cloned().collect();
        if input_idle >= AWAY_AFTER {
            if self.away_since.is_none() {
                self.away_since = Some(last_input);
                log::line(format!("away since {}", clock::to_iso(last_input).unwrap_or_default()));
                for id in &panes {
                    self.close(id, Clock::Attention, last_input);
                }
            }
        } else if self.away_since.take().is_some() {
            log::line("back at the machine");
            for id in &panes {
                self.settle(id, last_input);
            }
        }

        for id in &panes {
            let Some(open) = self.state.open.get(id) else {
                continue;
            };
            let Some(session) = open.session.clone() else {
                continue;
            };
            let (running, working) = (open.agent_since.is_some(), open.working);
            let Some(last) = self.transcripts.last_activity(&session) else {
                continue;
            };
            let quiet = now - last > AGENT_SILENCE;
            if running && quiet {
                log::line(format!("{id}: agent quiet since {}", clock::to_iso(last).unwrap_or_default()));
                self.close(id, Clock::Agent, last);
            } else if working && !running && !quiet {
                if let Some(open) = self.state.open.get_mut(id) {
                    open.agent_since = Some(last);
                }
            }
        }
    }

    /// A pane that closed or exited stops both of its clocks.
    pub fn forget(&mut self, pane_id: &str, now: i64) {
        self.close(pane_id, Clock::Attention, now);
        self.close(pane_id, Clock::Agent, now);
        self.state.open.remove(pane_id);
    }

    /// Close whatever was still running when the daemon last ran, using the
    /// last moment it saw rather than now, so a machine that slept overnight
    /// does not report the night.
    pub fn close_stale(&mut self) {
        let Some(last_seen) = self.state.last_seen else {
            return;
        };
        let panes: Vec<String> = self.state.open.keys().cloned().collect();
        for pane_id in panes {
            self.forget(&pane_id, last_seen);
        }
    }

    fn close(&mut self, pane_id: &str, clock: Clock, end: i64) {
        let Some(open) = self.state.open.get_mut(pane_id) else {
            return;
        };
        let since = match clock {
            Clock::Attention => open.attention_since.take(),
            Clock::Agent => open.agent_since.take(),
        };
        let Some(start) = since.filter(|start| end > *start) else {
            return;
        };
        let block = Block {
            clock,
            origin: Origin::Herdr,
            linear_id: open.linear_id.clone(),
            project: open.project.clone(),
            start,
            end,
            titles: open.title.clone().into_iter().collect(),
            sources: vec![pane_id.to_string()],
            agents: open.agent.clone().into_iter().collect(),
        };
        bank(&mut self.state.pending, block);
    }

    /// Write out every block that has gone quiet for longer than the coalescing
    /// window. Each block is settled on its own; only a Chronoid that cannot be
    /// reached holds the rest for the next tick.
    pub fn flush(&mut self, now: i64) {
        let ready: Vec<usize> = self
            .state
            .pending
            .iter()
            .enumerate()
            .filter(|(_, block)| now - block.end >= COALESCE_GAP)
            .map(|(index, _)| index)
            .collect();

        let mut done = Vec::new();
        for index in ready {
            let block = self.state.pending[index].clone();
            if block.seconds() < MINIMUM_BLOCK {
                done.push(index);
                continue;
            }
            let summary = format!(
                "{} {} {} ({})",
                block.clock.label(),
                block.project,
                clock::duration(block.seconds()),
                block.title()
            );
            match self.write(&block) {
                Ok(Outcome::Written) => log::line(format!("wrote {summary}")),
                Ok(Outcome::Covered) => log::line(format!("skipped, already covered: {summary}")),
                Ok(Outcome::Dropped(reason)) => log::line(format!("dropped {summary}: {reason}")),
                Err(error) => {
                    log::line(format!("holding {summary}: {error:#}"));
                    break;
                }
            }
            done.push(index);
        }
        for index in done.into_iter().rev() {
            self.state.pending.remove(index);
        }
    }

    /// Write everything, however recent: used when the daemon is shutting down
    /// and there will be no later tick to close the day out.
    pub fn flush_all(&mut self, now: i64) {
        self.flush(now + COALESCE_GAP);
    }

    fn write(&mut self, block: &Block) -> anyhow::Result<Outcome> {
        // Live attention sits on top of what Chronoid is tracking right now.
        let placement = match block.clock {
            Clock::Attention => Placement::Overlay,
            Clock::Agent => Placement::Annotation,
        };
        self.ledger.write(&self.map, block, placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::AgentSession;

    const PROJECT: &str = "11111111-2222-3333-4444-555555555555";
    const T: i64 = 1_790_000_000;

    fn tracker() -> Tracker {
        tracker_with_personal(&[])
    }

    fn tracker_with_personal(personal: &[&str]) -> Tracker {
        let map: Map = serde_json::from_value(serde_json::json!({
            "projects": {PROJECT: {"name": "Client work", "chronoid_id": 7}},
            "categories": {},
            "issues": {"ABC-1": PROJECT},
            "repos": {},
            "personal_paths": personal,
        }))
        .unwrap();
        Tracker::new(State::default(), map)
    }

    fn pane(id: &str, directory: &str, focused: bool, status: &str) -> Pane {
        serde_json::from_value(serde_json::json!({
            "pane_id": id,
            "focused": focused,
            "agent": "claude",
            "agent_status": status,
            "cwd": directory,
        }))
        .unwrap()
    }

    const MAPPED: &str = "/nowhere/worktrees/ABC-1";
    const UNMAPPED: &str = "/nowhere/scratch";

    #[test]
    fn focus_on_a_personal_pane_ends_attention_elsewhere() {
        let mut tracker = tracker_with_personal(&["/nowhere/personal"]);
        tracker.apply(&pane("p1", MAPPED, true, "idle"), T);
        tracker.apply(&pane("p2", "/nowhere/personal/notes", true, "idle"), T + 600);
        assert_eq!(tracker.state.open["p1"].attention_since, None);
        assert_eq!((tracker.state.pending[0].start, tracker.state.pending[0].end), (T, T + 600));
    }

    #[test]
    fn unmapped_panes_record_agent_time_but_no_attention() {
        let mut tracker = tracker();
        tracker.apply(&pane("p1", UNMAPPED, true, "working"), T);
        let open = &tracker.state.open["p1"];
        assert_eq!(open.attention_since, None);
        assert_eq!(open.agent_since, Some(T));
    }

    #[test]
    fn focus_moves_attention_between_mapped_panes() {
        let mut tracker = tracker();
        tracker.apply(&pane("p1", MAPPED, true, "idle"), T);
        tracker.apply(&pane("p2", UNMAPPED, true, "idle"), T + 600);
        assert_eq!(tracker.state.open["p1"].attention_since, None);
        assert!(!tracker.state.open["p1"].focused);
        assert_eq!(tracker.state.pending.len(), 1);
        assert_eq!((tracker.state.pending[0].start, tracker.state.pending[0].end), (T, T + 600));
    }

    #[test]
    fn attention_stops_at_the_last_input_and_resumes_on_return() {
        let mut tracker = tracker();
        tracker.apply(&pane("p1", MAPPED, true, "idle"), T);
        // Twenty minutes in, the last input was eight minutes ago.
        tracker.tick(T + 1200, 480);
        assert_eq!(tracker.state.open["p1"].attention_since, None);
        let block = &tracker.state.pending[0];
        assert_eq!((block.start, block.end), (T, T + 720));
        // A pane update while away does not restart attention.
        tracker.apply(&pane("p1", MAPPED, true, "idle"), T + 1500);
        assert_eq!(tracker.state.open["p1"].attention_since, None);
        // Input ten seconds ago: you are back, and attention starts from then.
        tracker.tick(T + 3600, 10);
        assert_eq!(tracker.state.open["p1"].attention_since, Some(T + 3590));
    }

    #[test]
    fn agents_without_a_transcript_keep_running() {
        let mut tracker = tracker();
        let mut working = pane("p1", UNMAPPED, false, "working");
        working.agent_session = Some(AgentSession {
            agent: "claude".into(),
            kind: "id".into(),
            value: "no-such-session-for-this-test".into(),
        });
        tracker.apply(&working, T);
        tracker.tick(T + 7200, 0);
        assert_eq!(tracker.state.open["p1"].agent_since, Some(T));
    }

    #[test]
    fn a_block_that_cannot_be_written_does_not_hold_the_rest() {
        // Safe to flush in a test only because both blocks are dropped before
        // the ledger connects to Chronoid. Tests never flush a writable block.
        let mut tracker = tracker();
        // Two attention blocks on a project the map no longer has.
        for start in [T, T + 3600] {
            tracker.state.pending.push(Block {
                clock: Clock::Attention,
                origin: Origin::Herdr,
                linear_id: Some("gone".into()),
                project: "Gone".into(),
                start,
                end: start + 600,
                titles: vec![],
                sources: vec![],
                agents: vec![],
            });
        }
        tracker.flush(T + 86_400);
        assert!(tracker.state.pending.is_empty());
    }
}
