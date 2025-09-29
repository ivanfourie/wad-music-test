pub use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

#[derive(thiserror::Error, Debug)]
pub enum WadError {
    #[error("not a WAD file")]
    NotAWad,
    #[error("lump not found: {0}")]
    LumpNotFound(String),
}

/// WAD directory entry
#[derive(Debug, Clone)]
pub struct Lump {
    pub name: String,   // 8-char upper ASCII without trailing NULs
    pub filepos: u32,   // offset from start of file
    pub size: u32,      // bytes
}

/// Parsed WAD file with a case-insensitive name index.
/// Multiple lumps can share the same name, so the index maps to a list of indices.
#[derive(Debug)]
pub struct Wad {
    file: File,
    lumps: Vec<Lump>,
    index: HashMap<String, Vec<usize>>,
}

impl Wad {
    /// Open and parse an IWAD or PWAD.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut f = File::open(&path).with_context(|| format!("opening {:?}", path))?;

        // Header: ident[4], numlumps[4], infotableofs[4]
        let mut ident = [0u8; 4];
        f.read_exact(&mut ident)?;
        let numlumps = read_u32(&mut f)?;
        let infoofs = read_u32(&mut f)?;

        if &ident != b"IWAD" && &ident != b"PWAD" {
            return Err(WadError::NotAWad.into());
        }

        // Directory: numlumps entries of { filepos[4], size[4], name[8] }
        f.seek(SeekFrom::Start(infoofs as u64))?;
        let mut lumps = Vec::with_capacity(numlumps as usize);
        for _ in 0..numlumps {
            let filepos = read_u32(&mut f)?;
            let size = read_u32(&mut f)?;
            let mut name_bytes = [0u8; 8];
            f.read_exact(&mut name_bytes)?;
            let name = bytes_to_lump_name(&name_bytes);
            lumps.push(Lump { name, filepos, size });
        }

        // Build case-insensitive multi-map
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, l) in lumps.iter().enumerate() {
            index.entry(l.name.clone()).or_default().push(i);
        }

        Ok(Self { file: f, lumps, index })
    }

    /// Number of lumps.
    pub fn len(&self) -> usize {
        self.lumps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lumps.is_empty()
    }

    /// Borrow all directory entries.
    pub fn lumps(&self) -> &[Lump] {
        &self.lumps
    }

    /// List lump names, in file order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.lumps.iter().map(|l| l.name.as_str())
    }

    /// Find all indices for a lump name, case-insensitive.
    pub fn find_all(&self, name: &str) -> Option<&[usize]> {
        self.index.get(&name.to_uppercase()).map(|v| v.as_slice())
    }

    /// True if at least one lump with the given name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(&name.to_uppercase())
    }

    /// Get the first matching lump directory entry.
    pub fn get_first(&self, name: &str) -> Option<&Lump> {
        self.find_all(name).and_then(|ids| ids.first().map(|&i| &self.lumps[i]))
    }

    /// Return lumps whose name starts with the given ASCII prefix, case-insensitive.
    /// Useful for D_ music lumps or DS sound effects.
    pub fn by_prefix(&self, prefix: &str) -> Vec<&Lump> {
        let p = prefix.to_ascii_uppercase();
        self.lumps
            .iter()
            .filter(|l| l.name.starts_with(&p))
            .collect()
    }

    /// Read lump bytes by index.
    pub fn read_at(&mut self, idx: usize) -> Result<Vec<u8>> {
        let l = self.lumps.get(idx).context("lump index out of range")?;
        self.read_span(l.filepos as u64, l.size as usize)
    }

    /// Read the first lump that matches the given name.
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>> {
        let idx = self.find_all(name)
            .and_then(|v| v.first().copied())
            .ok_or_else(|| WadError::LumpNotFound(name.to_string()))?;
        self.read_at(idx)
    }

    // Zero-alloc iterator variant so we don't need a Vec
    pub fn iter_with_prefixes<'a>(&'a self, prefixes: &[&str]) 
        -> impl Iterator<Item = &'a Lump> + 'a 
    {
        let ups: Vec<String> = prefixes.iter().map(|p| p.to_ascii_uppercase()).collect();
        self.lumps.iter().filter(move |l| ups.iter().any(|p| l.name.starts_with(p)))
    }

    /// List lumps by a set of prefixes (e.g. ["D_", "MUS_"]).
    pub fn list_with_prefixes<'a>(&'a self, prefixes: &[&str]) -> Vec<&'a Lump> {
        let ups: Vec<String> = prefixes.iter().map(|p| p.to_ascii_uppercase()).collect();
        self.lumps
            .iter()
            .filter(|l| ups.iter().any(|p| l.name.starts_with(p)))
            .collect()
    }

    /// Raw file span read
    fn read_span(&mut self, start: u64, len: usize) -> Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; len];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Minimal helper for a one-off function call.
/// Uses the Wad type under the hood.
pub fn read_lump(path: &str, name: &str) -> Result<Vec<u8>> {
    let mut wad = Wad::open(path)?;
    wad.read(name)
}

/// Convert an 8-byte WAD name to upper ASCII without trailing NULs.
fn bytes_to_lump_name(b: &[u8; 8]) -> String {
    // Doom lump names are ASCII upper. Normalize to upper for case-insensitive lookups.
    let s = b.split(|&c| c == 0).next().unwrap_or(b);
    String::from_utf8_lossy(s).to_uppercase()
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};
    use tempfile::NamedTempFile;

    fn write_le_u32<W: Write>(w: &mut W, v: u32) { w.write_all(&v.to_le_bytes()).unwrap(); }

    // Build a tiny synthetic WAD with two lumps: HELLO and D_TEST
    fn make_fake_wad() -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();

        // Header: ident, numlumps=2, infotableofs= after data
        let ident = *b"IWAD";
        let numlumps = 2u32;

        // We will place lump data immediately after the 12-byte header.
        // Lump 0: "HELLO" with 5 bytes
        // Lump 1: "D_TEST" with 4 bytes
        let lump0_data = b"hello";
        let lump1_data = b"\x4D\x54\x68\x64"; // "MThd" start of MIDI for detection testing
        let header_size = 12;
        let lump0_ofs = header_size as u32;
        let lump1_ofs = lump0_ofs + lump0_data.len() as u32;
        let dir_ofs = lump1_ofs + lump1_data.len() as u32;

        // write header
        f.write_all(&ident).unwrap();
        write_le_u32(&mut f, numlumps);
        write_le_u32(&mut f, dir_ofs);

        // write data for two lumps
        f.write_all(lump0_data).unwrap();
        f.write_all(lump1_data).unwrap();

        // directory entries: filepos[4], size[4], name[8]
        // Lump 0: HELLO
        write_le_u32(&mut f, lump0_ofs);
        write_le_u32(&mut f, lump0_data.len() as u32);
        let mut name0 = [0u8; 8];
        name0[..5].copy_from_slice(b"HELLO");
        f.write_all(&name0).unwrap();

        // Lump 1: D_TEST
        write_le_u32(&mut f, lump1_ofs);
        write_le_u32(&mut f, lump1_data.len() as u32);
        let mut name1 = [0u8; 8];
        name1[..6].copy_from_slice(b"D_TEST");
        f.write_all(&name1).unwrap();

        f.flush().unwrap();
        f.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
        f
    }

    #[test]
    fn opens_and_lists() {
        const MUSIC_PREFIXES: &[&str] = &["D_", "MUS_"];
        let tmp = make_fake_wad();
        let wad = Wad::open(tmp.path()).unwrap();
        assert_eq!(wad.len(), 2);
        let names: Vec<_> = wad.names().collect();
        assert_eq!(names, vec!["HELLO", "D_TEST"]);
        assert!(wad.contains("hello")); // case-insensitive
        assert_eq!(wad.list_with_prefixes(MUSIC_PREFIXES).len(), 1);
    }

    #[test]
    fn reads_by_name_and_detects_format() {
        let tmp = make_fake_wad();
        let mut wad = Wad::open(tmp.path()).unwrap();
        let hello = wad.read("hello").unwrap();
        assert_eq!(hello, b"hello");

        let dtest = wad.read("D_TEST").unwrap();
        assert!(dtest.starts_with(b"MThd")); // looks like MIDI
    }

    #[test]
    fn bad_header_is_rejected() {
        use std::io::Write;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"XXXX\0\0\0\0\0\0\0\0").unwrap(); // bogus header
        f.flush().unwrap();
        let err = Wad::open(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not a WAD file"));
    }

    #[test]
    fn list_with_prefixes_is_case_insensitive() {
        const PREFS: &[&str] = &["d_", "mus_"]; // lower
        let tmp = make_fake_wad();
        let wad = Wad::open(tmp.path()).unwrap();
        assert_eq!(wad.list_with_prefixes(PREFS).len(), 1);
    }
}
