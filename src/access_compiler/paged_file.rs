//! A paged column as one file: the bytes of every page back to back, then two
//! footer sections a planner can read with one positioned read each.
//!
//! - The *zone map* holds each page's exact minimum and maximum and its
//!   non-null row count, as a writer would store statistics.
//! - The *derived bounds* hold the interval each page's bound facts give, read
//!   from the page's own fields when the file is written. Unlike the zone map,
//!   any reader can audit an entry against its page by reading the few bytes
//!   the bound comes from, without decoding a value.
//!
//! A trailer at the end of the file locates the sections and holds a checksum
//! of each, computed over the section's bytes and every page's offset, so a
//! section that was altered, or that belongs to another version of the file,
//! fails to read. All integers are little-endian.
//!
//! ```text
//! page 0 bytes | page 1 bytes | ... | zone map | derived bounds | trailer
//! section entry: flags u8 (bit 0: interval present), low i128, high i128,
//!                non_null u32                                    (37 bytes)
//! trailer:       each page's offset u64, the zone map's offset and length,
//!                the bounds' offset and length, the two sections' checksums,
//!                the page count, the trailer's own length, and the magic,
//!                all u64
//! ```

use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::paged::{self, PagedColumn};

const MAGIC: u64 = 0x5749_544e_5047_4632; // "WITNPGF2"
const ENTRY_BYTES: usize = 1 + 16 + 16 + 4;

/// Which per-page summary a planner reads from the footer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Summary {
    ZoneMap,
    DerivedBounds,
}

/// Where a paged file keeps its pages and footer sections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PagedFileLayout {
    /// Byte offset of each page.
    pub offsets: Vec<u64>,
    zone_map: (u64, u64),
    bounds: (u64, u64),
    zone_map_checksum: u64,
    bounds_checksum: u64,
}

/// One page's entry in a footer section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SummaryEntry {
    pub interval: Option<(i128, i128)>,
    pub non_null: usize,
}

impl PagedFileLayout {
    fn section(&self, summary: Summary) -> (u64, u64) {
        match summary {
            Summary::ZoneMap => self.zone_map,
            Summary::DerivedBounds => self.bounds,
        }
    }

    fn checksum(&self, summary: Summary) -> u64 {
        match summary {
            Summary::ZoneMap => self.zone_map_checksum,
            Summary::DerivedBounds => self.bounds_checksum,
        }
    }

    /// Bytes a planner reads to fetch one summary.
    pub fn section_bytes(&self, summary: Summary) -> u64 {
        self.section(summary).1
    }
}

/// Writes `column` to `path` and returns where its parts lie.
pub fn write(column: &PagedColumn, path: &Path) -> Result<PagedFileLayout, String> {
    let file = File::create(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut out = BufWriter::new(file);
    let mut position = 0_u64;
    let mut offsets = Vec::with_capacity(column.pages().len());
    for page in column.pages() {
        let bytes = page.column.page.bytes();
        offsets.push(position);
        out.write_all(bytes).map_err(|error| error.to_string())?;
        position += bytes.len() as u64;
    }
    let mut section = |entries: &mut dyn Iterator<Item = Result<SummaryEntry, String>>| {
        let start = position;
        let mut bytes = Vec::new();
        for entry in entries {
            let entry = entry?;
            let (flags, (low, high)) = match entry.interval {
                Some(interval) => (1_u8, interval),
                None => (0_u8, (0, 0)),
            };
            bytes.push(flags);
            bytes.extend_from_slice(&low.to_le_bytes());
            bytes.extend_from_slice(&high.to_le_bytes());
            let non_null = u32::try_from(entry.non_null).map_err(|_| "page too large")?;
            bytes.extend_from_slice(&non_null.to_le_bytes());
        }
        out.write_all(&bytes).map_err(|error| error.to_string())?;
        position += bytes.len() as u64;
        Ok::<_, String>((start, position - start, section_checksum(&bytes, &offsets)))
    };
    let zone_map = section(&mut column.pages().iter().map(|page| {
        Ok(SummaryEntry {
            interval: page.stats,
            non_null: page.non_null,
        })
    }))?;
    let bounds = section(&mut column.pages().iter().map(|page| {
        Ok(SummaryEntry {
            interval: paged::derived_bounds(page)?,
            non_null: page.non_null,
        })
    }))?;
    let mut trailer = Vec::new();
    for value in offsets.iter().copied().chain([
        zone_map.0, zone_map.1, bounds.0, bounds.1, zone_map.2, bounds.2,
    ]) {
        trailer.extend_from_slice(&value.to_le_bytes());
    }
    trailer.extend_from_slice(&(offsets.len() as u64).to_le_bytes());
    trailer.extend_from_slice(&((trailer.len() + 16) as u64).to_le_bytes());
    trailer.extend_from_slice(&MAGIC.to_le_bytes());
    out.write_all(&trailer).map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())?;
    Ok(PagedFileLayout {
        offsets,
        zone_map: (zone_map.0, zone_map.1),
        bounds: (bounds.0, bounds.1),
        zone_map_checksum: zone_map.2,
        bounds_checksum: bounds.2,
    })
}

/// Reads the trailer of a paged file: two positioned reads, the fixed tail
/// and then the trailer it sizes.
pub fn open(file: &File) -> Result<PagedFileLayout, String> {
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    let mut tail = [0_u8; 16];
    if length < 16 {
        return Err("paged file is too short".into());
    }
    file.read_exact_at(&mut tail, length - 16)
        .map_err(|error| error.to_string())?;
    let word = |bytes: &[u8], index: usize| {
        u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().unwrap())
    };
    if word(&tail, 1) != MAGIC {
        return Err("not a paged file".into());
    }
    let trailer_length = word(&tail, 0);
    if trailer_length < 24 || trailer_length > length || !(trailer_length - 8).is_multiple_of(8) {
        return Err("paged file trailer is malformed".into());
    }
    let mut trailer = vec![0_u8; trailer_length as usize];
    file.read_exact_at(&mut trailer, length - trailer_length)
        .map_err(|error| error.to_string())?;
    let words = trailer.len() / 8;
    let pages = word(&trailer, words - 3) as usize;
    if words != pages + 6 + 3 {
        return Err("paged file trailer does not match its page count".into());
    }
    let offsets = (0..pages).map(|index| word(&trailer, index)).collect();
    let section = |at: usize| (word(&trailer, pages + at), word(&trailer, pages + at + 1));
    Ok(PagedFileLayout {
        offsets,
        zone_map: section(0),
        bounds: section(2),
        zone_map_checksum: word(&trailer, pages + 4),
        bounds_checksum: word(&trailer, pages + 5),
    })
}

/// Reads one footer section with a single positioned read.
pub fn read_summary(
    file: &File,
    layout: &PagedFileLayout,
    summary: Summary,
) -> Result<Vec<SummaryEntry>, String> {
    let (offset, length) = layout.section(summary);
    if length != (layout.offsets.len() * ENTRY_BYTES) as u64 {
        return Err("footer section does not match the page count".into());
    }
    let mut bytes = vec![0_u8; length as usize];
    file.read_exact_at(&mut bytes, offset)
        .map_err(|error| error.to_string())?;
    if section_checksum(&bytes, &layout.offsets) != layout.checksum(summary) {
        return Err("footer section fails its checksum".into());
    }
    bytes
        .chunks_exact(ENTRY_BYTES)
        .map(|entry| {
            let low = i128::from_le_bytes(entry[1..17].try_into().unwrap());
            let high = i128::from_le_bytes(entry[17..33].try_into().unwrap());
            let non_null = u32::from_le_bytes(entry[33..37].try_into().unwrap()) as usize;
            let interval = match entry[0] {
                0 => None,
                1 if low <= high => Some((low, high)),
                _ => return Err("footer entry is malformed".to_string()),
            };
            Ok(SummaryEntry { interval, non_null })
        })
        .collect()
}

/// FNV-1a over a section's bytes and then every page offset: binds the
/// section to the pages it describes, so it detects a section that was
/// altered or written for another version of the file. It detects
/// corruption, not a writer that lies.
fn section_checksum(section: &[u8], offsets: &[u64]) -> u64 {
    section
        .iter()
        .copied()
        .chain(offsets.iter().flat_map(|offset| offset.to_le_bytes()))
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3)
        })
}
