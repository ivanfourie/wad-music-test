# Doom WAD Music (Rust)

little Rust playground that can open a DOOM/Heretic/Strife WAD file, list its contents, and play back music (MUS/MIDI) using a General MIDI SoundFont.

⚠️ This is an experiment — it works, but it’s not polished.

## Features

* Parse IWAD/PWAD headers and directory.
* Detect and convert MUS lumps to MIDI (with correct timing).
* Play music via `fluidlite` and `cpal`
  * Supports pause/resume (space bar).
  * Stop playback without quitting (Esc).
* List available songs by lump name (D_*, MUS_*).
* Command-line REPL interface (list, play by name).
* Case-insensitive song lookups (runNin → D_RUNNIN).
* Safe time math (no overflows), plays tricky tracks like D_VICTOR correctly.

## What works

* You can point it at a WAD (e.g. doom2.wad) and a SoundFont (.sf2) and:
    1. List available songs.
    2. Type the song name to play it.
    3. Pause/resume with space, stop with Esc.
    4. Play multiple songs in sequence without restarting.

## Usage
```bash
cargo run --release -- path/to/DOOM2.WAD path/to/soundfont.sf2
```

## Requirements

* Rust 1.75+ (tested).
* A WAD file (DOOM/DOOM2/Heretic/Strife). // Shareware works beautifully
* A General MIDI SoundFont (.sf2).
  * Free ones that sounds great: [Arachno](href=http://www.arachnosoft.com/main/soundfont.php), [GeneralUser GS](https://schristiancollins.com/generaluser.php)

## License
MIT