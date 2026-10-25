//! Self-contained resident column files. Codec descriptions and page bytes
//! travel together; opening reconstructs facts without writer objects or
//! oracle values. This is distinct from the legacy cold-read container.
//!
//! Version 1 stores a header, length-delimited page records, and an FNV-1a
//! checksum over every preceding byte. Each record holds a recipe, decoder
//! IR, optional exact statistics, and the existing serialized page. Facts,
//! page ends, and order-region certificates are reconstructed, not serialized.
//! Checksums detect accidental corruption; writer-attested order/null flags
//! and code validity retain the prototype's existing trust assumptions.

use std::fs;
use std::path::Path;

use super::paged::{PageParts, PagedColumn};
use super::{
    DecoderIr, DecoderNode, DeltaCoding, EncodedColumn, FieldId, NodeId, Recipe, SerializedPage,
};

const MAGIC: &[u8; 8] = b"WITNCOL1";
const MAX_NODES: usize = 64;
const MAX_RECIPE_DEPTH: usize = 128;
const MAX_DECODED_PAGE_BYTES: usize = 256 * 1024 * 1024;

pub struct NativeColumn {
    pub column: PagedColumn,
    pub file_bytes: usize,
    pub descriptor_bytes: usize,
    pub page_bytes: usize,
}

/// Encodes a complete column, including codec descriptions and statistics.
pub fn to_bytes(column: &PagedColumn) -> Result<Vec<u8>, String> {
    let mut out = MAGIC.to_vec();
    let page_rows = column.page_rows();
    if page_rows > u32::MAX as usize {
        return Err("native pages exceed the u32 row domain".into());
    }
    word(&mut out, page_rows);
    word(&mut out, column.pages().len());
    for page in column.pages() {
        let mut recipe = Vec::new();
        write_recipe(&page.column.recipe, &mut recipe, 0)?;
        validate_fields(&page.column.decoder, &page.column.page)?;
        let decoder = write_decoder(&page.column.decoder)?;
        let mut record = Vec::new();
        word(&mut record, recipe.len());
        word(&mut record, decoder.len());
        record.push(u8::from(page.stats.is_some()));
        let (low, high) = page.stats.unwrap_or((0, 0));
        record.extend_from_slice(&low.to_le_bytes());
        record.extend_from_slice(&high.to_le_bytes());
        word(&mut record, page.non_null);
        record.extend_from_slice(&recipe);
        record.extend_from_slice(&decoder);
        record.extend_from_slice(page.column.page.bytes());
        word(&mut out, record.len());
        out.extend_from_slice(&record);
    }
    let checksum = checksum(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    Ok(out)
}

pub fn write(column: &PagedColumn, path: impl AsRef<Path>) -> Result<usize, String> {
    let bytes = to_bytes(column)?;
    fs::write(path.as_ref(), &bytes)
        .map_err(|error| format!("{}: {error}", path.as_ref().display()))?;
    Ok(bytes.len())
}

/// Opens resident bytes. File I/O, checksum validation, descriptor parsing,
/// dictionary-order verification, and fact derivation are initialization
/// costs; no full logical value vector is materialized.
pub fn open(path: impl AsRef<Path>) -> Result<NativeColumn, String> {
    let bytes =
        fs::read(path.as_ref()).map_err(|error| format!("{}: {error}", path.as_ref().display()))?;
    from_bytes(&bytes)
}

pub fn from_bytes(bytes: &[u8]) -> Result<NativeColumn, String> {
    if bytes.len() < 32 || bytes.get(..8) != Some(MAGIC) {
        return Err("invalid native column header".into());
    }
    let end = bytes.len() - 8;
    let stored = u64::from_le_bytes(bytes[end..].try_into().unwrap());
    if stored != checksum(&bytes[..end]) {
        return Err("native column checksum mismatch".into());
    }
    let mut input = Input::new(&bytes[8..end]);
    let page_rows = input.word()?;
    let count = input.word()?;
    if page_rows == 0 || page_rows > u32::MAX as usize || count > input.remaining() / 65 {
        return Err("invalid native page dimensions".into());
    }
    let mut pages = Vec::with_capacity(count);
    let mut page_bytes = 0_usize;
    for _ in 0..count {
        let length = input.word()?;
        let mut record = Input::new(input.take(length)?);
        let recipe_len = record.word()?;
        let decoder_len = record.word()?;
        let present = record.byte()?;
        let low = i128::from_le_bytes(record.take(16)?.try_into().unwrap());
        let high = i128::from_le_bytes(record.take(16)?.try_into().unwrap());
        let non_null = record.word()?;
        let stats = match present {
            0 if low == 0 && high == 0 => None,
            1 if low <= high => Some((low, high)),
            _ => return Err("invalid native statistics".into()),
        };
        let mut recipe_input = Input::new(record.take(recipe_len)?);
        let recipe = read_recipe(&mut recipe_input, 0)?;
        recipe_input.finish()?;
        let decoder = read_decoder(record.take(decoder_len)?)?;
        if !recipe_matches(&recipe, &decoder, decoder.root())? {
            return Err("native recipe disagrees with decoder".into());
        }
        let page = SerializedPage::parse(record.take(record.remaining())?.to_vec())?;
        validate_fields(&decoder, &page)?;
        let declared_nonnull = match decoder.node(decoder.root())? {
            DecoderNode::Nullable { nonnull_len, .. } => *nonnull_len,
            _ => decoder.len()?,
        };
        if non_null != declared_nonnull {
            return Err("native non-null count disagrees with decoder".into());
        }
        page_bytes = page_bytes
            .checked_add(page.bytes().len())
            .ok_or("native byte count overflow")?;
        let column = EncodedColumn {
            recipe,
            decoder,
            page,
            truth: Vec::new(),
        };
        pages.push(PageParts {
            column,
            stats,
            non_null,
        });
    }
    input.finish()?;
    let column = PagedColumn::from_encoded_pages(page_rows, pages)?;
    Ok(NativeColumn {
        column,
        file_bytes: bytes.len(),
        descriptor_bytes: bytes.len() - page_bytes,
        page_bytes,
    })
}

fn word(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u64).to_le_bytes());
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, byte| {
        (h ^ u64::from(*byte)).wrapping_mul(0x1000_0000_01b3)
    })
}

struct Input<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(count)
            .filter(|&end| end <= self.bytes.len())
            .ok_or("truncated native descriptor")?;
        let result = &self.bytes[self.at..end];
        self.at = end;
        Ok(result)
    }
    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn word(&mut self) -> Result<usize, String> {
        usize::try_from(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
            .map_err(|_| "native integer exceeds address space".into())
    }
    fn finish(&self) -> Result<(), String> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err("native descriptor has trailing bytes".into())
        }
    }
}

fn write_recipe(recipe: &Recipe, out: &mut Vec<u8>, depth: usize) -> Result<(), String> {
    if depth >= MAX_RECIPE_DEPTH {
        return Err("native recipe is too deep".into());
    }
    let (tag, parameter, child) = match recipe {
        Recipe::BitPack => (0, None, None),
        Recipe::For(x) => (1, None, Some(x)),
        Recipe::Dictionary(x) => (2, None, Some(x)),
        Recipe::Rle {
            index_interval,
            values,
        } => (3, Some(*index_interval), Some(values)),
        Recipe::Delta {
            restart_interval,
            deltas,
        } => (4, Some(*restart_interval), Some(deltas)),
        Recipe::UnsignedDelta {
            restart_interval,
            deltas,
        } => (5, Some(*restart_interval), Some(deltas)),
        Recipe::Patch {
            index_interval,
            values,
        } => (6, Some(*index_interval), Some(values)),
        Recipe::Nullable {
            rank_interval,
            values,
        } => (7, Some(*rank_interval), Some(values)),
        Recipe::Frame(x) => (8, None, Some(x)),
        Recipe::ChunkedFrame {
            chunk_bytes,
            values,
        } => (9, Some(*chunk_bytes), Some(values)),
    };
    out.push(tag);
    if let Some(parameter) = parameter {
        word(out, parameter);
    }
    if let Some(child) = child {
        write_recipe(child, out, depth + 1)?;
    }
    Ok(())
}

fn read_recipe(input: &mut Input<'_>, depth: usize) -> Result<Recipe, String> {
    if depth >= MAX_RECIPE_DEPTH {
        return Err("native recipe is too deep".into());
    }
    let tag = input.byte()?;
    if tag == 0 {
        return Ok(Recipe::BitPack);
    }
    let parameter = if [3, 4, 5, 6, 7, 9].contains(&tag) {
        let parameter = input.word()?;
        if parameter == 0 {
            return Err("native recipe has a zero interval".into());
        }
        parameter
    } else {
        0
    };
    if !(1..=9).contains(&tag) {
        return Err("unknown native recipe".into());
    }
    let child = Box::new(read_recipe(input, depth + 1)?);
    Ok(match tag {
        1 => Recipe::For(child),
        2 => Recipe::Dictionary(child),
        3 => Recipe::Rle {
            index_interval: parameter,
            values: child,
        },
        4 => Recipe::Delta {
            restart_interval: parameter,
            deltas: child,
        },
        5 => Recipe::UnsignedDelta {
            restart_interval: parameter,
            deltas: child,
        },
        6 => Recipe::Patch {
            index_interval: parameter,
            values: child,
        },
        7 => Recipe::Nullable {
            rank_interval: parameter,
            values: child,
        },
        8 => Recipe::Frame(child),
        9 => Recipe::ChunkedFrame {
            chunk_bytes: parameter,
            values: child,
        },
        _ => unreachable!(),
    })
}

fn write_decoder(decoder: &DecoderIr) -> Result<Vec<u8>, String> {
    if decoder.nodes().len() > MAX_NODES {
        return Err("native decoder has too many nodes".into());
    }
    let mut out = Vec::new();
    word(&mut out, decoder.nodes().len());
    word(&mut out, decoder.root().0);
    for node in decoder.nodes() {
        let (tag, words) = match *node {
            DecoderNode::BitUnpack {
                stream,
                width,
                len,
                miniblock_rows,
                miniblock_bytes,
            } => (
                0,
                vec![
                    stream.0,
                    width as usize,
                    len,
                    miniblock_rows,
                    miniblock_bytes,
                ],
            ),
            DecoderNode::For { base, values } => (1, vec![base.0, values.0]),
            DecoderNode::Dictionary {
                ids,
                dictionary,
                entries,
                sorted_unique,
            } => (
                2,
                vec![ids.0, dictionary.0, entries, usize::from(sorted_unique)],
            ),
            DecoderNode::Rle {
                values,
                run_lengths,
                run_index,
                len,
                runs,
                index_interval,
            } => (
                3,
                vec![
                    values.0,
                    run_lengths.0,
                    run_index.0,
                    len,
                    runs,
                    index_interval,
                ],
            ),
            DecoderNode::Delta {
                deltas,
                restarts,
                restart_interval,
                len,
                coding,
            } => (
                4,
                vec![
                    deltas.0,
                    restarts.0,
                    restart_interval,
                    len,
                    usize::from(coding == DeltaCoding::Unsigned),
                ],
            ),
            DecoderNode::Patch {
                values,
                positions,
                position_index,
                exceptions,
                count,
                index_interval,
            } => (
                5,
                vec![
                    values.0,
                    positions.0,
                    position_index.0,
                    exceptions.0,
                    count,
                    index_interval,
                ],
            ),
            DecoderNode::Nullable {
                validity,
                rank_index,
                values,
                logical_len,
                nonnull_len,
                rank_interval,
            } => (
                6,
                vec![
                    validity.0,
                    rank_index.0,
                    values.0,
                    logical_len,
                    nonnull_len,
                    rank_interval,
                ],
            ),
        };
        out.push(tag);
        for value in words {
            word(&mut out, value);
        }
    }
    Ok(out)
}

fn read_decoder(bytes: &[u8]) -> Result<DecoderIr, String> {
    let mut input = Input::new(bytes);
    let count = input.word()?;
    let root = input.word()?;
    if count == 0 || count > MAX_NODES || count > input.remaining() / 17 || root != count - 1 {
        return Err("invalid native decoder node count/root".into());
    }
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        nodes.push(match input.byte()? {
            0 => DecoderNode::BitUnpack {
                stream: FieldId(input.word()?),
                width: u8::try_from(input.word()?).map_err(|_| "invalid native width")?,
                len: input.word()?,
                miniblock_rows: input.word()?,
                miniblock_bytes: input.word()?,
            },
            1 => DecoderNode::For {
                base: FieldId(input.word()?),
                values: NodeId(input.word()?),
            },
            2 => {
                let ids = NodeId(input.word()?);
                let dictionary = FieldId(input.word()?);
                let entries = input.word()?;
                let sorted_unique = match input.word()? {
                    0 => false,
                    1 => true,
                    _ => return Err("invalid dictionary flag".into()),
                };
                DecoderNode::Dictionary {
                    ids,
                    dictionary,
                    entries,
                    sorted_unique,
                }
            }
            3 => DecoderNode::Rle {
                values: NodeId(input.word()?),
                run_lengths: FieldId(input.word()?),
                run_index: FieldId(input.word()?),
                len: input.word()?,
                runs: input.word()?,
                index_interval: input.word()?,
            },
            4 => {
                let deltas = NodeId(input.word()?);
                let restarts = FieldId(input.word()?);
                let restart_interval = input.word()?;
                let len = input.word()?;
                let coding = match input.word()? {
                    0 => DeltaCoding::ZigZag,
                    1 => DeltaCoding::Unsigned,
                    _ => return Err("invalid delta coding".into()),
                };
                DecoderNode::Delta {
                    deltas,
                    restarts,
                    restart_interval,
                    len,
                    coding,
                }
            }
            5 => DecoderNode::Patch {
                values: NodeId(input.word()?),
                positions: FieldId(input.word()?),
                position_index: FieldId(input.word()?),
                exceptions: FieldId(input.word()?),
                count: input.word()?,
                index_interval: input.word()?,
            },
            6 => DecoderNode::Nullable {
                validity: FieldId(input.word()?),
                rank_index: FieldId(input.word()?),
                values: NodeId(input.word()?),
                logical_len: input.word()?,
                nonnull_len: input.word()?,
                rank_interval: input.word()?,
            },
            _ => return Err("unknown native decoder node".into()),
        });
    }
    input.finish()?;
    let decoder = DecoderIr::new(nodes, NodeId(root))?;
    let mut current = Some(decoder.root());
    let mut reachable = 0;
    while let Some(node) = current {
        reachable += 1;
        current = decoder.node(node)?.children().next();
    }
    if reachable != count {
        return Err("native decoder has unreachable nodes".into());
    }
    Ok(decoder)
}

fn recipe_matches(recipe: &Recipe, decoder: &DecoderIr, node: NodeId) -> Result<bool, String> {
    let (child, at) = match (recipe, decoder.node(node)?) {
        (Recipe::Frame(x) | Recipe::ChunkedFrame { values: x, .. }, _) => (x, node),
        (Recipe::BitPack, DecoderNode::BitUnpack { .. }) => return Ok(true),
        (Recipe::For(x), DecoderNode::For { values, .. }) => (x, *values),
        (Recipe::Dictionary(x), DecoderNode::Dictionary { ids, .. }) => (x, *ids),
        (
            Recipe::Rle {
                index_interval: a,
                values: x,
            },
            DecoderNode::Rle {
                index_interval: b,
                values,
                ..
            },
        ) if a == b => (x, *values),
        (
            Recipe::Delta {
                restart_interval: a,
                deltas: x,
            },
            DecoderNode::Delta {
                restart_interval: b,
                deltas,
                coding: DeltaCoding::ZigZag,
                ..
            },
        ) if a == b => (x, *deltas),
        (
            Recipe::UnsignedDelta {
                restart_interval: a,
                deltas: x,
            },
            DecoderNode::Delta {
                restart_interval: b,
                deltas,
                coding: DeltaCoding::Unsigned,
                ..
            },
        ) if a == b => (x, *deltas),
        (
            Recipe::Patch {
                index_interval: a,
                values: x,
            },
            DecoderNode::Patch {
                index_interval: b,
                values,
                ..
            },
        ) if a == b => (x, *values),
        (
            Recipe::Nullable {
                rank_interval: a,
                values: x,
            },
            DecoderNode::Nullable {
                rank_interval: b,
                values,
                ..
            },
        ) if a == b => (x, *values),
        _ => return Ok(false),
    };
    recipe_matches(child, decoder, at)
}

fn validate_fields(decoder: &DecoderIr, page: &SerializedPage) -> Result<(), String> {
    let require = |field: FieldId, bytes: Option<usize>| -> Result<(), String> {
        let bytes = bytes.ok_or("native field size overflow")?;
        if field == page.layout().metadata
            || page
                .layout()
                .fields
                .get(field.0)
                .is_none_or(|field| field.length < bytes)
        {
            return Err("native decoder exceeds its field".into());
        }
        Ok(())
    };
    let mut decoded = 0_usize;
    for frame in &page.layout().frames {
        decoded = decoded
            .checked_add(frame.decoded_length)
            .ok_or("native decoded size overflow")?;
    }
    if decoded > MAX_DECODED_PAGE_BYTES {
        return Err("native decoded page exceeds 256 MiB".into());
    }
    for (index, node) in decoder.nodes().iter().enumerate() {
        match *node {
            DecoderNode::BitUnpack {
                stream,
                width,
                len,
                miniblock_rows,
                miniblock_bytes,
            } => {
                let bits = miniblock_rows
                    .checked_mul(width as usize)
                    .ok_or("native block size overflow")?;
                if miniblock_bytes < bits.div_ceil(8) {
                    return Err("native packed miniblock is too short".into());
                }
                require(
                    stream,
                    if width == 0 {
                        Some(0)
                    } else {
                        len.div_ceil(miniblock_rows).checked_mul(miniblock_bytes)
                    },
                )?;
            }
            DecoderNode::For { base, .. } => require(base, Some(8))?,
            DecoderNode::Dictionary {
                dictionary,
                entries,
                ..
            } => require(dictionary, entries.checked_mul(8))?,
            DecoderNode::Rle {
                run_lengths,
                run_index,
                runs,
                index_interval,
                ..
            } => {
                require(run_lengths, runs.checked_mul(4))?;
                require(
                    run_index,
                    runs.div_ceil(index_interval)
                        .checked_add(1)
                        .and_then(|x| x.checked_mul(8)),
                )?;
            }
            DecoderNode::Delta {
                restarts,
                restart_interval,
                len,
                ..
            } => require(restarts, len.div_ceil(restart_interval).checked_mul(8))?,
            DecoderNode::Patch {
                positions,
                position_index,
                exceptions,
                count,
                index_interval,
                ..
            } => {
                require(positions, count.checked_mul(4))?;
                require(exceptions, count.checked_mul(8))?;
                require(
                    position_index,
                    count
                        .div_ceil(index_interval)
                        .checked_add(1)
                        .and_then(|x| x.checked_mul(8)),
                )?;
            }
            DecoderNode::Nullable {
                validity,
                rank_index,
                logical_len,
                rank_interval,
                ..
            } => {
                if index != decoder.root().0 || !rank_interval.is_multiple_of(8) {
                    return Err("native nullable layout is unsupported".into());
                }
                require(validity, Some(logical_len.div_ceil(8)))?;
                require(
                    rank_index,
                    logical_len
                        .div_ceil(rank_interval)
                        .checked_add(1)
                        .and_then(|x| x.checked_mul(4)),
                )?;
            }
        }
    }
    Ok(())
}
