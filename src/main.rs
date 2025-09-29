use anyhow::Result;
use clap::Parser;
use std::{io::{stdin, stdout, Write}, path::PathBuf, thread, time::Duration};

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{enable_raw_mode, disable_raw_mode};


mod wad;
use wad::Wad;

use crate::mus::mus_to_smf;
mod mus;

mod midi;
mod synth;

use midi::{build_timeline, format_duration};
use synth::Audio;

#[derive(Parser, Debug)]
struct Opt {
    /// Path to DOOM or DOOM2 WAD
    wad: PathBuf,
    /// Path to GM SoundFont (.sf2)
    soundfont: String,
}

const MUSIC_PREFIXES: &[&str] = &["D_", "MUS_"];

/// Accepts: RUNNIN, D_RUNNIN, E1M1, MUS_E1M1, etc.
/// Tries exact, then tries with each known prefix.
fn find_song<'a>(names: &'a [String], input: &str) -> Option<&'a str> {
    let q = input.trim().to_ascii_uppercase();
    if q.is_empty() { return None; }

    if let Some(hit) = names.iter().find(|n| **n == q) {
        return Some(hit.as_str());
    }
    for p in MUSIC_PREFIXES {
        let cand = format!("{}{}", p, q);
        if let Some(hit) = names.iter().find(|n| **n == cand) {
            return Some(hit.as_str());
        }
    }
    None
}

struct RawGuard;
impl RawGuard {
    fn enter() -> anyhow::Result<Self> { enable_raw_mode()?; Ok(Self) }
}
impl Drop for RawGuard {
    fn drop(&mut self) { let _ = disable_raw_mode(); }
}


fn main() -> Result<()> {
    let opt = Opt::parse();
    let mut wad = Wad::open(&opt.wad)?;
    println!("Using SoundFont: {}", opt.soundfont);

    let music_lumps: Vec<_> = wad.iter_with_prefixes(MUSIC_PREFIXES).collect();

    println!("\nAvailable songs:");
    for l in &music_lumps {
        println!("  {} ({} bytes)", l.name, l.size);
    }

    let music_names: Vec<String> = music_lumps.iter().map(|l| l.name.clone()).collect();

    // REPL: type a song name (RUNNIN or D_RUNNIN). Empty line quits.
    loop {
        print!("\n> Enter song (RUNNIN / E1M1), 'list' to show all, or empty to quit: ");

        stdout().flush().ok();

        let mut line = String::new();
        if stdin().read_line(&mut line).is_err() {
            break; // EOF or input error
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }

        if line.eq_ignore_ascii_case("list") {
            println!("\nAvailable songs:");
            for name in &music_names {
                println!("  {}", name);
            }
            continue;
        }

        let Some(candidate) = find_song(&music_names, line) else {
            println!("Not found. Suggestions:");
            let q = line.to_ascii_uppercase();
            for n in music_names.iter().filter(|n| n.contains(&q)).take(6) {
                println!("  {}", n);
            }
            continue;
        };

        // Read lump
        let bytes = match wad.read(&candidate) {
            Ok(b) => b,
            Err(e) => {
                println!("Failed to read {}: {}", candidate, e);
                continue;
            }
        };
        println!("\nRead {}: {} bytes", candidate, bytes.len());

        // Format detector
        if bytes.starts_with(b"MUS\x1A") {
            println!("Format: MUS");
            let smf = match mus_to_smf(&bytes) {
                Ok(s) => s,
                Err(e) => {
                    println!("MUS parse error: {}", e);
                    continue;
                }
            };

            let tl = build_timeline(&smf);
            println!("PPQ: {}", tl.ppq);
            println!(
                "Initial tempo: {} µs/qn (~{:.1} BPM)",
                tl.initial_us_per_qn,
                60_000_000.0 / tl.initial_us_per_qn
            );
            println!("Total events parsed: {}", tl.events.len());
            println!("Estimated track length: {}", format_duration(tl.last_t_us));

            let audio = match Audio::new(&opt.soundfont) {
                Ok(a) => a,
                Err(e) => { println!("Audio init failed: {}", e); continue; }
            };
            if let Err(e) = audio.start() {
                println!("Audio start failed: {}", e);
                continue;
            }

            // start playback and get a handle
            let player = audio.play_timeline(&tl);
            // enter raw mode to capture keys immediately
            // raw mode guard
            let _raw = RawGuard::enter()?;
            println!("Controls: Space = pause/resume, Esc = stop");

            loop {
                // quit this loop if the song finished by itself
                if player.is_finished() {
                    println!("Playback finished.");
                    break;
                }

                // poll for key events with a short timeout
                if event::poll(std::time::Duration::from_millis(50))? {
                    if let Event::Key(k) = event::read()? {
                        match k.code {
                            KeyCode::Char(' ') => {
                                player.toggle();
                                // Optional: visual feedback
                                // println!("[{}]", if paused { "paused" } else { "playing" });
                            }
                            KeyCode::Esc => {
                                player.stop(); // stop current song
                                break;
                            }
                            KeyCode::Char('c') if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                player.stop(); // stop current song
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                // small idle sleep to keep CPU down
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        } else if bytes.starts_with(b"MThd") {
            println!("Format: Standard MIDI (not handled here yet).");
        } else {
            println!("Format: unknown");
        }
    }

    Ok(())
}
