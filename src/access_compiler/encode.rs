use std::collections::BTreeSet;

use super::format::{CheckedInvariants, PageBuilder};
use super::{
    DecoderIr, DecoderNode, DeltaCoding, DependencyRule, NodeId, NullPlacement, SerializedPage,
};

const MINIBLOCK_ROWS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recipe {
    BitPack,
    Rle {
        index_interval: usize,
        values: Box<Recipe>,
    },
    Dictionary(Box<Recipe>),
    Delta {
        restart_interval: usize,
        deltas: Box<Recipe>,
    },
    UnsignedDelta {
        restart_interval: usize,
        deltas: Box<Recipe>,
    },
    For(Box<Recipe>),
    Patch {
        index_interval: usize,
        values: Box<Recipe>,
    },
    Nullable {
        rank_interval: usize,
        values: Box<Recipe>,
    },
    Frame(Box<Recipe>),
}

impl Recipe {
    pub fn name(&self) -> String {
        match self {
            Self::BitPack => "BitPack".into(),
            Self::Rle { values, .. } => format!("RLE({})", values.name()),
            Self::Dictionary(ids) => format!("Dictionary({})", ids.name()),
            Self::Delta { deltas, .. } => format!("Delta({})", deltas.name()),
            Self::UnsignedDelta { deltas, .. } => {
                format!("UnsignedDelta({})", deltas.name())
            }
            Self::For(values) => format!("FOR({})", values.name()),
            Self::Patch { values, .. } => format!("Patch({})", values.name()),
            Self::Nullable { values, .. } => format!("Nullable({})", values.name()),
            Self::Frame(values) => format!("Frame({})", values.name()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct InputColumn {
    pub values: Vec<Option<i64>>,
    pub patch_rows: BTreeSet<usize>,
}

impl InputColumn {
    pub fn dense(values: Vec<i64>) -> Self {
        Self {
            values: values.into_iter().map(Some).collect(),
            patch_rows: BTreeSet::new(),
        }
    }

    pub fn nullable(values: Vec<Option<i64>>) -> Self {
        Self {
            values,
            patch_rows: BTreeSet::new(),
        }
    }

    pub fn with_patch_rows(mut self, rows: impl IntoIterator<Item = usize>) -> Self {
        self.patch_rows.extend(rows);
        self
    }
}

#[derive(Clone, Debug)]
pub struct EncodedColumn {
    pub recipe: Recipe,
    pub decoder: DecoderIr,
    pub page: SerializedPage,
    pub truth: Vec<Option<i64>>,
}

#[derive(Clone, Debug)]
struct Batch {
    values: Vec<Option<i128>>,
    patch_rows: BTreeSet<usize>,
}

impl Batch {
    fn dense(self) -> Result<DenseBatch, String> {
        let values = self
            .values
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or("nullable input reached a non-nullable decoder primitive")?;
        Ok(DenseBatch {
            values,
            patch_rows: self.patch_rows,
        })
    }
}

#[derive(Clone, Debug)]
struct DenseBatch {
    values: Vec<i128>,
    patch_rows: BTreeSet<usize>,
}

pub fn encode(recipe: &Recipe, input: InputColumn) -> Result<EncodedColumn, String> {
    if input.values.is_empty() {
        return Err("access-compiler pages require at least one row".into());
    }
    if input
        .patch_rows
        .iter()
        .any(|&row| row >= input.values.len())
    {
        return Err("patch row is outside the input column".into());
    }
    let truth = input.values.clone();
    let non_decreasing_non_null = truth
        .iter()
        .flatten()
        .try_fold(None, |previous, &value| match previous {
            Some(previous) if previous > value => Err(()),
            _ => Ok(Some(value)),
        })
        .is_ok();
    let null_placement = null_placement(&truth);
    let invariants = CheckedInvariants {
        non_decreasing: non_decreasing_non_null && null_placement == NullPlacement::NoNulls,
        non_decreasing_non_null,
        null_placement,
    };
    let batch = Batch {
        values: input
            .values
            .into_iter()
            .map(|value| value.map(i128::from))
            .collect(),
        patch_rows: input.patch_rows,
    };
    let mut builder = PageBuilder::new(invariants);
    let mut nodes = Vec::new();
    let root = encode_node(recipe, batch, &mut builder, &mut nodes)?;
    let decoder = DecoderIr::new(nodes, root)?;
    let page = builder.finish()?;
    Ok(EncodedColumn {
        recipe: recipe.clone(),
        decoder,
        page,
        truth,
    })
}

fn encode_node(
    recipe: &Recipe,
    batch: Batch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    match recipe {
        Recipe::Frame(child) => {
            let start = builder.field_count();
            let node = encode_node(child, batch, builder, nodes)?;
            builder.frame_fields(start, builder.field_count())?;
            Ok(node)
        }
        Recipe::Nullable {
            rank_interval,
            values,
        } => encode_nullable(*rank_interval, values, batch, builder, nodes),
        _ => encode_dense_node(recipe, batch.dense()?, builder, nodes),
    }
}

fn encode_dense_node(
    recipe: &Recipe,
    batch: DenseBatch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    match recipe {
        Recipe::BitPack => encode_bitpack(batch, builder, nodes),
        Recipe::Rle {
            index_interval,
            values,
        } => encode_rle(*index_interval, values, batch, builder, nodes),
        Recipe::Dictionary(ids) => encode_dictionary(ids, batch, builder, nodes),
        Recipe::Delta {
            restart_interval,
            deltas,
        } => encode_delta(
            *restart_interval,
            deltas,
            batch,
            builder,
            nodes,
            DeltaCoding::ZigZag,
        ),
        Recipe::UnsignedDelta {
            restart_interval,
            deltas,
        } => encode_delta(
            *restart_interval,
            deltas,
            batch,
            builder,
            nodes,
            DeltaCoding::Unsigned,
        ),
        Recipe::For(values) => encode_for(values, batch, builder, nodes),
        Recipe::Patch {
            index_interval,
            values,
        } => encode_patch(*index_interval, values, batch, builder, nodes),
        Recipe::Nullable { .. } => Err("nested Nullable requires an optional batch".into()),
        Recipe::Frame(child) => {
            let start = builder.field_count();
            let node = encode_dense_node(child, batch, builder, nodes)?;
            builder.frame_fields(start, builder.field_count())?;
            Ok(node)
        }
    }
}

fn encode_bitpack(
    batch: DenseBatch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    let values = batch
        .values
        .iter()
        .map(|&value| u64::try_from(value).map_err(|_| "BitPack input does not fit u64"))
        .collect::<Result<Vec<_>, _>>()?;
    let maximum = values.iter().copied().max().unwrap_or(0);
    let width = (u64::BITS - maximum.leading_zeros()) as u8;
    let miniblock_bytes = (MINIBLOCK_ROWS * width as usize).div_ceil(8).max(8);
    let blocks = values.len().div_ceil(MINIBLOCK_ROWS);
    let mut packed = vec![
        0_u8;
        if width == 0 {
            0
        } else {
            blocks * miniblock_bytes
        }
    ];
    if width > 0 {
        for (row, value) in values.into_iter().enumerate() {
            let block = row / MINIBLOCK_ROWS;
            let in_block = row % MINIBLOCK_ROWS;
            let bit_offset = block * miniblock_bytes * 8 + in_block * width as usize;
            for bit in 0..width as usize {
                if value >> bit & 1 == 1 {
                    packed[(bit_offset + bit) / 8] |= 1 << ((bit_offset + bit) % 8);
                }
            }
        }
    }
    let stream = builder.add_field("bitpack.miniblocks", packed, 64, miniblock_bytes)?;
    push_node(
        nodes,
        DecoderNode::BitUnpack {
            stream,
            width,
            len: batch.values.len(),
            miniblock_rows: MINIBLOCK_ROWS,
            miniblock_bytes,
        },
    )
}

fn encode_for(
    child: &Recipe,
    batch: DenseBatch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    let base = *batch.values.iter().min().ok_or("empty FOR batch")?;
    let base_i64 = i64::try_from(base).map_err(|_| "FOR base does not fit i64")?;
    let base_field = builder.add_field("for.base", base_i64.to_le_bytes().to_vec(), 8, 8)?;
    let offsets = batch
        .values
        .iter()
        .map(|value| value.checked_sub(base).ok_or("FOR subtraction overflow"))
        .collect::<Result<Vec<_>, _>>()?;
    let values = encode_dense_node(
        child,
        DenseBatch {
            values: offsets,
            patch_rows: batch.patch_rows,
        },
        builder,
        nodes,
    )?;
    push_node(
        nodes,
        DecoderNode::For {
            base: base_field,
            values,
        },
    )
}

fn encode_delta(
    restart_interval: usize,
    child: &Recipe,
    batch: DenseBatch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
    coding: DeltaCoding,
) -> Result<NodeId, String> {
    if restart_interval == 0 || !restart_interval.is_multiple_of(MINIBLOCK_ROWS) {
        return Err("delta restart interval must be a positive miniblock multiple".into());
    }
    let mut restart_bytes = Vec::new();
    let mut codes = vec![0_i128; batch.values.len()];
    for block_start in (0..batch.values.len()).step_by(restart_interval) {
        let anchor = i64::try_from(batch.values[block_start])
            .map_err(|_| "delta restart does not fit i64")?;
        restart_bytes.extend_from_slice(&anchor.to_le_bytes());
        let block_end = (block_start + restart_interval).min(batch.values.len());
        for (row, code) in codes
            .iter_mut()
            .enumerate()
            .take(block_end)
            .skip(block_start + 1)
        {
            let delta = batch.values[row]
                .checked_sub(batch.values[row - 1])
                .ok_or("delta subtraction overflow")?;
            *code = match coding {
                DeltaCoding::ZigZag => {
                    let delta = i64::try_from(delta).map_err(|_| "delta does not fit i64")?;
                    i128::from(zigzag(delta))
                }
                DeltaCoding::Unsigned => {
                    let delta = u64::try_from(delta)
                        .map_err(|_| "unsigned delta requires non-decreasing input")?;
                    i128::from(delta)
                }
            };
        }
    }
    let restarts = builder.add_field("delta.restarts", restart_bytes, 8, 64)?;
    let deltas = encode_dense_node(
        child,
        DenseBatch {
            values: codes,
            patch_rows: batch.patch_rows,
        },
        builder,
        nodes,
    )?;
    if let DecoderNode::BitUnpack {
        stream,
        miniblock_bytes,
        ..
    } = nodes[deltas.0]
    {
        builder.add_dependency(DependencyRule::Restart {
            data: stream,
            restarts,
            data_bytes_per_restart: restart_interval / MINIBLOCK_ROWS * miniblock_bytes,
            bytes_per_restart: 8,
        });
    }
    push_node(
        nodes,
        DecoderNode::Delta {
            deltas,
            restarts,
            restart_interval,
            len: batch.values.len(),
            coding,
        },
    )
}

fn encode_dictionary(
    child: &Recipe,
    batch: DenseBatch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    let mut dictionary = batch.values.clone();
    dictionary.sort_unstable();
    dictionary.dedup();
    let mut dictionary_bytes = Vec::with_capacity(dictionary.len() * 8);
    for &value in &dictionary {
        dictionary_bytes.extend_from_slice(
            &i64::try_from(value)
                .map_err(|_| "dictionary value does not fit i64")?
                .to_le_bytes(),
        );
    }
    let dictionary_field = builder.add_field("dictionary.values", dictionary_bytes, 8, 64)?;
    let ids = batch
        .values
        .iter()
        .map(|value| {
            dictionary
                .binary_search(value)
                .map(|id| id as i128)
                .map_err(|_| "dictionary construction lost a value")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let ids = encode_dense_node(
        child,
        DenseBatch {
            values: ids,
            patch_rows: batch.patch_rows,
        },
        builder,
        nodes,
    )?;
    push_node(
        nodes,
        DecoderNode::Dictionary {
            ids,
            dictionary: dictionary_field,
            entries: dictionary.len(),
            sorted_unique: true,
        },
    )
}

fn encode_rle(
    index_interval: usize,
    child: &Recipe,
    batch: DenseBatch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    if index_interval == 0 {
        return Err("RLE index interval must be positive".into());
    }
    let mut run_values = Vec::new();
    let mut run_lengths = Vec::new();
    for value in &batch.values {
        if run_values.last() == Some(value) {
            *run_lengths.last_mut().unwrap() += 1_u32;
        } else {
            run_values.push(*value);
            run_lengths.push(1_u32);
        }
    }
    let mut lengths_bytes = Vec::with_capacity(run_lengths.len() * 4);
    for length in &run_lengths {
        lengths_bytes.extend_from_slice(&length.to_le_bytes());
    }
    let mut index_bytes = Vec::new();
    let mut row = 0_u32;
    for (run, length) in run_lengths.iter().enumerate() {
        if run % index_interval == 0 {
            index_bytes.extend_from_slice(&row.to_le_bytes());
            index_bytes.extend_from_slice(&(run as u32).to_le_bytes());
        }
        row = row.checked_add(*length).ok_or("RLE row index overflow")?;
    }
    index_bytes.extend_from_slice(&row.to_le_bytes());
    index_bytes.extend_from_slice(&(run_lengths.len() as u32).to_le_bytes());
    let run_lengths_field = builder.add_field("rle.lengths", lengths_bytes, 4, 128)?;
    let run_index = builder.add_field("rle.index", index_bytes, 8, 64)?;
    builder.add_dependency(DependencyRule::IndexedStream {
        data: run_lengths_field,
        index: run_index,
        data_block_bytes: index_interval * 4,
        index_entry_bytes: 8,
    });
    let values = encode_dense_node(
        child,
        DenseBatch {
            values: run_values,
            patch_rows: BTreeSet::new(),
        },
        builder,
        nodes,
    )?;
    push_node(
        nodes,
        DecoderNode::Rle {
            values,
            run_lengths: run_lengths_field,
            run_index,
            len: batch.values.len(),
            runs: run_lengths.len(),
            index_interval,
        },
    )
}

fn encode_patch(
    index_interval: usize,
    child: &Recipe,
    batch: DenseBatch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    if index_interval == 0 {
        return Err("patch index interval must be positive".into());
    }
    let positions = batch.patch_rows.iter().copied().collect::<Vec<_>>();
    let mut main = batch.values.clone();
    let mut exceptions = Vec::with_capacity(positions.len());
    for &position in &positions {
        exceptions.push(batch.values[position]);
        let replacement = nearest_regular(&batch.values, &batch.patch_rows, position).unwrap_or(0);
        main[position] = replacement;
    }
    let mut position_bytes = Vec::with_capacity(positions.len() * 4);
    let mut exception_bytes = Vec::with_capacity(exceptions.len() * 8);
    let mut index_bytes = Vec::new();
    for (exception, (&position, &value)) in positions.iter().zip(&exceptions).enumerate() {
        position_bytes.extend_from_slice(&(position as u32).to_le_bytes());
        exception_bytes.extend_from_slice(
            &i64::try_from(value)
                .map_err(|_| "patch exception does not fit i64")?
                .to_le_bytes(),
        );
        if exception % index_interval == 0 {
            index_bytes.extend_from_slice(&(position as u32).to_le_bytes());
            index_bytes.extend_from_slice(&(exception as u32).to_le_bytes());
        }
    }
    index_bytes.extend_from_slice(&(batch.values.len() as u32).to_le_bytes());
    index_bytes.extend_from_slice(&(positions.len() as u32).to_le_bytes());
    let position_field = builder.add_field("patch.positions", position_bytes, 4, 128)?;
    let position_index = builder.add_field("patch.index", index_bytes, 8, 64)?;
    let exception_field = builder.add_field("patch.exceptions", exception_bytes, 8, 64)?;
    builder.add_dependency(DependencyRule::IndexedStream {
        data: position_field,
        index: position_index,
        data_block_bytes: index_interval * 4,
        index_entry_bytes: 8,
    });
    let values = encode_dense_node(
        child,
        DenseBatch {
            values: main,
            patch_rows: BTreeSet::new(),
        },
        builder,
        nodes,
    )?;
    push_node(
        nodes,
        DecoderNode::Patch {
            values,
            positions: position_field,
            position_index,
            exceptions: exception_field,
            count: positions.len(),
            index_interval,
        },
    )
}

fn encode_nullable(
    rank_interval: usize,
    child: &Recipe,
    batch: Batch,
    builder: &mut PageBuilder,
    nodes: &mut Vec<DecoderNode>,
) -> Result<NodeId, String> {
    if rank_interval == 0 || !rank_interval.is_multiple_of(8) {
        return Err("nullable rank interval must be a positive byte multiple".into());
    }
    let logical_len = batch.values.len();
    let mut validity = vec![0_u8; logical_len.div_ceil(8)];
    let mut compact = Vec::new();
    let mut compact_patches = BTreeSet::new();
    for (row, value) in batch.values.into_iter().enumerate() {
        if let Some(value) = value {
            validity[row / 8] |= 1 << (row % 8);
            if batch.patch_rows.contains(&row) {
                compact_patches.insert(compact.len());
            }
            compact.push(value);
        }
    }
    if compact.is_empty() {
        return Err("all-null pages are outside this compiler microprototype".into());
    }
    let mut rank_bytes = Vec::new();
    let mut rank = 0_u32;
    for block_start in (0..logical_len).step_by(rank_interval) {
        rank_bytes.extend_from_slice(&rank.to_le_bytes());
        let block_end = (block_start + rank_interval).min(logical_len);
        rank += (block_start..block_end)
            .filter(|row| validity[*row / 8] >> (*row % 8) & 1 == 1)
            .count() as u32;
    }
    rank_bytes.extend_from_slice(&rank.to_le_bytes());
    let validity_field = builder.add_field("nullable.validity", validity, 8, rank_interval / 8)?;
    let rank_index = builder.add_field("nullable.rank", rank_bytes, 4, 64)?;
    builder.add_dependency(DependencyRule::IndexedStream {
        data: validity_field,
        index: rank_index,
        data_block_bytes: rank_interval / 8,
        index_entry_bytes: 4,
    });
    let values = encode_dense_node(
        child,
        DenseBatch {
            values: compact,
            patch_rows: compact_patches,
        },
        builder,
        nodes,
    )?;
    push_node(
        nodes,
        DecoderNode::Nullable {
            validity: validity_field,
            rank_index,
            values,
            logical_len,
            nonnull_len: rank as usize,
            rank_interval,
        },
    )
}

fn nearest_regular(values: &[i128], patches: &BTreeSet<usize>, row: usize) -> Option<i128> {
    (1..values.len()).find_map(|distance| {
        row.checked_sub(distance)
            .filter(|candidate| !patches.contains(candidate))
            .or_else(|| {
                row.checked_add(distance)
                    .filter(|candidate| *candidate < values.len() && !patches.contains(candidate))
            })
            .map(|candidate| values[candidate])
    })
}

fn push_node(nodes: &mut Vec<DecoderNode>, node: DecoderNode) -> Result<NodeId, String> {
    let id = NodeId(nodes.len());
    nodes.push(node);
    Ok(id)
}

fn zigzag(value: i64) -> u64 {
    ((value as u64) << 1) ^ ((value >> 63) as u64)
}

fn null_placement(values: &[Option<i64>]) -> NullPlacement {
    let first_value = values.iter().position(Option::is_some);
    let last_value = values.iter().rposition(Option::is_some);
    match (first_value, last_value) {
        (Some(0), Some(last)) if last + 1 == values.len() && values.iter().all(Option::is_some) => {
            NullPlacement::NoNulls
        }
        (Some(first), Some(last))
            if values[..first].iter().all(Option::is_none)
                && values[first..=last].iter().all(Option::is_some)
                && last + 1 == values.len() =>
        {
            NullPlacement::First
        }
        (Some(0), Some(last))
            if values[..=last].iter().all(Option::is_some)
                && values[last + 1..].iter().all(Option::is_none) =>
        {
            NullPlacement::Last
        }
        _ => NullPlacement::Arbitrary,
    }
}
