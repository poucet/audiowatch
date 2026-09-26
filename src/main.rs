//! audiowatch -- watch which processes put audio on this Mac's outputs.
//!
//! See README.md for what it detects and, more importantly, what it cannot.

mod agent;
mod bridge;
mod cli;
mod config;
mod event;
mod filter;
mod hal;
mod logfile;
mod notify;
mod proc;
mod reconcile;
mod sys;
mod timefmt;
mod watch;

use cli::Command;
use logfile::{Log, Query};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match cli::parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("audiowatch: {e}");
            return ExitCode::from(2);
        }
    };
    match run(parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("audiowatch: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: cli::Args) -> Result<(), String> {
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);

    match args.command {
        Command::Help => {
            print!("{}", cli::USAGE);
            Ok(())
        }
        Command::Paths => {
            let (cfg, created) = config::load_or_create(&config_path)?;
            println!(
                "config  {}{}",
                config_path.display(),
                if created { "  (just created)" } else { "" }
            );
            println!("log     {}", cfg.log_path.display());
            println!("agent   {}", agent::plist_path().display());
            println!("rules   {} allow rules", cfg.allow.len());
            Ok(())
        }
        Command::Devices => {
            for d in hal::CoreAudio::new().devices() {
                println!(
                    "{:<34} out {:>2}ch  in {:>2}ch  {}",
                    d.name,
                    d.output_channels,
                    d.input_channels,
                    if d.running_somewhere {
                        "RUNNING"
                    } else {
                        "idle"
                    }
                );
                if let Some(uid) = d.uid {
                    println!("{:>36}{uid}", "");
                }
            }
            Ok(())
        }
        Command::Now => {
            watch::print_now(&hal::CoreAudio::new());
            Ok(())
        }
        Command::TestNotify => test_notify(),
        Command::InstallAgent => install_agent(&config_path),
        Command::Tail { last, follow } => {
            let (cfg, _) = config::load_or_create(&config_path)?;
            show(
                &cfg.log_path,
                Query {
                    last: Some(last),
                    kinds: kinds_for(args.all),
                    ..Default::default()
                },
            )?;
            if follow {
                println!("-- following {} --", cfg.log_path.display());
                logfile::follow(&cfg.log_path, std::time::Duration::from_millis(250), |r| {
                    if args.all || Query::sound_kinds().contains(&r.kind) {
                        println!("{}", r.human());
                    }
                })
                .map_err(|e| format!("cannot follow {}: {e}", cfg.log_path.display()))?;
            }
            Ok(())
        }
        Command::Since { spec } => {
            let (cfg, _) = config::load_or_create(&config_path)?;
            let since = timefmt::parse_since(&spec, timefmt::now_millis())?;
            println!("-- since {} --", timefmt::format_local(since));
            show(
                &cfg.log_path,
                Query {
                    since: Some(since),
                    kinds: kinds_for(args.all),
                    ..Default::default()
                },
            )
        }
        Command::Watch => {
            let (cfg, created) = config::load_or_create(&config_path)?;
            if created && !args.quiet {
                println!(
                    "audiowatch: wrote a default config to {}",
                    config_path.display()
                );
            }
            watch::Watcher::new(cfg, args.quiet)?.run()
        }
    }
}

fn kinds_for(all: bool) -> Option<Vec<event::Kind>> {
    if all {
        None
    } else {
        Some(Query::sound_kinds())
    }
}

fn show(log_path: &std::path::Path, query: Query) -> Result<(), String> {
    let log =
        Log::load(log_path).map_err(|e| format!("cannot read {}: {e}", log_path.display()))?;
    let hits = log.select(&query);
    if hits.is_empty() {
        println!("nothing in {}", log_path.display());
    }
    for r in hits {
        println!("{}", r.human());
    }
    if log.unreadable > 0 {
        eprintln!(
            "audiowatch: {} unreadable line(s) in the log",
            log.unreadable
        );
    }
    Ok(())
}

fn test_notify() -> Result<(), String> {
    let mut record = event::Record::new(
        timefmt::now_millis(),
        event::Kind::OutputStart,
        std::process::id() as i32,
    );
    record.exe = sys::pid_path(std::process::id() as i32);
    record.devices = hal::CoreAudio::new()
        .devices()
        .into_iter()
        .filter(|d| d.output_channels > 0)
        .map(|d| d.name)
        .take(1)
        .collect();
    record.note = Some("this is audiowatch testing its own notifications".into());
    let message = notify::message_for(&record);
    println!(
        "posting:\n  title    {}\n  subtitle {}\n  body     {}",
        message.title, message.subtitle, message.body
    );
    notify::post(&message)?;
    // Notification Center takes a moment to pass it through usernoted.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    match notify::recently_delivered(30) {
        Ok(true) => {
            println!(
                "\nverified: Notification Center delivered it (usernoted logged the presentation)."
            );
            Ok(())
        }
        Ok(false) => Err(
            "osascript accepted the notification but Notification Center did not \
report delivering it.\n  Check System Settings > Notifications > Script Editor is allowed, and \
that Do Not Disturb is off."
                .to_string(),
        ),
        Err(e) => {
            println!("\nposted, but could not check the system log: {e}");
            Ok(())
        }
    }
}

fn install_agent(config_path: &std::path::Path) -> Result<(), String> {
    let binary: PathBuf = std::env::current_exe()
        .map_err(|e| format!("cannot find my own path: {e}"))?
        .canonicalize()
        .map_err(|e| format!("cannot resolve my own path: {e}"))?;
    if binary.starts_with(std::env::temp_dir())
        || binary.to_string_lossy().contains("/target/debug/")
    {
        eprintln!(
            "audiowatch: warning -- installing the agent to point at {}\n\
             \x20 That path will stop working if you rebuild or move the source tree.\n\
             \x20 Consider `cargo build --release` and copying target/release/audiowatch somewhere stable first.",
            binary.display()
        );
    }
    // Make sure the config exists before the agent goes looking for it.
    let (_, created) = config::load_or_create(config_path)?;
    let (plist, commands) = agent::install(&binary, config_path)?;
    println!("wrote {}", plist.display());
    println!("  binary {}", binary.display());
    println!(
        "  config {}{}",
        config_path.display(),
        if created { "  (just created)" } else { "" }
    );
    println!("\nNot loaded. To start it, run:\n\n{commands}\n");
    Ok(())
}
