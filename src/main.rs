mod activity;
mod agent_hours;
mod backfill;
mod chronoid;
mod clock;
mod herdr;
mod ledger;
mod log;
mod map;
mod paths;
mod presence;
mod state;
mod tracker;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use state::State;
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracker::Tracker;

/// How often the daemon runs its periodic work, however busy Herdr is.
const TICK: i64 = 30;
/// A pause this long between two passes through the event loop means the
/// process was suspended: the machine slept. Nothing that was open then was
/// being worked on.
const SLEPT_AFTER: i64 = 120;
/// Above this size the log is set aside at startup, keeping one old copy.
const LOG_LIMIT: u64 = 10 * 1024 * 1024;

#[derive(Parser)]
#[command(about = "Report Herdr pane time into Chronoid", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Track panes and write Chronoid entries until stopped.
    Daemon,
    /// Start the daemon unless it is already running (Herdr's plugin startup).
    Startup,
    /// Show what is being tracked and what is waiting to be written.
    Status,
    /// Rebuild time from before the plugin ran, out of agent session history.
    Backfill {
        #[command(subcommand)]
        step: BackfillStep,
    },
}

#[derive(Subcommand)]
enum BackfillStep {
    /// Reconstruct agent runtime and attention into a plan file for review.
    Plan {
        /// First day to rebuild, as YYYY-MM-DD.
        #[arg(long)]
        since: String,
        /// A directory that has since moved, as OLD=NEW, so history recorded
        /// under OLD is attributed at NEW. Repeat for several.
        #[arg(long, value_name = "OLD=NEW")]
        moved: Vec<String>,
    },
    /// Write the plan's entries to Chronoid. Agent runtime only, unless
    /// --attention says the reviewed attention should go too.
    Apply {
        #[arg(long)]
        attention: bool,
    },
}

static STOPPING: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon => daemon(),
        Command::Startup => startup(),
        Command::Status => status(),
        Command::Backfill { step: BackfillStep::Plan { since, moved } } => {
            let format = time::macros::format_description!("[year]-[month]-[day]");
            let since = time::Date::parse(&since, format).context("--since must be YYYY-MM-DD")?;
            let moved = moved
                .iter()
                .map(|pair| backfill::Move::parse(pair))
                .collect::<Result<Vec<_>>>()?;
            backfill::plan(since, &moved)
        }
        Command::Backfill { step: BackfillStep::Apply { attention } } => {
            if let Some(pid) = running_daemon() {
                anyhow::bail!("stop the tracking daemon (pid {pid}) first: it writes the same state file");
            }
            backfill::apply(attention)
        }
    }
}

fn daemon() -> Result<()> {
    std::fs::create_dir_all(paths::state_dir()).context("create the state directory")?;
    std::fs::write(paths::pid_file(), std::process::id().to_string()).context("write the pid file")?;
    install_signal_handlers();

    let mut tracker = Tracker::new(State::load()?, map::Map::load()?);
    tracker.close_stale();
    log::line("daemon started");

    while !STOPPING.load(Ordering::Relaxed) {
        if let Err(error) = follow(&mut tracker) {
            log::line(format!("restarting the event stream: {error:#}"));
            tracker.close_stale();
            let _ = tracker.state.save();
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    let now = clock::now();
    let panes: Vec<String> = tracker.state.open.keys().cloned().collect();
    for pane_id in panes {
        tracker.forget(&pane_id, now);
    }
    tracker.flush_all(now);
    tracker.state.last_seen = Some(now);
    tracker.state.save()?;
    log::line("daemon stopped");
    let _ = std::fs::remove_file(paths::pid_file());
    Ok(())
}

/// One connection's worth of tracking. Returns on any stream error so the
/// caller can reconnect; a Herdr restart is not a reason to lose the day.
fn follow(tracker: &mut Tracker) -> Result<()> {
    let mut events = herdr::Events::subscribe(Duration::from_secs(TICK as u64))?;
    let now = clock::now();
    for pane in herdr::pane_list()? {
        tracker.apply(&pane, now);
    }
    tracker.state.last_seen = Some(now);
    tracker.tick(now, presence::input_idle_seconds());
    tracker.flush(now);
    tracker.state.save()?;
    // Writing can take a while; the sleep check measures time spent waiting.
    tracker.state.last_seen = Some(clock::now());
    let mut next_tick = now + TICK;

    while !STOPPING.load(Ordering::Relaxed) {
        // `None` is a read timeout or an event the tracker does not use.
        let event = events.next()?;
        let now = clock::now();
        let paused = now - tracker.state.last_seen.unwrap_or(now);
        if paused > SLEPT_AFTER {
            // The caller closes everything at the last moment seen.
            anyhow::bail!("nothing ran for {}: the machine slept", clock::duration(paused));
        }
        tracker.state.last_seen = Some(now);
        match event {
            Some(herdr::Event::Updated(pane)) => tracker.apply(&pane, now),
            Some(herdr::Event::Gone(pane_id)) => tracker.forget(&pane_id, now),
            None => {}
        }
        if now >= next_tick {
            tracker.tick(now, presence::input_idle_seconds());
            tracker.flush(now);
            tracker.state.save()?;
            tracker.state.last_seen = Some(clock::now());
            next_tick = now + TICK;
        }
    }
    Ok(())
}

fn startup() -> Result<()> {
    if let Some(pid) = running_daemon() {
        println!("herdr-chronoid daemon already running (pid {pid})");
        return Ok(());
    }
    let executable = std::env::current_exe().context("find this executable")?;
    std::fs::create_dir_all(paths::state_dir()).context("create the state directory")?;
    if std::fs::metadata(paths::log_file()).is_ok_and(|log| log.len() > LOG_LIMIT) {
        std::fs::rename(paths::log_file(), paths::log_file().with_extension("log.1")).context("set the old log aside")?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::log_file())
        .context("open the daemon log")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    unsafe {
        // Herdr's startup hook is short-lived; the daemon needs its own session
        // to outlive it.
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().context("start the daemon")?;
    println!("herdr-chronoid daemon started (pid {})", child.id());
    Ok(())
}

fn status() -> Result<()> {
    let state = State::load()?;
    match running_daemon() {
        Some(pid) => println!("daemon running (pid {pid})"),
        None => println!("daemon not running"),
    }

    let idle = presence::input_idle_seconds();
    if idle >= tracker::AWAY_AFTER {
        println!("away: no input for {}, so no attention is recorded", clock::duration(idle));
    }

    let now = clock::now();
    println!("\nTracking now:");
    let mut tracking = false;
    for (pane_id, open) in &state.open {
        for (label, since) in [("attention", open.attention_since), ("agent", open.agent_since)] {
            if let Some(since) = since {
                tracking = true;
                println!(
                    "  {label:9} {:20} {:>7}  {} ({pane_id})",
                    open.project,
                    clock::duration(now - since),
                    open.title.as_deref().unwrap_or("-")
                );
            }
        }
    }
    if !tracking {
        println!("  nothing");
    }

    println!("\nWaiting to be written:");
    if state.pending.is_empty() {
        println!("  nothing");
    }
    for block in &state.pending {
        println!(
            "  {:9} {:20} {:>7}  {}",
            block.clock.label(),
            block.project,
            clock::duration(block.seconds()),
            block.title()
        );
    }

    let agent_hours = std::fs::read_to_string(agent_hours::path()).unwrap_or_default().lines().count();
    println!("\nAgent hours recorded: {agent_hours} blocks in {}", agent_hours::path().display());
    println!("Project map: {}", map::path().display());
    println!("Log: {}", paths::log_file().display());
    Ok(())
}

fn running_daemon() -> Option<u32> {
    let pid: i32 = std::fs::read_to_string(paths::pid_file()).ok()?.trim().parse().ok()?;
    // Signal 0 asks whether the process is there without touching it.
    (unsafe { libc::kill(pid, 0) } == 0).then_some(pid as u32)
}

fn install_signal_handlers() {
    extern "C" fn stop(_: libc::c_int) {
        STOPPING.store(true, Ordering::Relaxed);
    }
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
}
