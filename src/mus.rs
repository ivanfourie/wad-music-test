// mus.rs
use anyhow::{bail, Context, Result};
use midly::{
    Header, Format, Timing, Smf,
    TrackEvent, TrackEventKind, MetaMessage, MidiMessage,
    num::{u4, u14, u15, u24},
};

pub fn mus_to_smf(mus: &[u8]) -> Result<Smf<'static>> {
    // Header: "MUS\x1A", score length, score start
    if mus.len() < 16 || &mus[0..4] != b"MUS\x1A" { bail!("not a MUS"); }

    let score_len   = u16::from_le_bytes([mus[4], mus[5]]) as usize;
    let score_start = u16::from_le_bytes([mus[6], mus[7]]) as usize;

    let end = score_start.checked_add(score_len)
        .context("score_start + score_len overflow")?;
    if end > mus.len() { bail!("MUS score range out of bounds"); }

    let stream = &mus[score_start..end];

    // Keep MUS ticks as PPQ=140
    let header = Header {
        format: Format::SingleTrack,
        timing: Timing::Metrical(u15::from(140)),
    };
    let mut track: Vec<TrackEvent<'static>> = Vec::new();

    // 140 BPM => 428_571 µs/qn
    track.push(TrackEvent {
        delta: 0.into(),
        kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(1_000_000))),
    });

    // Per-channel remembered velocity (DoomGeneric uses 127)
    let mut last_vel = [127u8; 16];

    let mut i = 0usize;
    let mut pending_delta: u32 = 0;

    while i < stream.len() {
        let ev = stream[i]; i += 1;

        // Bit 7 says there is a delta time field after this event
        let has_delta = (ev & 0x80) != 0;

        // Event type is bits 4..6 (3 bits). Do not include bit 7.
        let ty = (ev >> 4) & 0x07;
        let ch_mus = ev & 0x0F;
        let ch_midi = map_channel(ch_mus);
        let ch = u4::from(ch_midi);

        match ty {
            0 => { // Release note
                if i >= stream.len() { break; }
                let key = stream[i] & 0x7F; i += 1;
                push(&mut track, pending_delta, TrackEventKind::Midi {
                    channel: ch,
                    message: MidiMessage::NoteOff { key: key.into(), vel: 0.into() }
                });
                pending_delta = 0;
            }
            1 => { // Play note
                if i >= stream.len() { break; }
                let mut key = stream[i]; i += 1;

                // Velocity flag lives in bit 7 of the key byte
                let mut vel = last_vel[ch_mus as usize];
                if key & 0x80 != 0 {
                    key &= 0x7F;
                    if i >= stream.len() { break; }
                    vel = stream[i]; i += 1;
                    last_vel[ch_mus as usize] = vel;
                }
                // Clamp velocity to 127 (0x7F)
                vel = vel.min(127);

                push(&mut track, pending_delta, TrackEventKind::Midi {
                    channel: ch,
                    message: MidiMessage::NoteOn { key: key.into(), vel: vel.into() }
                });
                pending_delta = 0;
            }
            2 => { // Pitch wheel (7-bit, centered at 64)
                if i >= stream.len() { break; }
                let v = stream[i] as i32; i += 1;        // 0..255, center 128
                let centered = v - 128;                  // -128..+127
                let bend14 = (8192 + centered * 64)      // scale so ±128 → ≈ ±8192
                    .clamp(0, 16383) as u16;
                let bend = midly::num::u14::from(bend14);
                push(&mut track, pending_delta, TrackEventKind::Midi {
                    channel: ch,
                    message: MidiMessage::PitchBend { bend: midly::PitchBend(bend) }
                });
                pending_delta = 0;
            }
            3 => {
                // System event: one data byte follows. We don't use it for GM, but we MUST consume it.
                //eprintln!("MUS System event at offset {}", i - 1);
                if i >= stream.len() { break; }
                let _sys = stream[i]; // values are DMX-internal (e.g. score markers), ignore for playback
                i += 1;
                // no pending_delta reset here; treat like other events (we already reset after pushing events).
            }
            4 => { // Controller
                if i >= stream.len() { break; }
                let ctrl = stream[i]; i += 1;

                match ctrl {
                    0 => { // Program change
                        if i >= stream.len() { break; }
                        let prog = stream[i]; i += 1;
                        push(&mut track, pending_delta, TrackEventKind::Midi {
                            channel: ch,
                            message: MidiMessage::ProgramChange { program: prog.into() }
                        });
                    }
                    _ => {
                        if i >= stream.len() { break; }
                        let val = stream[i]; i += 1;
                        if let Some(cc) = map_controller(ctrl) {
                            push(&mut track, pending_delta, TrackEventKind::Midi {
                                channel: ch,
                                message: MidiMessage::Controller {
                                    controller: cc.into(),
                                    value: val.into(),
                                }
                            });
                        }
                    }
                }
                pending_delta = 0;
            }
            5 => {
                // end of measure, no pauload */
            }
            6 => { // End of score
                // eprintln!("MUS end at offset {}", i - 1);
                break;
            }
            7 => { // Unused but historically includes 1 byte so players keep sync
                if i >= stream.len() { break; }
                let _ignored = stream[i]; i += 1;
            }
            _ => {
                // Unknown type. You can choose to bail or skip.
            }
        }

        // Only read a delta if the event's MSB was set
        if has_delta {
            let (d, used) = read_var_time(&stream[i..]);
            i += used;
            pending_delta = pending_delta.saturating_add(d);
        }
    }

    track.push(TrackEvent { delta: 0.into(), kind: TrackEventKind::Meta(MetaMessage::EndOfTrack) });
    Ok(Smf { header, tracks: vec![track] })
}

fn push(track: &mut Vec<midly::TrackEvent<'static>>, delta: u32, kind: midly::TrackEventKind<'static>) {
    track.push(midly::TrackEvent { delta: delta.into(), kind });
}

// MUS variable-length time is little-endian base-128.
// Each byte contributes 7 bits at increasing shifts. MSB=1 means more bytes follow.
fn read_var_time(bytes: &[u8]) -> (u32, usize) {
    let mut val: u32 = 0;
    let mut used = 0;
    for &b in bytes {
        used += 1;
        val = (val << 7) | ((b & 0x7F) as u32); // BIG-endian VLQ accumulation
        if b & 0x80 == 0 { break; }
    }
    (val, used)
}

fn map_channel(ch_mus: u8) -> u8 {
    // MUS channel 15 is drums. Map to GM channel 9.
    match ch_mus {
        15 => 9,                // drums
        c if c >= 9 => c+1, // 9..14 -> 10..15
        c => c,
    }
}

fn map_controller(c: u8) -> Option<u8> {
    Some(match c {
        1  => 0,   // bank select
        2  => 1,   // modulation
        3  => 7,   // volume
        4  => 10,  // pan
        5  => 11,  // expression
        6  => 91,  // reverb
        7  => 93,  // chorus
        8  => 64,  // sustain
        9  => 67,  // soft pedal
        10 => 120, // all sounds off
        11 => 123, // all notes off
        _  => return None,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use midly::{TrackEventKind, MidiMessage};

    #[test]
    fn test_valid_mus_to_midi_conversion() {
        // Minimal valid MUS lump: header + one note on + end
        // Header: "MUS\x1A", score_len, score_start, rest zero
        // Score: Play note event (type=1, ch=0), key=60; End of score (type=6)
        // Layout: [header (16 bytes)][score (3 bytes)]
        let score_start = 16u16;
        let score = [0x10, 60, 0x60]; // Play note, key=60; End of score
        let score_len = score.len() as u16;

        let mut mus = vec![
            b'M', b'U', b'S', 0x1A, // magic
            score_len as u8, (score_len >> 8) as u8, // score_len (LE)
            score_start as u8, (score_start >> 8) as u8, // score_start (LE)
        ];
        mus.extend_from_slice(&[0; 8]); // pad header to 16 bytes
        mus.extend_from_slice(&score); // score at offset 16

        let smf = mus_to_smf(&mus).expect("Should convert valid MUS lump");
        assert_eq!(smf.header.format, midly::Format::SingleTrack);
        assert_eq!(smf.tracks.len(), 1);
        let events = &smf.tracks[0];
        // Should contain at least: Tempo, NoteOn, EndOfTrack
        assert!(events.iter().any(|e| matches!(e.kind, TrackEventKind::Meta(_))));
        assert!(events.iter().any(|e| matches!(e.kind, TrackEventKind::Midi { message: MidiMessage::NoteOn { .. }, .. })));
        assert!(events.iter().any(|e| matches!(e.kind, TrackEventKind::Meta(midly::MetaMessage::EndOfTrack))));
    }
}
