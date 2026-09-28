//! Command-line parsing, kept separate so it can be tested without running
//! anything.

use std::path::PathBuf;

pub const USAGE: &str = "\
audiowatch -- says which process put audio on this Mac's outputs.

  audiowatch                     watch, and notify about anything unexpected
  audiowatch --tail [N]          the last N sound events (default 20)
  audiowatch --tail [N] --follow ... and keep printing new ones
  audiowatch --since <when>      events since 30m, 2h, 7d, or 2026-09-26T11:00
  audiowatch --now               what is making sound at this instant
  audiowatch --devices           audio devices, with channel counts
  audiowatch --test-notify       post a notification and check it was delivered
  audiowatch --install-agent     write the LaunchAgent so it runs at login
  audiowatch --paths             where the config and the log live
  audiowatch --mcp [--port N] [--dir DIR]
                                 serve the recorder over MCP (default port 3929)
  audiowatch rec list|clocks|record ...
                                 record channels of any audio device to WAV

Options
  --all                 include connect/disconnect events, not just sound
  --config <path>       use a different config file
  --quiet               no stdout while watching (the log is still written)
  -h, --help            this

The log is the primary output; notifications are a convenience. Add a line to
the config file to stop something notifying -- `audiowatch --paths` says where.
";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Watch,
    Tail { last: usize, follow: bool },
    Since { spec: String },
    Now,
    Devices,
    TestNotify,
    InstallAgent,
    Paths,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub command: Command,
    pub config: Option<PathBuf>,
    /// Include connect/disconnect chatter in `--tail` / `--since`.
    pub all: bool,
    pub quiet: bool,
}

pub const DEFAULT_TAIL: usize = 20;

/// Parse the argument list, excluding the program name.
pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut command: Option<Command> = None;
    let mut config = None;
    let mut all = false;
    let mut quiet = false;
    let mut follow = false;
    let mut tail_count: Option<usize> = None;

    let set = |c: Command, existing: &mut Option<Command>| -> Result<(), String> {
        if let Some(prev) = existing {
            return Err(format!("{prev:?} and {c:?} cannot both be asked for"));
        }
        *existing = Some(c);
        Ok(())
    };

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" | "help" => set(Command::Help, &mut command)?,
            "--watch" | "watch" => set(Command::Watch, &mut command)?,
            "--now" | "now" => set(Command::Now, &mut command)?,
            "--devices" | "devices" => set(Command::Devices, &mut command)?,
            "--test-notify" => set(Command::TestNotify, &mut command)?,
            "--install-agent" => set(Command::InstallAgent, &mut command)?,
            "--paths" => set(Command::Paths, &mut command)?,
            "--tail" | "tail" => {
                set(
                    Command::Tail {
                        last: DEFAULT_TAIL,
                        follow: false,
                    },
                    &mut command,
                )?;
                // An optional count may follow, as `--tail 50`.
                if let Some(next) = args.get(i + 1) {
                    if let Ok(n) = next.parse::<usize>() {
                        tail_count = Some(n);
                        i += 1;
                    }
                }
            }
            "--since" | "since" => {
                let spec = args
                    .get(i + 1)
                    .ok_or_else(|| "--since needs a time, e.g. --since 2h".to_string())?;
                set(Command::Since { spec: spec.clone() }, &mut command)?;
                i += 1;
            }
            "-n" => {
                let n = args
                    .get(i + 1)
                    .ok_or_else(|| "-n needs a number".to_string())?;
                tail_count = Some(
                    n.parse()
                        .map_err(|_| format!("-n needs a number, not '{n}'"))?,
                );
                i += 1;
            }
            "-f" | "--follow" => follow = true,
            "--all" => all = true,
            "--quiet" | "-q" => quiet = true,
            "--config" => {
                let p = args
                    .get(i + 1)
                    .ok_or_else(|| "--config needs a path".to_string())?;
                config = Some(crate::config::expand_tilde(p));
                i += 1;
            }
            other => {
                return Err(format!("unknown argument '{other}' -- try --help"));
            }
        }
        i += 1;
    }

    let mut command = command.unwrap_or(Command::Watch);
    if let Command::Tail { last, follow: f } = &mut command {
        *last = tail_count.unwrap_or(DEFAULT_TAIL);
        *f = follow;
    } else if tail_count.is_some() {
        return Err("-n only makes sense with --tail".to_string());
    }

    Ok(Args {
        command,
        config,
        all,
        quiet,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_arguments_means_watch() {
        let a = p(&[]).unwrap();
        assert_eq!(a.command, Command::Watch);
        assert!(!a.quiet);
        assert_eq!(a.config, None);
    }

    #[test]
    fn tail_takes_a_count_either_way_and_defaults_to_twenty() {
        assert_eq!(
            p(&["--tail"]).unwrap().command,
            Command::Tail {
                last: 20,
                follow: false
            }
        );
        assert_eq!(
            p(&["--tail", "50"]).unwrap().command,
            Command::Tail {
                last: 50,
                follow: false
            }
        );
        assert_eq!(
            p(&["--tail", "-n", "5"]).unwrap().command,
            Command::Tail {
                last: 5,
                follow: false
            }
        );
        assert_eq!(
            p(&["tail"]).unwrap().command,
            Command::Tail {
                last: 20,
                follow: false
            }
        );
    }

    #[test]
    fn follow_works_in_both_spellings_and_either_order() {
        assert_eq!(
            p(&["--tail", "-f"]).unwrap().command,
            Command::Tail {
                last: 20,
                follow: true
            }
        );
        assert_eq!(
            p(&["--follow", "--tail", "3"]).unwrap().command,
            Command::Tail {
                last: 3,
                follow: true
            }
        );
    }

    #[test]
    fn since_captures_its_argument() {
        assert_eq!(
            p(&["--since", "2h"]).unwrap().command,
            Command::Since { spec: "2h".into() }
        );
        assert_eq!(
            p(&["since", "2026-09-26T11:00"]).unwrap().command,
            Command::Since {
                spec: "2026-09-26T11:00".into()
            }
        );
        assert!(p(&["--since"]).unwrap_err().contains("needs a time"));
    }

    #[test]
    fn a_count_after_since_is_not_swallowed_as_a_tail_count() {
        // `--since 30` is a bad time spec, not a tail count -- the error comes
        // later, from the time parser, but the argument must reach it.
        assert_eq!(
            p(&["--since", "30"]).unwrap().command,
            Command::Since { spec: "30".into() }
        );
    }

    #[test]
    fn every_mode_has_a_flag_and_a_bare_word_where_it_reads_naturally() {
        assert_eq!(p(&["--now"]).unwrap().command, Command::Now);
        assert_eq!(p(&["now"]).unwrap().command, Command::Now);
        assert_eq!(p(&["--devices"]).unwrap().command, Command::Devices);
        assert_eq!(p(&["--test-notify"]).unwrap().command, Command::TestNotify);
        assert_eq!(
            p(&["--install-agent"]).unwrap().command,
            Command::InstallAgent
        );
        assert_eq!(p(&["--paths"]).unwrap().command, Command::Paths);
        assert_eq!(p(&["-h"]).unwrap().command, Command::Help);
    }

    #[test]
    fn options_are_picked_up_alongside_a_command() {
        let a = p(&["--tail", "5", "--all", "--quiet", "--config", "/tmp/x.conf"]).unwrap();
        assert_eq!(
            a.command,
            Command::Tail {
                last: 5,
                follow: false
            }
        );
        assert!(a.all);
        assert!(a.quiet);
        assert_eq!(a.config, Some(PathBuf::from("/tmp/x.conf")));
    }

    #[test]
    fn a_config_path_may_use_a_tilde() {
        let a = p(&["--config", "~/x.conf"]).unwrap();
        assert!(a.config.unwrap().is_absolute());
    }

    #[test]
    fn two_commands_at_once_is_an_error_rather_than_a_silent_choice() {
        assert!(p(&["--now", "--devices"]).is_err());
        assert!(p(&["--tail", "--now"]).is_err());
    }

    #[test]
    fn nonsense_is_rejected_with_a_pointer_to_help() {
        let e = p(&["--frobnicate"]).unwrap_err();
        assert!(e.contains("--frobnicate"), "{e}");
        assert!(e.contains("--help"), "{e}");
        assert!(p(&["-n", "x"]).is_err());
        assert!(p(&["--config"]).is_err());
        assert!(p(&["-n", "5"])
            .unwrap_err()
            .contains("only makes sense with --tail"));
    }

    #[test]
    fn usage_mentions_every_command_it_offers() {
        for flag in [
            "--tail",
            "--since",
            "--now",
            "--devices",
            "--test-notify",
            "--install-agent",
            "--paths",
            "--all",
            "--config",
            "--quiet",
        ] {
            assert!(USAGE.contains(flag), "usage should document {flag}");
        }
    }
}
