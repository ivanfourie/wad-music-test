/// Decode a DOOM sfx lump into mono i16 samples and return (sample_rate_hz, pcm)
///
/// DOOM sound lumps have a DMX header. DoomGeneric processes them as:
///   - read samplerate (u16 LE) and declared length (u32 LE) at bytes 0..8
///   - then "skip 16 from start and 16 from end" and also skip 8 more before data
///     which yields a final data start at offset 24, and usable length = declared_len - 32
/// This decoder mirrors that trimming and converts 8 bit unsigned PCM to i16 with a little headroom.
fn decode_doom_sound(name: &str, bytes: &[u8]) -> Result<(u32, Vec<i16>)> {
    // Quick header sanity
    if bytes.len() < 8 || bytes[0] != 0x03 || bytes[1] != 0x00 {
        anyhow::bail!("unsupported or corrupt DOOM sound lump: {name}");
    }

    let samplerate = u16::from_le_bytes([bytes[2], bytes[3]]) as u32;
    let declared_len = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

    // DMX drops very short sounds
    if declared_len <= 48 {
        anyhow::bail!("unsupported or corrupt DOOM sound lump: {name}");
    }

    // Trim like DoomGeneric
    let start = 24usize;      // 16 + 8
    let end_guard = 16usize;
    if bytes.len() <= start + end_guard {
        anyhow::bail!("unsupported or corrupt DOOM sound lump: {name}");
    }

    // Usable payload equals declared_len minus the 32 bytes DMX ignores
    let mut usable = declared_len.saturating_sub(8);
    // Clamp by what actually exists after trimming the tail
    let max_len = bytes.len() - start - end_guard;
    if usable > max_len { usable = max_len; }

    let src = &bytes[start .. start + usable];

    // Convert 8 bit unsigned [0..255] to i16. 128 maps to 0. Add a little headroom to reduce clipping when mixing.
    let pcm: Vec<i16> = src.iter().map(|&u| {
        let centered = u as i32 - 128;
        let s = centered * 256;
        (s * 4 / 5) as i16
    }).collect();

    // Guard broken rates. Most sfx are 11025 Hz
    let sr = if (4_000..=48_000).contains(&samplerate) { samplerate } else { 11_025 };

    Ok((sr, pcm))
}

/// Apply short linear fades at the beginning and end of a PCM buffer.
///
/// Doom sound effects often start/stop abruptly. When converted directly,
/// this can produce a sharp "click" because the waveform suddenly jumps
/// from zero to a nonzero value (or vice versa). A very short fade-in and
/// fade-out smooths that discontinuity and makes playback cleaner.
///
/// - Fade-in length: ~3 ms, at least 8 samples
/// - Fade-out length: ~5 ms, at least 8 samples
/// - Both fades are clamped to the buffer length (so they don't exceed
///   the sample count for very short sounds).
///
/// The fade factor `g` linearly ramps the amplitude from 0 → 1 at the start,
/// and from 1 → 0 at the end.
///
/// # Arguments
/// * `pcm` - mutable slice of i16 samples (mono or one channel of stereo)
/// * `sample_rate` - sample rate in Hz, used to compute fade lengths
fn apply_fades(pcm: &mut [i16], sample_rate: u32) {
    if pcm.is_empty() { return; }
    
    // Convert milliseconds to sample counts
    let n_in  = (((sample_rate as u64) * 3) / 1000) as usize; // 3 ms
    let n_out = (((sample_rate as u64) * 5) / 1000) as usize; // 5 ms
    
    // Clamp to at least 8 samples, but not longer than the clip itself
    let n_in  = n_in.max(8).min(pcm.len());
    let n_out = n_out.max(8).min(pcm.len());

    // Apply linear fade-in
    for i in 0..n_in {
        let g = i as f32 / n_in as f32;
        pcm[i] = (pcm[i] as f32 * g) as i16;
    }

    // Apply linear fade-out
    for i in 0..n_out {
        let g = 1.0 - (i as f32 / n_out as f32);
        let idx = pcm.len() - 1 - i;
        pcm[idx] = (pcm[idx] as f32 * g) as i16;
    }
}

/// Resample every sound to a single device format (mono, 44.1 kHz).
/// Rodio mixes all appended sources in the sink, so keeping one format avoids per-source resamplers.
fn to_mono_44k1(sr: u32, pcm: Vec<i16>) -> UniformSourceIterator<SamplesBuffer<i16>, i16> {
    let mono = SamplesBuffer::new(1, sr, pcm);
    UniformSourceIterator::new(mono, 1, 44_100)
}