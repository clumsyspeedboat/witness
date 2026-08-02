use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

use super::{AccessSet, FieldId, FieldLocation, FrameCodec, FrameId, SerializedPage, Span};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClosureMode {
    Selective,
    FullPage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessMetrics {
    pub logical_bytes: usize,
    pub delivered_bytes: usize,
    pub transferred_bytes: usize,
    pub transfer_operations: usize,
    pub frames_decoded: usize,
}

enum ReadSource<'a> {
    Memory,
    Bytes { bytes: &'a [u8], base_offset: usize },
    File { file: &'a File, base_offset: u64 },
}

pub struct ReadSession<'a> {
    page: &'a SerializedPage,
    mode: ClosureMode,
    logical: AccessSet,
    delivered: Vec<Span>,
    transferred: Vec<Span>,
    storage_cache: Vec<u8>,
    frames: BTreeMap<FrameId, Vec<u8>>,
    source: ReadSource<'a>,
    transfer_operations: usize,
    frames_decoded: usize,
    track_access: bool,
    primitive_values_read: usize,
}

impl<'a> ReadSession<'a> {
    pub fn new(page: &'a SerializedPage, mode: ClosureMode) -> Self {
        Self {
            page,
            mode,
            logical: AccessSet::default(),
            delivered: Vec::new(),
            transferred: Vec::new(),
            storage_cache: Vec::new(),
            frames: BTreeMap::new(),
            source: ReadSource::Memory,
            transfer_operations: 0,
            frames_decoded: 0,
            track_access: true,
            primitive_values_read: 0,
        }
    }

    pub fn new_untracked(page: &'a SerializedPage, mode: ClosureMode) -> Self {
        Self {
            page,
            mode,
            logical: AccessSet::default(),
            delivered: Vec::new(),
            transferred: Vec::new(),
            storage_cache: Vec::new(),
            frames: BTreeMap::new(),
            source: ReadSource::Memory,
            transfer_operations: 0,
            frames_decoded: 0,
            track_access: false,
            primitive_values_read: 0,
        }
    }

    pub fn from_bytes(
        page: &'a SerializedPage,
        mode: ClosureMode,
        bytes: &'a [u8],
        base_offset: usize,
    ) -> Result<Self, String> {
        let end = base_offset
            .checked_add(page.bytes().len())
            .ok_or("mapped page offset overflow")?;
        if end > bytes.len() {
            return Err("mapped page lies outside backing bytes".into());
        }
        Ok(Self {
            page,
            mode,
            logical: AccessSet::default(),
            delivered: Vec::new(),
            transferred: Vec::new(),
            storage_cache: vec![0; page.bytes().len()],
            frames: BTreeMap::new(),
            source: ReadSource::Bytes { bytes, base_offset },
            transfer_operations: 0,
            frames_decoded: 0,
            track_access: true,
            primitive_values_read: 0,
        })
    }

    pub fn from_file(
        page: &'a SerializedPage,
        mode: ClosureMode,
        file: &'a File,
        base_offset: u64,
    ) -> Self {
        Self {
            page,
            mode,
            logical: AccessSet::default(),
            delivered: Vec::new(),
            transferred: Vec::new(),
            storage_cache: vec![0; page.bytes().len()],
            frames: BTreeMap::new(),
            source: ReadSource::File { file, base_offset },
            transfer_operations: 0,
            frames_decoded: 0,
            track_access: true,
            primitive_values_read: 0,
        }
    }

    pub fn metrics(&self) -> AccessMetrics {
        if !self.track_access {
            return AccessMetrics {
                logical_bytes: 0,
                delivered_bytes: 0,
                transferred_bytes: 0,
                transfer_operations: 0,
                frames_decoded: 0,
            };
        }
        AccessMetrics {
            logical_bytes: self.logical.bytes(),
            delivered_bytes: self.delivered.iter().copied().map(Span::len).sum(),
            transferred_bytes: self.transferred.iter().copied().map(Span::len).sum(),
            transfer_operations: self.transfer_operations,
            frames_decoded: self.frames_decoded,
        }
    }

    pub fn transferred_ranges(&self) -> &[Span] {
        &self.transferred
    }

    pub fn primitive_values_read(&self) -> usize {
        self.primitive_values_read
    }

    pub fn read_u8(&mut self, field: FieldId, offset: usize) -> Result<u8, String> {
        self.record_primitive_values(1);
        Ok(self.read_range(field, Span::new(offset, offset + 1)?)?[0])
    }

    pub fn read_u32(&mut self, field: FieldId, index: usize) -> Result<u32, String> {
        self.record_primitive_values(1);
        let offset = index.checked_mul(4).ok_or("u32 field offset overflow")?;
        Ok(u32::from_le_bytes(
            self.read_range(field, Span::new(offset, offset + 4)?)?
                .try_into()
                .unwrap(),
        ))
    }

    pub fn read_i64(&mut self, field: FieldId, index: usize) -> Result<i64, String> {
        self.record_primitive_values(1);
        let offset = index.checked_mul(8).ok_or("i64 field offset overflow")?;
        Ok(i64::from_le_bytes(
            self.read_range(field, Span::new(offset, offset + 8)?)?
                .try_into()
                .unwrap(),
        ))
    }

    pub fn read_i64_values(
        &mut self,
        field: FieldId,
        start: usize,
        end: usize,
    ) -> Result<Vec<i64>, String> {
        if start > end {
            return Err("invalid i64 field range".into());
        }
        self.record_primitive_values(end - start);
        let byte_start = start.checked_mul(8).ok_or("i64 field offset overflow")?;
        let byte_end = end.checked_mul(8).ok_or("i64 field offset overflow")?;
        self.read_range(field, Span::new(byte_start, byte_end)?)?
            .chunks_exact(8)
            .map(|bytes| Ok(i64::from_le_bytes(bytes.try_into().unwrap())))
            .collect()
    }

    pub fn read_bit_values(
        &mut self,
        field: FieldId,
        rows: Span,
        width: u8,
        miniblock_rows: usize,
        miniblock_bytes: usize,
    ) -> Result<Vec<u64>, String> {
        self.record_primitive_values(rows.len());
        if width == 0 {
            return Ok(vec![0; rows.len()]);
        }
        if width > 64 || miniblock_rows == 0 || miniblock_bytes == 0 {
            return Err("invalid bit-unpack parameters".into());
        }
        if rows.start == rows.end {
            return Ok(Vec::new());
        }
        let first_byte = packed_byte_offset(rows.start, width, miniblock_rows, miniblock_bytes)?;
        let last_row = rows.end - 1;
        let last_byte = packed_byte_offset(last_row, width, miniblock_rows, miniblock_bytes)?;
        let last_bit = (last_row % miniblock_rows) * width as usize % 8;
        let byte_end = last_byte + (last_bit + width as usize).div_ceil(8);
        // The overflow checks `packed_byte_offset` performs per row are hoisted
        // here instead. Offsets are monotone in `row`, so validating the widest
        // miniblock stride and the last row's offset (`last_byte`, above) bounds
        // every offset the loop computes; the arithmetic inside is then provably
        // in range and needs no per-row checking.
        miniblock_rows
            .checked_mul(width as usize)
            .ok_or("packed bit offset overflow")?;
        let bytes = self.read_range(field, Span::new(first_byte, byte_end)?)?;
        let width_bits = width as usize;
        let mask = if width == 64 {
            u128::from(u64::MAX)
        } else {
            (1_u128 << width) - 1
        };
        let mut values = Vec::with_capacity(rows.len());
        for row in rows.start..rows.end {
            let in_block = row % miniblock_rows;
            let bit_offset = in_block * width_bits;
            let offset = (row / miniblock_rows) * miniblock_bytes + bit_offset / 8 - first_byte;
            let bit = bit_offset % 8;
            // A fixed-width load wherever the delivered slice has room for one:
            // high bytes are masked off, so reading the full window is exact.
            // Near the end of the slice fall back to assembling only the bytes
            // this value needs. Selective plans deliver short spans, so the tail
            // path must stay cheap -- a clamped variable-length copy here costs
            // more than the byte loop it replaces.
            let word = if offset + WIDE_LOAD <= bytes.len() {
                let window: [u8; WIDE_LOAD] = bytes[offset..offset + WIDE_LOAD]
                    .try_into()
                    .map_err(|_| "bit-unpack window slice mismatch")?;
                u128::from_le_bytes(window)
            } else {
                let count = (bit + width_bits).div_ceil(8);
                let mut word = 0_u128;
                for (index, byte) in bytes[offset..offset + count].iter().copied().enumerate() {
                    word |= u128::from(byte) << (index * 8);
                }
                word
            };
            values.push(((word >> bit) & mask) as u64);
        }
        Ok(values)
    }

    pub fn read_bits(
        &mut self,
        field: FieldId,
        row: usize,
        width: u8,
        miniblock_rows: usize,
        miniblock_bytes: usize,
    ) -> Result<u64, String> {
        self.record_primitive_values(1);
        if width == 0 {
            return Ok(0);
        }
        if width > 64 || miniblock_rows == 0 || miniblock_bytes == 0 {
            return Err("invalid bit-unpack parameters".into());
        }
        let block = row / miniblock_rows;
        let in_block = row % miniblock_rows;
        let bit_offset = in_block
            .checked_mul(width as usize)
            .ok_or("packed bit offset overflow")?;
        let byte_offset = block
            .checked_mul(miniblock_bytes)
            .and_then(|offset| offset.checked_add(bit_offset / 8))
            .ok_or("packed byte offset overflow")?;
        let bit_in_byte = bit_offset % 8;
        let byte_count = (bit_in_byte + width as usize).div_ceil(8);
        let bytes = self.read_range(field, Span::new(byte_offset, byte_offset + byte_count)?)?;
        let mut word = 0_u128;
        for (index, byte) in bytes.iter().copied().enumerate() {
            word |= u128::from(byte) << (index * 8);
        }
        let mask = if width == 64 {
            u128::from(u64::MAX)
        } else {
            (1_u128 << width) - 1
        };
        Ok(((word >> bit_in_byte) & mask) as u64)
    }

    pub fn read_range(&mut self, field_id: FieldId, span: Span) -> Result<&[u8], String> {
        // Copy out only the scalars this read needs. Cloning the whole
        // `FieldLayout` would heap-allocate its `name` on every call, and every
        // scalar read reaches this path -- `read_i64` alone accounts for
        // millions of calls across the study -- so that allocation, not the
        // read itself, dominated the aggregation leg.
        let (length, read_granularity, location) = {
            let field = self
                .page
                .layout()
                .fields
                .get(field_id.0)
                .ok_or_else(|| format!("read references absent field {}", field_id.0))?;
            (field.length, field.read_granularity, field.location.clone())
        };
        if span.end > length {
            return Err(format!("read exceeds field {}", field_id.0));
        }
        if self.track_access {
            self.logical.insert(field_id, span);
        }
        match location {
            FieldLocation::Direct { offset } => {
                let delivered = match self.mode {
                    ClosureMode::Selective => Span::new(
                        offset + span.start / read_granularity * read_granularity,
                        offset
                            + span
                                .end
                                .div_ceil(read_granularity)
                                .saturating_mul(read_granularity)
                                .min(length),
                    )?,
                    ClosureMode::FullPage => Span::new(0, self.page.bytes().len())?,
                };
                if self.track_access || !matches!(&self.source, ReadSource::Memory) {
                    self.deliver(delivered)?;
                }
                let range = offset + span.start..offset + span.end;
                match &self.source {
                    ReadSource::Memory => Ok(&self.page.bytes()[range]),
                    _ => Ok(&self.storage_cache[range]),
                }
            }
            FieldLocation::Framed {
                frame,
                decoded_offset,
            } => {
                self.ensure_frame(frame)?;
                let decoded = &self.frames[&frame];
                Ok(&decoded[decoded_offset + span.start..decoded_offset + span.end])
            }
        }
    }

    fn ensure_frame(&mut self, frame_id: FrameId) -> Result<(), String> {
        if self.frames.contains_key(&frame_id) {
            return Ok(());
        }
        let frame = self
            .page
            .layout()
            .frames
            .get(frame_id.0)
            .ok_or("framed read references absent frame")?
            .clone();
        let delivered = match self.mode {
            ClosureMode::Selective => {
                Span::new(frame.offset, frame.offset + frame.compressed_length)?
            }
            ClosureMode::FullPage => Span::new(0, self.page.bytes().len())?,
        };
        if self.track_access || !matches!(&self.source, ReadSource::Memory) {
            self.deliver(delivered)?;
        }
        let compressed_range = frame.offset..frame.offset + frame.compressed_length;
        let compressed = match &self.source {
            ReadSource::Memory => &self.page.bytes()[compressed_range],
            _ => &self.storage_cache[compressed_range],
        };
        let decoded = match frame.codec {
            FrameCodec::Zstd => zstd::bulk::decompress(compressed, frame.decoded_length)
                .map_err(|error| format!("Zstd frame decompression failed: {error}"))?,
        };
        self.frames.insert(frame_id, decoded);
        self.frames_decoded += 1;
        Ok(())
    }

    fn deliver(&mut self, span: Span) -> Result<(), String> {
        if contains(&self.delivered, span) {
            return Ok(());
        }
        let new = uncovered(&self.transferred, span);
        for range in &new {
            match &self.source {
                ReadSource::Memory => {}
                ReadSource::Bytes { bytes, base_offset } => {
                    let start = base_offset + range.start;
                    let end = base_offset + range.end;
                    self.storage_cache[range.start..range.end].copy_from_slice(&bytes[start..end]);
                }
                ReadSource::File { file, base_offset } => {
                    let offset = (*base_offset)
                        .checked_add(range.start as u64)
                        .ok_or("file-backed read offset overflow")?;
                    file.read_exact_at(&mut self.storage_cache[range.start..range.end], offset)
                        .map_err(|error| format!("file-backed page read failed: {error}"))?;
                }
            }
            self.transfer_operations += 1;
        }
        self.delivered.push(span);
        merge(&mut self.delivered);
        self.transferred.extend(new);
        merge(&mut self.transferred);
        Ok(())
    }

    fn record_primitive_values(&mut self, count: usize) {
        if self.track_access {
            self.primitive_values_read = self.primitive_values_read.saturating_add(count);
        }
    }
}

/// Bytes loaded per value on the fast unpack path: enough for any 64-bit
/// field at any bit alignment, and a size the compiler can turn into a fixed
/// unaligned load.
const WIDE_LOAD: usize = 16;

fn packed_byte_offset(
    row: usize,
    width: u8,
    miniblock_rows: usize,
    miniblock_bytes: usize,
) -> Result<usize, String> {
    let block = row / miniblock_rows;
    let bit_offset = (row % miniblock_rows)
        .checked_mul(width as usize)
        .ok_or("packed bit offset overflow")?;
    block
        .checked_mul(miniblock_bytes)
        .and_then(|offset| offset.checked_add(bit_offset / 8))
        .ok_or_else(|| "packed byte offset overflow".into())
}

fn contains(existing: &[Span], requested: Span) -> bool {
    existing
        .iter()
        .any(|span| span.start <= requested.start && requested.end <= span.end)
}

fn uncovered(existing: &[Span], requested: Span) -> Vec<Span> {
    let mut pending = vec![requested];
    for covered in existing {
        let mut next = Vec::new();
        for span in pending {
            if covered.end <= span.start || covered.start >= span.end {
                next.push(span);
            } else {
                if span.start < covered.start {
                    next.push(Span {
                        start: span.start,
                        end: covered.start,
                    });
                }
                if covered.end < span.end {
                    next.push(Span {
                        start: covered.end,
                        end: span.end,
                    });
                }
            }
        }
        pending = next;
    }
    pending
}

fn merge(spans: &mut Vec<Span>) {
    spans.sort_unstable();
    let mut write = 0;
    for read in 0..spans.len() {
        if write > 0 && spans[read].start <= spans[write - 1].end {
            spans[write - 1].end = spans[write - 1].end.max(spans[read].end);
        } else {
            spans[write] = spans[read];
            write += 1;
        }
    }
    spans.truncate(write);
}
