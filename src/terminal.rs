//! Terminal control thread. The *only* thing this thread ever does is block
//! in `ReadFile` on stdin and send a `Command`. On EOF (player launched
//! detached, terminal closed) it parks forever — no retry loop, no spin.

use std::io::{self, BufRead};
use std::thread::{self, JoinHandle};

use crossbeam_channel::Sender;

use crate::broker::{Command, Source, EQ_BANDS, EQ_BAND_HZ};
use crate::{log_debug, log_info, log_warn};

pub fn spawn(cmd_tx: Sender<Command>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("kyouko-terminal".into())
        .spawn(move || {
            let stdin = io::stdin();
            log_info!("TERM", "ready — type 'help' for commands");
            loop {
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) => {
                        log_info!("TERM", "stdin closed — parking thread forever (zero polling)");
                        park_forever();
                    }
                    Ok(_) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        log_debug!("TERM", "input: {line}");
                        if let Some(cmd) = parse(line) {
                            if cmd_tx.send(cmd).is_ok() {
                                // Channels carry the data; one posted message
                                // carries the wake-up.
                                crate::ui::wake_broker();
                            } else {
                                log_warn!("TERM", "broker gone — parking");
                                park_forever();
                            }
                        }
                    }
                    Err(e) => {
                        log_warn!("TERM", "stdin read failed: {e} — parking");
                        park_forever();
                    }
                }
            }
        })
        .expect("spawn terminal thread")
}

/// `park()` may wake spuriously; looping keeps it a true `never` with zero CPU.
fn park_forever() -> ! {
    loop {
        thread::park();
    }
}

fn parse(line: &str) -> Option<Command> {
    let (head, rest) = match line.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (line, ""),
    };
    match head {
        // Concise command keys (v2). Legacy long forms were removed on purpose
        // — `p`/`y`/`r`/`s`/`q` are the whole story, plus vol/eq/l/state/icon.
        "p" => {
            if rest.is_empty() {
                log_warn!("TERM", "usage: p <path>");
                None
            } else {
                // Tolerate shell-style quoting around paths with spaces.
                let path = rest
                    .strip_prefix('"')
                    .and_then(|p| p.strip_suffix('"'))
                    .unwrap_or(rest);
                Some(Command::Load {
                    source: Source::File(path.to_string()),
                    paused: false,
                })
            }
        }
        "y" => {
            let mut parts = rest.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some(url), fmt) => Some(Command::Load {
                    source: Source::Youtube {
                        url: url.to_string(),
                        format: fmt.unwrap_or("140").to_string(),
                    },
                    paused: false,
                }),
                _ => {
                    log_warn!("TERM", "usage: y <url> [format_id] (default 140 = m4a 128k)");
                    None
                }
            }
        }
        // One key for the whole play/pause/resume cycle (spacebar-style).
        "r" => Some(Command::TogglePause),
        "s" => Some(Command::Stop),
        "vol" | "volume" => match rest.parse::<f64>() {
            Ok(v) if (0.0..=100.0).contains(&v) => Some(Command::SetVolume((v / 100.0) as f32)),
            Ok(v) => {
                log_warn!("TERM", "vol {v} out of range (0-100)");
                None
            }
            Err(_) => {
                log_warn!("TERM", "usage: vol <0-100>");
                None
            }
        },
        "eq" => parse_eq(rest),
        "l" | "loop" => Some(Command::ToggleLoop),
        "state" | "dump" => Some(Command::DumpState),
        "icon" => {
            // Print the procedural tray glyph — see it without a screenshot.
            println!("{}", crate::ui::glyph::glyph_ascii());
            None
        }
        "q" => Some(Command::Quit),
        "help" | "?" => {
            print_help();
            None
        }
        _ => {
            log_warn!("TERM", "unknown command '{head}' — type 'help'");
            None
        }
    }
}

fn parse_eq(rest: &str) -> Option<Command> {
    let mut parts = rest.split_whitespace();
    let band = parts.next();
    let gain = parts.next();
    match (band, gain) {
        (Some("on"), _) => Some(Command::EqEnabled(true)),
        (Some("off"), _) => Some(Command::EqEnabled(false)),
        (Some("reset"), _) => Some(Command::EqGain { band: None, gain_db: 0.0 }),
        (Some(b), Some(g)) => {
            let band = if b == "all" {
                None
            } else {
                match b.parse::<usize>() {
                    Ok(i) if i < EQ_BANDS => Some(i),
                    _ => {
                        log_warn!("TERM", "eq band '{b}' invalid (0-{}, or 'all')", EQ_BANDS - 1);
                        return None;
                    }
                }
            };
            match g.parse::<f32>() {
                Ok(db) => Some(Command::EqGain { band, gain_db: db }),
                Err(_) => {
                    log_warn!("TERM", "eq gain '{g}' invalid (dB, {:+}..{:+})", -12.0, 12.0);
                    None
                }
            }
        }
        _ => {
            log_warn!("TERM", "usage: eq <0-9|all> <gain_dB> | eq on|off|reset");
            None
        }
    }
}

fn print_help() {
    let bands = EQ_BAND_HZ
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "commands:\n  \
         p <path>              load a local file (audio or video, decoded as audio)\n  \
         y <url> [format_id]   stream from YouTube via yt-dlp (default 140 = m4a 128k)\n  \
         r                     toggle play / pause / resume\n  \
         s                     stop playback\n  \
         vol <0-100>           set volume percent\n  \
         eq <0-9|all> <gain_dB> band gains {bands} Hz, {:+}..{:+} dB\n  \
         eq on | off | reset\n  \
         l / loop              toggle track repeat mode ON/OFF\n  \
         state                 dump full state\n  \
         icon                  print the tray glyph\n  \
         q                     graceful shutdown",
        -12.0,
        12.0
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concise_keys_map_to_commands() {
        assert!(matches!(
            parse("p C:\\m\\a.flac"),
            Some(Command::Load {
                source: Source::File(_),
                paused: false
            })
        ));
        assert!(matches!(
            parse("y https://youtu.be/x 251"),
            Some(Command::Load {
                source: Source::Youtube { format, .. },
                paused: false
            }) if format == "251"
        ));
        assert!(matches!(parse("y https://youtu.be/x"), Some(Command::Load { .. })));
        assert!(matches!(parse("r"), Some(Command::TogglePause)));
        assert!(matches!(parse("s"), Some(Command::Stop)));
        assert!(matches!(parse("q"), Some(Command::Quit)));
        assert!(matches!(parse("l"), Some(Command::ToggleLoop)));
        assert!(matches!(
            parse("vol 50"),
            Some(Command::SetVolume(v)) if (v - 0.5).abs() < 1e-6
        ));
        assert!(matches!(
            parse("eq 0 3"),
            Some(Command::EqGain {
                band: Some(0),
                gain_db
            }) if gain_db == 3.0
        ));
    }

    #[test]
    fn legacy_long_forms_are_gone() {
        for line in ["play x", "load x", "yt x", "pause", "resume", "toggle", "stop", "quit", "exit"] {
            assert!(parse(line).is_none(), "'{line}' must no longer parse");
        }
    }

    #[test]
    fn quoted_paths_with_spaces_survive() {
        match parse("p \"C:\\My Music\\a b.flac\"") {
            Some(Command::Load {
                source: Source::File(p),
                ..
            }) => assert_eq!(p, "C:\\My Music\\a b.flac"),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(parse("p").is_none(), "bare p needs a path");
    }
}
