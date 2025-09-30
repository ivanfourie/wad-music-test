//! midi.rs
//!
//! This module parses a Standard MIDI File (SMF) into a flat timeline of timestamped events.
//! Instead of leaving events in their original tracks and tick-based timing,
//! we convert them into absolute time in microseconds and normalize them into our own
//! `Msg` enum, which is easier to feed into a synthesizer.
//!
//! ### Quick primer on MIDI
//! - MIDI is a protocol for describing music, not audio. It’s just structured events.
//! - Events include things like "Note On", "Note Off", "Change Instrument", "Set Tempo".
//! - Each event may be associated with a channel (0–15), so you can have up to 16 instruments
//!   playing in parallel.
//! - Timing inside MIDI files is expressed in "ticks". To turn ticks into real time, we use
//!   the header’s pulses-per-quarter-note (PPQ) plus tempo events that say how many microseconds
//!   one quarter note lasts.
//!
//! This module takes care of:
//!  - Reading ticks + tempo and converting them to microsecond timestamps
//!  - Normalizing events like NoteOn with velocity=0 into NoteOff
//!  - Flattening multiple tracks into a single chronological event list

use midly::{MetaMessage, Smf, TrackEventKind};

/// A normalized MIDI message used by our playback engine.
///
/// We keep only the subset of MIDI messages we care about,
/// each with simplified fields (just u8 or u16).
#[derive(Clone, Copy, Debug)]
pub enum Msg {
    /// Start playing a note: (channel, key, velocity)
    NoteOn(u8, u8, u8),
    /// Stop playing a note: (channel, key, velocity)
    NoteOff(u8, u8, u8),
    /// Change instrument program on a channel
    Program(u8, u8),
    /// Generic MIDI controller change: (channel, controller number, value)
    Control(u8, u8, u8),
    /// Pitch bend wheel: (channel, bend value 0–16383, center=8192)
    PitchBend(u8, u16),
    /// Per-note aftertouch pressure
    AfterTouch(u8, u8, u8),
    /// Channel-wide aftertouch pressure
    ChannelAftertouch(u8, u8),
    /// Tempo change in microseconds per quarter note
    Tempo(f64),
}

/// A MIDI message tied to an absolute time in microseconds.
#[derive(Clone, Copy, Debug)]
pub struct Timed {
    pub t_us: u64,
    pub msg: Msg,
}

/// The full parsed result of a MIDI file.
pub struct Timeline {
    /// All events from all tracks, ordered by time
    pub events: Vec<Timed>,
    /// Time of the last event (in µs)
    pub last_t_us: u64,
    /// Pulses per quarter note (from header)
    pub ppq: f64,
    /// Initial tempo if no tempo event is given
    pub initial_us_per_qn: f64,
}

/// Build a linear timeline of events from a parsed MIDI file.
///
/// This does the following:
/// - Determine the file’s pulses-per-quarter-note (PPQ)
/// - Scan for an initial tempo (default 120 BPM if none)
/// - Walk through each track, accumulating delta ticks into absolute ticks
/// - Convert ticks into microseconds using the current tempo
/// - Collect MIDI events into our `Msg` enum
/// - Merge all tracks into one chronological event list
pub fn build_timeline(smf: &Smf<'_>) -> Timeline {
    // Pulses per quarter note (PPQ): needed to convert ticks to time
    let ppq = match smf.header.timing {
        midly::Timing::Metrical(t) => t.as_int() as f64,
        _ => 480.0, // fallback if SMPTE timing is used
    };

    // Default tempo: 500,000 µs per quarter note = 120 BPM
    let mut default_us_per_qn: f64 = 500_000.0;

    // Scan tracks for a Tempo meta event to seed initial tempo
    'scan: for tr in &smf.tracks {
        for ev in tr {
            if let TrackEventKind::Meta(MetaMessage::Tempo(tp)) = ev.kind {
                default_us_per_qn = tp.as_int() as f64;
                break 'scan;
            }
        }
    }

    let mut events = Vec::new();
    let mut counts = [0usize; 16];
    // Walk each track independently
    for tr in &smf.tracks {
        let mut abs_ticks: u64 = 0;
        let mut us_per_qn = default_us_per_qn;

        for ev in tr {
            // Convert delta ticks -> absolute ticks
            abs_ticks += ev.delta.as_int() as u64;

            // Convert ticks to absolute microseconds
            let t_sec = (abs_ticks as f64) / ppq * (us_per_qn / 1_000_000.0);
            let t_us = (t_sec * 1_000_000.0) as u64;
            
            match ev.kind {
                TrackEventKind::Meta(m) => {
                    if let MetaMessage::Tempo(tp) = m {
                        // Update tempo for subsequent events
                        us_per_qn = tp.as_int() as f64;
                        events.push(Timed { t_us, msg: Msg::Tempo(us_per_qn) });
                    }
                }
                TrackEventKind::Midi { channel, message } => {
                    let ch = u8::from(channel);
                    use midly::MidiMessage::*;
                    match message {
                        // NoteOn with velocity=0 is equivalent to NoteOff
                        NoteOn { key, vel } if vel.as_int() == 0 => {
                            events.push(Timed { t_us, msg: Msg::NoteOff(ch, key.as_int(), 0) });
                            if vel.as_int() > 0 { counts[u8::from(channel) as usize] += 1; }
                        }
                        NoteOn { key, vel } => {
                            events.push(Timed { t_us, msg: Msg::NoteOn(ch, key.as_int(), vel.as_int()) });
                        }
                        NoteOff { key, vel } => {
                            events.push(Timed { t_us, msg: Msg::NoteOff(ch, key.as_int(), vel.as_int()) });
                        }
                        ProgramChange { program } => {
                            events.push(Timed { t_us, msg: Msg::Program(ch, program.as_int()) });
                        }
                        Controller { controller, value } => {
                            events.push(Timed { t_us, msg: Msg::Control(ch, controller.as_int(), value.as_int()) });
                        }
                        PitchBend { bend } => {
                            let raw = bend.0.as_int();
                            events.push(Timed { t_us, msg: Msg::PitchBend(ch, raw) });
                        }
                        Aftertouch { key, vel } => {
                            events.push(Timed { t_us, msg: Msg::AfterTouch(ch, key.as_int(), vel.as_int()) });
                        }
                        ChannelAftertouch { vel } => {
                            events.push(Timed { t_us, msg: Msg::ChannelAftertouch(ch, vel.as_int()) });
                        }
                    }
                }
                _ => {}
            }
        }
        eprintln!("NoteOn counts per MIDI ch: {:?}", counts);
    }

    // Merge all tracks into a single sorted timeline
    events.sort_by_key(|e| e.t_us);
    let last_t_us = events.last().map(|e| e.t_us).unwrap_or(0);

    Timeline { events, last_t_us, ppq, initial_us_per_qn: default_us_per_qn }
}

/// Format a microsecond timestamp as MM:SS string for logging/debugging.
pub fn format_duration(us: u64) -> String {
    let total_secs = us / 1_000_000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{:02}:{:02}", mins, secs)
}
