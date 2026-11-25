//! Rebuilds time from before the plugin was running, out of the agents' own
//! session history. Two steps, so billable time is never inferred and written
//! in one go: `plan` reconstructs everything into a reviewable file, `apply`
//! writes what is left in it.

mod sources;

use crate::clock;
use crate::ledger::{Ledger, Outcome, Placement};
use crate::map::{Attribution, Map};
use crate::state::{bank, Block, Clock, Origin, State};
use crate::tracker::{MINIMUM_BLOCK, UNASSIGNED};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sources::{Kind, Session};
use std::collections::BTreeMap;

/// Silence inside a turn longer than this is not the agent working; it is a
/// permission prompt nobody answered, or a machine asleep.
const TURN_IDLE: i64 = 15 * 60;
/// Time between two of your prompts up to this long counts as attention on the
/// second one: reading the reply, then writing the next message. Checked
/// against Chronoid's measured focus time, a 5 minute cap matched and a 10
/// minute cap ran high.
const ATTENTION_LEAD: i64 = 5 * 60;
/// A prompt with nothing before it still took a moment to write.
const LONE_PROMPT: i64 = 2 * 60;

#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub since: String,
    pub until: String,
    pub days: Vec<Day>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Day {
    pub date: String,
    /// Whether Chronoid tracked anything itself that day. Attention on such a
    /// day overlays the tracked activity; on any other day it fills the gap.
    pub chronoid_tracked: bool,
    pub agent: Vec<Planned>,
    pub attention: Vec<Planned>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Planned {
    #[serde(flatten)]
    pub block: Block,
    #[serde(default)]
    pub written: bool,
}

pub fn plan_file() -> std::path::PathBuf {
    crate::paths::state_dir().join("backfill-plan.json")
}

/// A directory that moved after the history naming it was written.
pub struct Move {
    from: String,
    to: String,
}

impl Move {
    /// `OLD=NEW`, each absolute or starting with `~/`.
    pub fn parse(pair: &str) -> Result<Self> {
        let (from, to) = pair.split_once('=').context("--moved takes OLD=NEW")?;
        let expand = |path: &str| match path.strip_prefix("~/") {
            Some(rest) => crate::paths::home().join(rest).to_string_lossy().into_owned(),
            None => path.to_string(),
        };
        Ok(Self { from: expand(from.trim_end_matches('/')), to: expand(to.trim_end_matches('/')) })
    }

    fn apply(moves: &[Move], directory: &str) -> String {
        for moved in moves {
            if let Some(rest) = directory.strip_prefix(&moved.from) {
                if rest.is_empty() || rest.starts_with('/') {
                    return format!("{}{rest}", moved.to);
                }
            }
        }
        directory.to_string()
    }
}

pub fn plan(since: time::Date, moves: &[Move]) -> Result<()> {
    let since_seconds = clock::start_of_day(since);
    let until = clock::now();
    let map = Map::load()?;
    let mut sessions = sources::read_all(since_seconds)?;
    for session in &mut sessions {
        session.directory = Move::apply(moves, &session.directory);
    }

    let mut agent = Vec::new();
    let mut prompts: Vec<(i64, String)> = sources::typed_prompts(since_seconds)
        .into_iter()
        .map(|(time, directory)| (time, Move::apply(moves, &directory)))
        .collect();
    for session in &sessions {
        let opened = session.activity.first().map(|activity| activity.time).unwrap_or(until);
        let attributed = map.attribute_on(&session.directory, session.branch.as_deref(), opened);
        if matches!(attributed, Attribution::Personal) {
            continue;
        }
        let (name, linear_id) = match &attributed {
            Attribution::Project { linear_id, project } => (project.name.clone(), Some(linear_id.clone())),
            _ => (UNASSIGNED.to_string(), None),
        };
        for (start, end) in working_turns(session) {
            agent.push(block(Clock::Agent, &name, linear_id.clone(), session, start, end));
        }
        if session.agent != "claude" {
            // Claude Code prompts come from the history file, which reaches
            // further back than its transcripts.
            prompts.extend(
                session.activity.iter()
                    .filter(|activity| activity.kind == Kind::Prompt)
                    .map(|activity| (activity.time, session.directory.clone())),
            );
        }
    }

    prompts.sort_by_key(|(time, _)| *time);
    let mut attention = Vec::new();
    let mut previous: Option<i64> = None;
    for (time, directory) in &prompts {
        // Your prompts across every agent form one timeline: you can only be
        // writing one of them at a time, so attention never double counts.
        let start = match previous {
            Some(last) if time - last <= ATTENTION_LEAD => last,
            Some(last) => (time - LONE_PROMPT).max(last),
            None => time - LONE_PROMPT,
        };
        previous = Some(*time);
        // Attention with no project is left alone: Chronoid keeps the tracked
        // activity, and the weekly review decides where it belongs.
        let Attribution::Project { linear_id, project } = map.attribute(directory, *time) else {
            continue;
        };
        if *time > start {
            attention.push(Block {
                clock: Clock::Attention,
                origin: Origin::Transcripts,
                linear_id: Some(linear_id),
                project: project.name,
                start,
                end: *time,
                titles: Vec::new(),
                sources: vec!["typed prompts".to_string()],
                agents: Vec::new(),
            });
        }
    }

    let mut merged = Vec::new();
    for mut blocks in [agent, attention] {
        blocks.sort_by_key(|block| block.start);
        let mut pending = Vec::new();
        for block in blocks {
            bank(&mut pending, block);
        }
        merged.extend(pending.into_iter().filter(|block| block.seconds() >= MINIMUM_BLOCK));
    }

    let mut days: BTreeMap<String, Day> = BTreeMap::new();
    for block in merged {
        let date = clock::local_date(block.start);
        let day = days.entry(date.clone()).or_insert_with(|| Day {
            date,
            chronoid_tracked: false,
            agent: Vec::new(),
            attention: Vec::new(),
        });
        let planned = Planned { block, written: false };
        match planned.block.clock {
            Clock::Agent => day.agent.push(planned),
            Clock::Attention => day.attention.push(planned),
        }
    }

    let chronoid = crate::chronoid::Chronoid::connect()?;
    for day in days.values_mut() {
        day.chronoid_tracked = chronoid
            .tracked_anything(&day.date)
            .with_context(|| format!("check Chronoid's tracking on {}", day.date))?;
        day.agent.sort_by_key(|planned| planned.block.start);
        day.attention.sort_by_key(|planned| planned.block.start);
    }

    let plan = Plan {
        since: since.to_string(),
        until: clock::to_iso(until)?,
        days: days.into_values().collect(),
    };
    std::fs::create_dir_all(crate::paths::state_dir())?;
    std::fs::write(plan_file(), serde_json::to_vec_pretty(&plan)?).context("write the plan")?;
    summarize(&plan, sessions.len());
    Ok(())
}

/// A session's working turns: from a prompt, or the first agent activity after
/// a quiet spell, to the agent's last activity before you spoke again.
fn working_turns(session: &Session) -> Vec<(i64, i64)> {
    let mut turns = Vec::new();
    let mut current: Option<(i64, i64)> = None;
    for activity in &session.activity {
        current = match (activity.kind, current) {
            (Kind::Prompt, open) => {
                turns.extend(open);
                Some((activity.time, activity.time))
            }
            (Kind::Agent, Some((start, last))) if activity.time - last <= TURN_IDLE => {
                Some((start, activity.time))
            }
            (Kind::Agent, open) => {
                turns.extend(open);
                Some((activity.time, activity.time))
            }
        };
    }
    turns.extend(current);
    turns.retain(|(start, end)| end > start);
    turns
}

fn block(clock: Clock, project: &str, linear_id: Option<String>, session: &Session,
         start: i64, end: i64) -> Block {
    Block {
        clock,
        origin: Origin::Transcripts,
        linear_id,
        project: project.to_string(),
        start,
        end,
        titles: session.title.clone().into_iter().collect(),
        sources: vec![format!("{} {}", session.agent, short(&session.id))],
        agents: vec![session.agent.to_string()],
    }
}

fn short(id: &str) -> &str {
    id.rsplit('-').next().filter(|tail| tail.len() >= 8).unwrap_or(id)
}

fn summarize(plan: &Plan, sessions: usize) {
    let mut totals: BTreeMap<(String, String), (i64, i64)> = BTreeMap::new();
    for day in &plan.days {
        let month = day.date[..7].to_string();
        for planned in &day.agent {
            totals.entry((month.clone(), planned.block.project.clone())).or_default().0 +=
                planned.block.seconds();
        }
        for planned in &day.attention {
            totals.entry((month.clone(), planned.block.project.clone())).or_default().1 +=
                planned.block.seconds();
        }
    }
    println!("Read {sessions} agent sessions since {}.\n", plan.since);
    println!("{:8} {:32} {:>9} {:>10}", "month", "project", "agent", "attention");
    let (mut agent_total, mut attention_total) = (0, 0);
    for ((month, project), (agent, attention)) in &totals {
        agent_total += agent;
        attention_total += attention;
        println!(
            "{month:8} {project:32} {:>9} {:>10}",
            clock::duration(*agent),
            clock::duration(*attention)
        );
    }
    println!(
        "{:8} {:32} {:>9} {:>10}",
        "",
        "total",
        clock::duration(agent_total),
        clock::duration(attention_total)
    );
    let untracked = plan.days.iter().filter(|day| !day.chronoid_tracked).count();
    println!(
        "\n{} days, {untracked} of them before or outside Chronoid's own tracking.",
        plan.days.len()
    );
    println!("Plan: {}", plan_file().display());
    println!("Delete any attention blocks or days you do not want billed, then run `backfill apply`.");
}

/// Write every unwritten block in the plan. Agent runtime always goes; attention
/// only when asked, after you have reviewed it. Progress is saved after every
/// entry, so an interrupted apply resumes without duplicates.
pub fn apply(include_attention: bool) -> Result<()> {
    let text = std::fs::read_to_string(plan_file()).context("read the plan; run `backfill plan` first")?;
    let mut plan: Plan = serde_json::from_str(&text).context("parse the plan")?;
    let map = Map::load()?;
    let state = State::load()?;
    let mut ledger = Ledger::default();
    let (mut written, mut skipped, mut overlapped, mut dropped) = (0, 0, 0, 0);

    for day_index in 0..plan.days.len() {
        let tracked = plan.days[day_index].chronoid_tracked;
        for clock in [Clock::Agent, Clock::Attention] {
            if clock == Clock::Attention && !include_attention {
                continue;
            }
            let count = match clock {
                Clock::Agent => plan.days[day_index].agent.len(),
                Clock::Attention => plan.days[day_index].attention.len(),
            };
            for index in 0..count {
                let day = &mut plan.days[day_index];
                let planned = match clock {
                    Clock::Agent => &mut day.agent[index],
                    Clock::Attention => &mut day.attention[index],
                };
                if planned.written {
                    skipped += 1;
                    continue;
                }
                let placement = match (clock, tracked) {
                    (Clock::Agent, _) => Placement::Annotation,
                    (Clock::Attention, true) => Placement::Overlay,
                    (Clock::Attention, false) => Placement::OfflineGap,
                };
                let outcome = ledger
                    .write(&map, &planned.block, placement)
                    .with_context(|| format!("write {} {} on {}", clock.label(), planned.block.project, day.date))?;
                match outcome {
                    Outcome::Written => written += 1,
                    Outcome::Covered => overlapped += 1,
                    // Left unwritten, so a later apply tries it again once the
                    // map or the entry is fixed.
                    Outcome::Dropped(reason) => {
                        dropped += 1;
                        println!("not written: {} {} on {}: {reason}", clock.label(), planned.block.project, day.date);
                        continue;
                    }
                }
                planned.written = true;
                state.save()?;
                std::fs::write(plan_file(), serde_json::to_vec_pretty(&plan)?)?;
            }
        }
    }
    println!("Wrote {written} entries ({skipped} already written, {overlapped} skipped as already covered, {dropped} not written).");
    if !include_attention {
        println!("Attention was not written. Run `backfill apply --attention` once you have reviewed it.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Move;

    #[test]
    fn moves_a_directory_and_everything_under_it_only() {
        let moves = [Move::parse("/old/git=/new/git").unwrap()];
        assert_eq!(Move::apply(&moves, "/old/git/widgets/.claude/worktrees/x"), "/new/git/widgets/.claude/worktrees/x");
        assert_eq!(Move::apply(&moves, "/old/git"), "/new/git");
        assert_eq!(Move::apply(&moves, "/old/gitlab/repo"), "/old/gitlab/repo");
    }
}
