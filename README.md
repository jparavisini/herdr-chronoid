# herdr-chronoid

A [Herdr](https://herdr.dev) plugin that reports pane time into
[Chronoid](https://www.chronoid.app), the macOS time tracker. It keeps two
clocks, because they answer different questions.

**Attention** is the time a pane is focused in Herdr. It goes to the Chronoid
project the pane belongs to, written as a `project_overlay` entry with
`assignment_policy: unassigned_only`, so it claims the activity Chronoid already
tracked underneath it and nothing else. Only one pane is focused at a time, so
these totals stay true to the clock and can be invoiced. Mark the projects you
bill for as billable in Chronoid.

**Agent runtime** is the time a pane's coding agent spends `working`. Panes run
concurrently, so these hours overlap each other and the wall clock. Chronoid
refuses overlapping entries, so they go to a local ledger,
`~/.local/state/herdr-chronoid/agent-hours.jsonl`, one JSON object per block,
for your own reports. They are never billable.

## Requirements

- macOS. Idle detection reads input through CoreGraphics.
- [Herdr](https://herdr.dev) 0.8.0 or later.
- [Chronoid](https://www.chronoid.app), running. The plugin talks to its local
  MCP server on `127.0.0.1:17686` with the bearer token the app writes to
  `~/Library/Application Support/com.vu.Chronoid/`. When Chronoid is closed,
  blocks stay pending and are written on a later tick.
- A Rust toolchain, 1.85 or later. Herdr builds the plugin with
  `cargo build --release`.
- A project map at `~/.config/chronoid/projects.json`. See below.

## Install

```sh
herdr plugin install jparavisini/herdr-chronoid
herdr plugin enable herdr-chronoid
```

Or from a clone:

```sh
git clone https://github.com/jparavisini/herdr-chronoid.git
herdr plugin link ./herdr-chronoid
herdr plugin enable herdr-chronoid
```

## The project map

`~/.config/chronoid/projects.json` decides which Chronoid project a pane's time
goes to. You write it, by hand or with your own sync from an issue tracker. The
plugin only reads it and never creates a Chronoid project. Create projects in
Chronoid first; its MCP server's `create_project` tool returns the id.

```json
{
  "projects": {
    "acme-website": { "name": "Acme website", "chronoid_id": 101 },
    "widgets-api": { "name": "Widgets API", "chronoid_id": 102 }
  },
  "categories": {
    "internal": 200
  },
  "issues": {
    "ABC-12": "acme-website",
    "ABC-31": "widgets-api"
  },
  "repos": {
    "acme-site": "acme-website",
    "widgets": [
      { "target": "acme-website", "until": "2026-03-01" },
      { "target": "widgets-api", "from": "2026-03-01" }
    ],
    "tooling": "category:internal"
  },
  "personal_paths": ["~/notes", "~/git/personal"]
}
```

All five keys must be present; use `{}` or `[]` for the ones you do not need.

- `projects`: your project keys, each with its name and Chronoid project id.
  A key is any string you like, for example a Linear project id.
- `categories`: Chronoid project ids for work that has a category but no
  project of its own. A target names one as `category:<key>`.
- `issues`: tickets to project keys. The prefixes these tickets use (`ABC`
  above) are the ones the plugin looks for in branch names and paths.
- `repos`: a repository's default target, by the repository's directory name.
  Worktrees count as their main repository. A list of entries with `from` and
  `until` dates (`from` inclusive, `until` exclusive) covers a repository that
  served different projects over time.
- `personal_paths`: directories whose time is never recorded, by either clock.

A pane's time goes to:

1. The ticket in the pane's branch or path, for example `ABC-12` in
   `feature/abc-12-login`, through `issues`.
2. Else the repository's default from `repos`.
3. Else nowhere for attention, since Chronoid still holds the tracked activity
   for you to sort out. Agent runtime is recorded as Unassigned.

## How it works

A daemon subscribes to Herdr's socket (`pane.updated` and friends), which
carries each pane's focus, agent status, directory and title. Focus and
`working` are edge-triggered into intervals. A closed interval is banked into a
pending block, merged with the last block on the same project when they are
within five minutes of each other, and cut at local midnight. A block is written
once it has been quiet for five minutes; blocks shorter than a minute are
dropped.

Three things stop a clock that Herdr would leave running:

- **Away.** After five minutes with no keyboard, mouse or trackpad input,
  attention ends at the last input, and starts again from the next one.
- **Asleep.** When the daemon finds it did not run for two minutes, the machine
  slept: every clock ends at the last moment the daemon saw.
- **Quiet agent.** An agent whose transcript (Claude Code or Codex, including
  Claude Code's subagent transcripts) has not been written for fifteen minutes
  has stopped, whatever its Herdr status says. A single tool call that runs
  longer than that without writing reads as quiet too. Its clock ends at the
  last write and starts again when the transcript moves. Agents without a
  transcript the plugin can find run until Herdr stops them.

The daemon checks these every 30 seconds, however busy Herdr is.

Each pending block is settled on its own. One Chronoid refuses, or that names a
project the map no longer has, is dropped with a log line. One that overlaps an
existing entry is skipped as already covered. Only a Chronoid that cannot be
reached holds the queue for a later tick.

State lives in `~/.local/state/herdr-chronoid/` (or `$XDG_STATE_HOME`):
`state.json` (open intervals, pending blocks), `agent-hours.jsonl`, `daemon.log`
(set aside as `daemon.log.1` past 10 MB at startup), `daemon.pid`.

## Commands

```sh
herdr-chronoid status    # what is being tracked, what is waiting to be written
herdr-chronoid startup   # start the daemon unless it is already running
herdr-chronoid daemon    # run it in the foreground
```

Herdr also gets two actions, "Show Chronoid tracking status" and "Restart
Chronoid tracking daemon".

## Backfill

Time from before the plugin ran is rebuilt from the agents' own session
history: Claude Code (`~/.claude/projects`), Codex (`~/.codex/sessions`) and
OpenCode (`~/.local/share/opencode/opencode.db`). Sessions are split wherever
the agent changed directory, so each stretch is filed under the repository it
was actually in.

- **Agent runtime** is each turn: from your prompt (or the agent waking on its
  own) to its last activity before you spoke again, cut at 15 minutes of
  silence. It goes to the agent hours ledger, like live agent time. Claude Code
  sessions are attributed with the branch their transcript records, not the
  branch the directory is on today.
- **Attention** is estimated from your prompts across every agent on one
  timeline: the time since your previous prompt, up to 5 minutes, counts toward
  the next one; a prompt after a longer gap counts 2 minutes. It can never add
  up to more than the wall clock. Claude Code prompts come from
  `~/.claude/history.jsonl`, which reaches back further than the transcripts
  Claude Code deletes after 30 days. On days Chronoid was tracking, the entry
  overlays the tracked activity; on other days it is an `offline_gap`.

A directory that has moved since is attributed at its new place with
`--moved OLD=NEW`. A worktree that has since been removed is attributed through
its repository, the nearest directory that still exists.

```sh
herdr-chronoid backfill plan --since 2026-03-01   # read-only; writes backfill-plan.json
herdr-chronoid backfill plan --since 2026-03-01 --moved ~/code=~/git
herdr-chronoid backfill apply                     # agent runtime only
herdr-chronoid backfill apply --attention         # plus the attention left in the plan
```

Review the plan before you apply it. `apply` records each entry as written in
the plan, so re-running it resumes without duplicates. An entry Chronoid
refuses, or whose project the map lacks, stays unwritten and is tried again by
the next `apply`. Run it with the daemon stopped.

## License

MIT. See [LICENSE](LICENSE).
