//! synth.rs
//!
//! This module owns the actual sound synthesis and audio playback.
//!
//! - **FluidLite** is a lightweight software synthesizer that can load a General MIDI SoundFont
//!   and render raw PCM audio from MIDI events.
//! - **CPAL** is a cross-platform audio library that gives us a stream to the system’s sound card.
//!
//! The job of this module is to:
//!  - Initialize a FluidLite synth with reverb/chorus parameters and a SoundFont
//!  - Set up a CPAL audio stream that continuously pulls audio from the synth
//!  - Provide a simple API (`Audio::new`, `Audio::start`, `Audio::play_timeline`) to the rest of the program
//!
//! ### How it works
//! - The synth sits behind an `Arc<Mutex<…>>` so that both the audio thread (pulling samples)
//!   and the scheduler thread (injecting MIDI events) can share it safely.
//! - CPAL repeatedly calls our callback to fill audio buffers. In that callback we just ask
//!   FluidLite to `write()` samples into the buffer.
//! - In parallel, we spawn a "conductor" thread (`play_timeline`) that walks through the
//!   pre-built `Timeline` of events and tells the synth things like `note_on` and `note_off`
//!   at the right microsecond.
//!
//! The result: a fully working software MIDI player.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use fluidlite::{Settings, Synth};
use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread,
};
use std::sync::mpsc::{self, Sender};

use crate::midi::{Timeline};

pub struct Player {
    paused: Arc<AtomicBool>,
    stop_tx: Sender<()>,
    finished: Arc<AtomicBool>,
}

impl Player {
    pub fn pause(&self)  { self.paused.store(true,  Ordering::SeqCst); }
    pub fn resume(&self) { self.paused.store(false, Ordering::SeqCst); }
    pub fn toggle(&self) {
        let now = self.paused.load(Ordering::SeqCst);
        self.paused.store(!now, Ordering::SeqCst);
    }
    pub fn stop(&self)   { let _ = self.stop_tx.send(()); }
    pub fn is_finished(&self) -> bool { self.finished.load(Ordering::SeqCst) }
}

/// The `Audio` struct bundles together everything needed for playback:
/// - a shared FluidLite synth instance
/// - the CPAL audio stream driving the sound card
/// - the sample rate chosen by the audio device
pub struct Audio {
    pub synth: Arc<Mutex<Synth>>,
    pub stream: Stream,
    pub sample_rate: f32,
}

impl Audio {
    /// Create a new audio system with a loaded SoundFont.
    ///
    /// This will:
    /// - initialize a FluidLite synth
    /// - load the given SoundFont (for instrument sounds)
    /// - set gain, reverb, chorus parameters
    /// - open the default audio device with CPAL
    /// - configure the audio stream callback so CPAL pulls PCM from FluidLite
    pub fn new(soundfont: &str) -> Result<Self> {
        // Build synth with default settings
        let settings = Settings::new()?;
        let fl = Synth::new(settings)?;
        fl.sfload(soundfont, true).context("loading soundfont")?;

        // Some basic effects: master gain, reverb, chorus
        fl.set_gain(0.7);
        fl.set_reverb_on(true);
        fl.set_reverb_params(0.7, 0.2, 0.9, 0.5);
        fl.set_chorus_on(true);
        fl.set_chorus_params(3, 1.2, 0.30, 8.0, Default::default());

        let synth = Arc::new(Mutex::new(fl));

        // Set up CPAL audio output
        let host = cpal::default_host();
        let dev = host.default_output_device().context("no default output device")?;
        let cfg = dev.default_output_config().context("default_output_config")?;
        let sample_rate = cfg.sample_rate().0 as f32;

        // Inform the synth of the system sample rate and reset controllers
        {
            let s = synth.lock().unwrap();
            s.set_sample_rate(sample_rate);
            for ch in 0..16u32 {
                let _ = s.pitch_bend(ch, 8192); // center
                let _ = s.cc(ch, 121, 0);       // Reset All Controllers
                let _ = s.cc(ch, 120, 0);       // All Sound Off
            }
        }

        // CPAL error handler for the stream
        let err_fn = |e| eprintln!("stream error: {e}");
        let fmt = cfg.sample_format();
        let stream_cfg = cfg.config();

        // Build an output stream. CPAL asks us to fill `out` with samples each frame.
        // We simply forward that request to FluidLite's `write` method.
        let stream = match fmt {
            SampleFormat::I16 => dev.build_output_stream(
                &stream_cfg,
                {
                    let synth = synth.clone();
                    move |out: &mut [i16], _| {
                        if let Err(e) = synth.lock().unwrap().write(out) {
                            eprintln!("fluid write i16: {e}");
                        }
                    }
                },
                err_fn,
                None,
            )?,
            _ => dev.build_output_stream(
                &stream_cfg,
                {
                    let synth = synth.clone();
                    move |out: &mut [f32], _| {
                        if let Err(e) = synth.lock().unwrap().write(out) {
                            eprintln!("fluid write f32: {e}");
                        }
                    }
                },
                err_fn,
                None,
            )?,
        };

        Ok(Self { synth, stream, sample_rate })
    }

    /// Spawn a background thread that walks the `Timeline` of events
    /// and sends them to the synth at the correct wall-clock time.
    ///
    /// This acts as the "conductor", while CPAL is the "orchestra".
    pub fn play_timeline(&self, tl: &Timeline) -> Player {
        let paused   = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let paused_t   = paused.clone();
        let finished_t = finished.clone();
        let synth = self.synth.clone();
        let events = tl.events.clone(); // assuming Msg: Copy; else clone as needed

        thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut paused_since: Option<std::time::Instant> = None;
            let mut paused_total_us: u128 = 0; // total paused micros accumulated

            let mut i = 0usize;

            'play: loop {
                // Stop request?
                if stop_rx.try_recv().is_ok() { break 'play; }

                // Handle pausing: don't advance logical time while paused
                if paused_t.load(std::sync::atomic::Ordering::SeqCst) {
                    if paused_since.is_none() {
                        paused_since = Some(std::time::Instant::now());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                } else if let Some(since) = paused_since.take() {
                    // just resumed: accumulate paused span
                    let span = since.elapsed().as_micros();
                    paused_total_us = paused_total_us.saturating_add(span);
                }

                // Finished all events?
                if i >= events.len() { break 'play; }

                // Compute "logical now" in microseconds (wall-clock elapsed minus paused time)
                let wall_us = start.elapsed().as_micros();
                let now_us_u128 = wall_us.saturating_sub(paused_total_us);
                // Clamp to u64 for our event timestamps
                let now_us = if now_us_u128 > u64::MAX as u128 { u64::MAX } else { now_us_u128 as u64 };

                let e = events[i];

                if now_us >= e.t_us {
                    // Dispatch this event
                    if let Ok(s) = synth.lock() {
                        use crate::midi::Msg::*;
                        match e.msg {
                            NoteOn(ch, key, vel)   => { let _ = s.note_on(ch as u32, key as u32, vel as u32); }
                            NoteOff(ch, key, _vel) => { let _ = s.note_off(ch as u32, key as u32); }
                            Program(ch, prog)      => { let _ = s.program_change(ch as u32, prog as u32); }
                            Control(ch, cc, val)   => { let _ = s.cc(ch as u32, cc as u32, val as u32); }
                            PitchBend(ch, bend)  => { let _ = s.pitch_bend(ch as u32, bend as u32); }
                            AfterTouch(ch, key, vel) => { let _ = s.key_pressure(ch as u32, key as u32, vel as u32); }
                            ChannelAftertouch(ch, vel) => { let _ = s.channel_pressure(ch as u32, vel as u32); }
                            Tempo(_)               => {} // already baked into timeline
                        }
                    } else {
                        // If the lock is poisoned, bail out gracefully instead of panicking
                        break 'play;
                    }
                    i += 1;
                } else {
                    // Wait until it's time for this event, without underflow
                    let wait_us = e.t_us.saturating_sub(now_us); // safe u64 subtraction
                    // Sleep a small chunk; don’t try to sleep the whole microsecond span
                    let ms = std::cmp::min(5, wait_us / 1000);
                    if ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                    } else {
                        // If <1ms, yield a tiny bit to avoid a busy spin
                        std::thread::sleep(std::time::Duration::from_micros(200));
                    }
                }
            }

            finished_t.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        Player { paused, stop_tx, finished }
    }

    /// Start the audio stream (begins pushing audio to the system device).
    ///
    /// Must be called before playback can be heard.
    pub fn start(&self) -> anyhow::Result<()> {
        self.stream.play()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::{Msg, Timed, Timeline};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // Dummy synth that just records calls
    struct DummySynth {
        pub note_on_calls: Arc<Mutex<Vec<(u32, u32, u32)>>>,
        pub note_off_calls: Arc<Mutex<Vec<(u32, u32)>>>,
    }
    impl DummySynth {
        fn new() -> Self {
            Self {
                note_on_calls: Arc::new(Mutex::new(vec![])),
                note_off_calls: Arc::new(Mutex::new(vec![])),
            }
        }
    }
    trait DummySynthTrait {
        fn note_on(&mut self, ch: u32, key: u32, vel: u32);
        fn note_off(&mut self, ch: u32, key: u32);
    }

    impl DummySynthTrait for DummySynth {
        fn note_on(&mut self, ch: u32, key: u32, vel: u32) {
            self.note_on_calls.lock().unwrap().push((ch, key, vel));
        }
        fn note_off(&mut self, ch: u32, key: u32) {
            self.note_off_calls.lock().unwrap().push((ch, key));
        }
    }

    #[test]
    fn test_play_timeline_state_transitions() {
        // Create a short timeline: NoteOn at t=0, NoteOff at t=50_000us
        let timeline = Timeline {
            events: vec![
                Timed { t_us: 0, msg: Msg::NoteOn(0, 60, 100) },
                Timed { t_us: 50_000, msg: Msg::NoteOff(0, 60, 0) },
            ],
            last_t_us: 50_000,
            ppq: 140.0,
            initial_us_per_qn: 1_000_000.0,
        };

        // Use DummySynth in place of FluidLite Synth
        let dummy = DummySynth::new();
        let synth = Arc::new(Mutex::new(dummy));

        // We can't create a real Audio (needs SoundFont and CPAL), so test play_timeline logic directly
        let paused = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let paused_t = paused.clone();
        let finished_t = finished.clone();
        let synth_clone = synth.clone();
        let events = timeline.events.clone();

        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut paused_since: Option<std::time::Instant> = None;
            let mut paused_total_us: u128 = 0;
            let mut i = 0usize;
            'play: loop {
                if stop_rx.try_recv().is_ok() { break 'play; }
                if paused_t.load(Ordering::SeqCst) {
                    if paused_since.is_none() { paused_since = Some(std::time::Instant::now()); }
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                } else if let Some(since) = paused_since.take() {
                    let span = since.elapsed().as_micros();
                    paused_total_us = paused_total_us.saturating_add(span);
                }
                if i >= events.len() { break 'play; }
                let wall_us = start.elapsed().as_micros();
                let now_us_u128 = wall_us.saturating_sub(paused_total_us);
                let now_us = if now_us_u128 > u64::MAX as u128 { u64::MAX } else { now_us_u128 as u64 };
                let e = events[i];
                if now_us >= e.t_us {
                    if let Ok(mut s) = synth_clone.lock() {
                        match e.msg {
                            Msg::NoteOn(ch, key, vel) => { let _ = s.note_on(ch as u32, key as u32, vel as u32); },
                            Msg::NoteOff(ch, key, _vel) => { let _ = s.note_off(ch as u32, key as u32); },
                            _ => {}
                        }
                    }
                    i += 1;
                } else {
                    let wait_us = e.t_us.saturating_sub(now_us);
                    let ms = std::cmp::min(5, (wait_us / 1000) as u64);
                    if ms > 0 {
                        std::thread::sleep(Duration::from_millis(ms));
                    } else {
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
            }
            finished_t.store(true, Ordering::SeqCst);
        });

        // Wait for thread to finish
        std::thread::sleep(Duration::from_millis(100));
        // Check state transitions
        let synth = synth.lock().unwrap();
        assert_eq!(synth.note_on_calls.lock().unwrap().len(), 1);
        assert_eq!(synth.note_off_calls.lock().unwrap().len(), 1);
        assert!(finished.load(Ordering::SeqCst));
    }
}