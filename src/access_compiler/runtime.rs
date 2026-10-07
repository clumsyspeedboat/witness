use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

use super::{AccessSet, FieldId, FieldLocation, FrameCodec, FrameId, SerializedPage, Span};

/// Decoded bytes of one frame group. Chunks decode on first touch, in place.
struct FrameBuffer {
    bytes: Vec<u8>,
    chunk: usize,
    decoded: Vec<bool>,
    pending: usize,
}

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
    frames: BTreeMap<FrameId, FrameBuffer>,
    source: ReadSource<'a>,
    transfer_operations: usize,
    frames_decoded: usize,
    track_access: bool,
    primitive_values_read: usize,
    batch_buffer: Vec<i128>,
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
            batch_buffer: Vec::new(),
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
            batch_buffer: Vec::new(),
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
            batch_buffer: Vec::new(),
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
            batch_buffer: Vec::new(),
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

    /// Reads the packed bytes of `rows` once, exactly as `read_bit_values`
    /// over the same rows would, without decoding them. A batch kernel calls
    /// this before unpacking the span in chunks; the chunk reads then fall
    /// inside an already delivered range and add no transfers.
    pub fn touch_bits(
        &mut self,
        field: FieldId,
        rows: Span,
        width: u8,
        miniblock_rows: usize,
        miniblock_bytes: usize,
    ) -> Result<(), String> {
        if width == 0 || rows.start == rows.end {
            return Ok(());
        }
        if width > 64 || miniblock_rows == 0 || miniblock_bytes == 0 {
            return Err("invalid bit-unpack parameters".into());
        }
        let first_byte = packed_byte_offset(rows.start, width, miniblock_rows, miniblock_bytes)?;
        let last_row = rows.end - 1;
        let last_byte = packed_byte_offset(last_row, width, miniblock_rows, miniblock_bytes)?;
        let last_bit = (last_row % miniblock_rows) * width as usize % 8;
        let byte_end = last_byte + (last_bit + width as usize).div_ceil(8);
        self.read_range(field, Span::new(first_byte, byte_end)?)?;
        Ok(())
    }

    /// Lends the session's reusable batch buffer to a batch kernel, which
    /// hands it back with `restore_batch_buffer`. Reusing one buffer, rather
    /// than declaring a full batch per call, keeps a short span from paying
    /// to initialize rows it never decodes.
    pub fn take_batch_buffer(&mut self) -> Vec<i128> {
        std::mem::take(&mut self.batch_buffer)
    }

    pub fn restore_batch_buffer(&mut self, buffer: Vec<i128>) {
        self.batch_buffer = buffer;
    }

    /// Batch form of `read_bit_values`: replaces the contents of `out` with
    /// `rows`, as `u64` codes or widened values, reusing its allocation. It reads the same byte range and
    /// records the same access, so batch and per-value kernels are accounted
    /// identically.
    pub fn unpack_bits_into<T: From<u64> + Default + Clone>(
        &mut self,
        field: FieldId,
        rows: Span,
        width: u8,
        miniblock_rows: usize,
        miniblock_bytes: usize,
        out: &mut Vec<T>,
    ) -> Result<(), String> {
        out.clear();
        self.record_primitive_values(rows.len());
        if width == 0 {
            out.resize(rows.len(), T::default());
            return Ok(());
        }
        if width > 64 || miniblock_rows == 0 || miniblock_bytes == 0 {
            return Err("invalid bit-unpack parameters".into());
        }
        if rows.start == rows.end {
            return Ok(());
        }
        let first_byte = packed_byte_offset(rows.start, width, miniblock_rows, miniblock_bytes)?;
        let last_row = rows.end - 1;
        let last_byte = packed_byte_offset(last_row, width, miniblock_rows, miniblock_bytes)?;
        let last_bit = (last_row % miniblock_rows) * width as usize % 8;
        let byte_end = last_byte + (last_bit + width as usize).div_ceil(8);
        miniblock_rows
            .checked_mul(width as usize)
            .ok_or("packed bit offset overflow")?;
        let bytes = self.read_range(field, Span::new(first_byte, byte_end)?)?;
        unpack_into(
            bytes,
            rows,
            width,
            miniblock_rows,
            miniblock_bytes,
            first_byte,
            out,
        );
        Ok(())
    }

    /// Batch dictionary lookup: replaces every id in `values` with entry `id`
    /// of an i64 table field. A tracked or storage-backed session reads entry
    /// by entry, exactly as per-value `read_i64` would; an untracked in-memory
    /// session, which records nothing, indexes one borrowed view instead.
    pub fn gather_i64_into(
        &mut self,
        field: FieldId,
        entries: usize,
        values: &mut [i128],
    ) -> Result<(), String> {
        let mut max_id = 0_usize;
        for &id in values.iter() {
            let id = usize::try_from(id).map_err(|_| "dictionary id does not fit usize")?;
            if id >= entries {
                return Err("dictionary id is outside table".into());
            }
            max_id = max_id.max(id);
        }
        if values.is_empty() {
            return Ok(());
        }
        if self.track_access || !matches!(self.source, ReadSource::Memory) {
            for value in values.iter_mut() {
                *value = i128::from(self.read_i64(field, *value as usize)?);
            }
            return Ok(());
        }
        let end = (max_id + 1)
            .checked_mul(8)
            .ok_or("dictionary view offset overflow")?;
        let table = self.read_range(field, Span::new(0, end)?)?;
        for value in values.iter_mut() {
            let offset = *value as usize * 8;
            *value = i128::from(i64::from_le_bytes(
                table[offset..offset + 8].try_into().unwrap(),
            ));
        }
        Ok(())
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
                let decoded = Span::new(decoded_offset + span.start, decoded_offset + span.end)?;
                self.ensure_frame(frame, decoded)?;
                Ok(&self.frames[&frame].bytes[decoded.start..decoded.end])
            }
        }
    }

    /// Decodes the chunks of frame group `head` that `decoded` spans. An
    /// unchunked frame is one chunk, decoded whole on its first read.
    fn ensure_frame(&mut self, head: FrameId, decoded: Span) -> Result<(), String> {
        if let Some(buffer) = self.frames.get(&head) {
            if buffer.pending == 0 {
                return Ok(());
            }
            let (first, last) = chunk_range(buffer.chunk, buffer.decoded.len(), decoded);
            if buffer.decoded[first..=last].iter().all(|done| *done) {
                return Ok(());
            }
        }
        let group = self.page.layout().frame_group(head)?.to_vec();
        let chunk = group[0].decoded_length.max(1);
        let buffer = self.frames.entry(head).or_insert_with(|| FrameBuffer {
            bytes: vec![0; group.iter().map(|frame| frame.decoded_length).sum()],
            chunk,
            decoded: vec![false; group.len()],
            pending: group.len(),
        });
        let (first, last) = chunk_range(chunk, group.len(), decoded);
        let missing = (first..=last)
            .filter(|&index| !buffer.decoded[index])
            .collect::<Vec<_>>();
        for index in missing {
            let frame = &group[index];
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
            if decoded.len() != frame.decoded_length {
                return Err("Zstd frame decoded to the wrong length".into());
            }
            let buffer = self.frames.get_mut(&head).ok_or("frame buffer vanished")?;
            let start = index * buffer.chunk;
            buffer.bytes[start..start + decoded.len()].copy_from_slice(&decoded);
            buffer.decoded[index] = true;
            buffer.pending -= 1;
            self.frames_decoded += 1;
        }
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

/// Chunks `first..=last` of a group of `chunks` uniform chunks of `chunk`
/// decoded bytes that intersect `decoded`; an empty span touches the chunk
/// holding its position, clamped to the last chunk.
fn chunk_range(chunk: usize, chunks: usize, decoded: Span) -> (usize, usize) {
    let first = (decoded.start / chunk).min(chunks - 1);
    let last = if decoded.is_empty() {
        first
    } else {
        ((decoded.end - 1) / chunk).min(chunks - 1)
    };
    (first, last)
}

/// Unpacks `rows` from `bytes`, the delivered slice starting at stream byte
/// `first_byte`. Callers have validated that the slice covers every row.
fn unpack_into<T: From<u64>>(
    bytes: &[u8],
    rows: Span,
    width: u8,
    miniblock_rows: usize,
    miniblock_bytes: usize,
    first_byte: usize,
    out: &mut Vec<T>,
) {
    let width_bits = width as usize;
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    };
    let extract = |offset: usize, bit: usize| -> u64 {
        // A 57-bit value at any bit alignment fits one 8-byte window; wider
        // values take a 16-byte window. Near the end of the slice assemble
        // only the bytes the value needs.
        if width_bits <= 57 && offset + 8 <= bytes.len() {
            let word = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
            (word >> bit) & mask
        } else if offset + WIDE_LOAD <= bytes.len() {
            let word = u128::from_le_bytes(bytes[offset..offset + WIDE_LOAD].try_into().unwrap());
            ((word >> bit) as u64) & mask
        } else {
            let count = (bit + width_bits).div_ceil(8);
            let mut word = 0_u128;
            for (index, byte) in bytes[offset..offset + count].iter().copied().enumerate() {
                word |= u128::from(byte) << (index * 8);
            }
            ((word >> bit) as u64) & mask
        }
    };
    if miniblock_bytes * 8 == miniblock_rows * width_bits {
        // Miniblocks abut without padding, so row r starts at bit r * width
        // and the cursor advances without per-row division.
        let first_bit = rows.start * width_bits - first_byte * 8;
        // Rows whose 8-byte window lies inside the slice take a branch-free
        // load; only the last few rows of a span need the careful path.
        // Groups of eight rows span exactly `width` bytes, so with the width
        // known at compile time every load in a group sits at a constant
        // offset of one 64-byte window and needs no bounds check.
        let grouped = unpack_groups(bytes, first_bit, width_bits, rows.len(), out);
        let fast = if width_bits <= 57 && bytes.len() >= 8 {
            let limit = (bytes.len() - 8) * 8 + 7;
            if first_bit > limit {
                0
            } else {
                ((limit - first_bit) / width_bits + 1).min(rows.len())
            }
        } else {
            0
        };
        let fast = fast.max(grouped);
        out.extend((grouped..fast).map(|index| {
            let bit = first_bit + index * width_bits;
            let offset = bit >> 3;
            let word = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
            T::from((word >> (bit & 7)) & mask)
        }));
        out.extend((fast..rows.len()).map(|index| {
            let bit = first_bit + index * width_bits;
            T::from(extract(bit >> 3, bit & 7))
        }));
    } else {
        out.extend((rows.start..rows.end).map(|row| {
            let bit_offset = (row % miniblock_rows) * width_bits;
            let offset = (row / miniblock_rows) * miniblock_bytes + bit_offset / 8 - first_byte;
            T::from(extract(offset, bit_offset % 8))
        }));
    }
}

/// Widest code `unpack_group` handles: its shifts stay inside one `u64`.
const GROUPED_WIDTH: usize = 50;
const GROUP_WINDOW: usize = 64;

/// Unpacks whole groups of eight rows from bit `first_bit` while a 64-byte
/// window fits, returning how many rows it appended.
fn unpack_groups<T: From<u64>>(
    bytes: &[u8],
    first_bit: usize,
    width: usize,
    rows: usize,
    out: &mut Vec<T>,
) -> usize {
    let start = first_bit / 8;
    if width == 0 || width > GROUPED_WIDTH || bytes.len() < start + GROUP_WINDOW {
        return 0;
    }
    let groups = ((bytes.len() - start - GROUP_WINDOW) / width + 1).min(rows / 8);
    let shift = first_bit % 8;
    macro_rules! dispatch {
        ($($width:literal)*) => {
            match width {
                $($width => unpack_group::<$width, T>(bytes, start, shift, groups, out),)*
                _ => return 0,
            }
        };
    }
    dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25
        26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50);
    groups * 8
}

fn unpack_group<const WIDTH: usize, T: From<u64>>(
    bytes: &[u8],
    start: usize,
    shift: usize,
    groups: usize,
    out: &mut Vec<T>,
) {
    let mask = (1_u64 << WIDTH) - 1;
    out.reserve(groups * 8);
    for group in 0..groups {
        let offset = start + group * WIDTH;
        let window: &[u8; GROUP_WINDOW] = bytes[offset..offset + GROUP_WINDOW].try_into().unwrap();
        out.extend((0..8).map(|index| {
            let bit = index * WIDTH;
            let byte = bit / 8;
            let word = u64::from_le_bytes(window[byte..byte + 8].try_into().unwrap());
            T::from((word >> (shift + bit % 8)) & mask)
        }));
    }
}

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
