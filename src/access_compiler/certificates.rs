use super::{OutputGuarantee, Span, push_range};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidatePlan {
    pub blocks: Vec<usize>,
    pub block_rows: usize,
    pub metadata_bytes: usize,
    pub guarantee: OutputGuarantee,
}

impl CandidatePlan {
    pub fn candidate_rows(&self, rows: usize) -> usize {
        self.blocks
            .iter()
            .map(|block| {
                let start = block.saturating_mul(self.block_rows);
                (start + self.block_rows).min(rows).saturating_sub(start)
            })
            .sum()
    }
}

#[derive(Clone, Debug)]
pub struct BlockBloom {
    block_rows: usize,
    bytes_per_block: usize,
    hashes: u8,
    bits: Vec<u8>,
    blocks: usize,
}

impl BlockBloom {
    pub fn build(
        values: &[Option<i64>],
        block_rows: usize,
        bits_per_value: usize,
    ) -> Result<Self, String> {
        if values.is_empty() || block_rows == 0 || bits_per_value == 0 {
            return Err("Bloom construction arguments are invalid".into());
        }
        let block_bits = (block_rows * bits_per_value).next_multiple_of(256);
        let bytes_per_block = block_bits / 8;
        let blocks = values.len().div_ceil(block_rows);
        let hashes = ((bits_per_value as f64 * std::f64::consts::LN_2).round() as u8).clamp(1, 16);
        let mut output = Self {
            block_rows,
            bytes_per_block,
            hashes,
            bits: vec![0; blocks * bytes_per_block],
            blocks,
        };
        for (row, value) in values.iter().enumerate() {
            if let Some(value) = value {
                output.insert(row / block_rows, *value);
            }
        }
        Ok(output)
    }

    pub fn probe_eq(&self, value: i64) -> CandidatePlan {
        CandidatePlan {
            blocks: (0..self.blocks)
                .filter(|&block| self.may_contain(block, value))
                .collect(),
            block_rows: self.block_rows,
            metadata_bytes: self.bits.len(),
            guarantee: OutputGuarantee::CandidateBitmap,
        }
    }

    pub fn probe_in(&self, values: &[i64]) -> CandidatePlan {
        CandidatePlan {
            blocks: (0..self.blocks)
                .filter(|&block| values.iter().any(|&value| self.may_contain(block, value)))
                .collect(),
            block_rows: self.block_rows,
            metadata_bytes: self.bits.len(),
            guarantee: OutputGuarantee::CandidateBitmap,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bits.len()
    }

    fn insert(&mut self, block: usize, value: i64) {
        let bit_count = self.bytes_per_block * 8;
        let (first, second) = bloom_hashes(value);
        for hash in 0..self.hashes {
            let bit =
                first.wrapping_add(u64::from(hash).wrapping_mul(second | 1)) as usize % bit_count;
            self.bits[block * self.bytes_per_block + bit / 8] |= 1 << (bit % 8);
        }
    }

    fn may_contain(&self, block: usize, value: i64) -> bool {
        let bit_count = self.bytes_per_block * 8;
        let (first, second) = bloom_hashes(value);
        (0..self.hashes).all(|hash| {
            let bit =
                first.wrapping_add(u64::from(hash).wrapping_mul(second | 1)) as usize % bit_count;
            self.bits[block * self.bytes_per_block + bit / 8] >> (bit % 8) & 1 == 1
        })
    }
}

#[derive(Clone, Debug)]
pub struct BlockMinMax {
    block_rows: usize,
    bounds: Vec<Option<(i64, i64)>>,
}

impl BlockMinMax {
    pub fn build(values: &[Option<i64>], block_rows: usize) -> Result<Self, String> {
        if values.is_empty() || block_rows == 0 {
            return Err("min/max construction arguments are invalid".into());
        }
        let bounds = values
            .chunks(block_rows)
            .map(|block| {
                let mut present = block.iter().flatten().copied();
                let first = present.next()?;
                Some(present.fold((first, first), |(low, high), value| {
                    (low.min(value), high.max(value))
                }))
            })
            .collect();
        Ok(Self { block_rows, bounds })
    }

    pub fn probe_eq(&self, value: i64) -> CandidatePlan {
        CandidatePlan {
            blocks: self
                .bounds
                .iter()
                .enumerate()
                .filter_map(|(block, bounds)| {
                    bounds
                        .is_some_and(|(low, high)| low <= value && value <= high)
                        .then_some(block)
                })
                .collect(),
            block_rows: self.block_rows,
            metadata_bytes: self.bounds.len() * 16,
            guarantee: OutputGuarantee::CandidateBitmap,
        }
    }

    pub fn probe_in(&self, values: &[i64]) -> CandidatePlan {
        CandidatePlan {
            blocks: self
                .bounds
                .iter()
                .enumerate()
                .filter_map(|(block, bounds)| {
                    bounds
                        .is_some_and(|(low, high)| {
                            values.iter().any(|value| low <= *value && *value <= high)
                        })
                        .then_some(block)
                })
                .collect(),
            block_rows: self.block_rows,
            metadata_bytes: self.bounds.len() * 16,
            guarantee: OutputGuarantee::CandidateBitmap,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bounds.len() * 16
    }
}

#[derive(Clone, Debug)]
pub struct SparseFence {
    stride: usize,
    entries: Vec<(i64, usize)>,
    rows: usize,
}

impl SparseFence {
    pub fn build_equal_budget(values: &[Option<i64>], byte_budget: usize) -> Result<Self, String> {
        if values.is_empty() || byte_budget < 16 || values.iter().any(Option::is_none) {
            return Err("sparse fence requires a non-null column and at least one entry".into());
        }
        let dense = values.iter().flatten().copied().collect::<Vec<_>>();
        if dense.windows(2).any(|pair| pair[0] > pair[1]) {
            return Err("sparse fence requires a non-decreasing column".into());
        }
        let max_entries = (byte_budget / 16).max(1);
        let stride = dense.len().div_ceil(max_entries).max(1);
        let entries = dense
            .iter()
            .enumerate()
            .step_by(stride)
            .map(|(row, &value)| (value, row))
            .collect();
        Ok(Self {
            stride,
            entries,
            rows: dense.len(),
        })
    }

    pub fn probe_eq(&self, value: i64) -> CandidatePlan {
        let lower = self.entries.partition_point(|(entry, _)| *entry < value);
        let upper = self.entries.partition_point(|(entry, _)| *entry <= value);
        let start = lower
            .checked_sub(1)
            .map_or(0, |entry| self.entries[entry].1);
        let end = self.entries.get(upper).map_or(self.rows, |entry| entry.1);
        let first_block = start / self.stride;
        let last_block = end.div_ceil(self.stride);
        CandidatePlan {
            blocks: (first_block..last_block).collect(),
            block_rows: self.stride,
            metadata_bytes: self.entries.len() * 16,
            guarantee: OutputGuarantee::CandidateBitmap,
        }
    }

    pub fn bytes(&self) -> usize {
        self.entries.len() * 16
    }
}

pub fn intersect_candidates(left: &CandidatePlan, right: &CandidatePlan) -> CandidatePlan {
    assert_eq!(left.block_rows, right.block_rows);
    CandidatePlan {
        blocks: left
            .blocks
            .iter()
            .copied()
            .filter(|block| right.blocks.binary_search(block).is_ok())
            .collect(),
        block_rows: left.block_rows,
        metadata_bytes: left.metadata_bytes + right.metadata_bytes,
        guarantee: OutputGuarantee::CandidateBitmap,
    }
}

pub fn refine_eq(
    values: &[Option<i64>],
    candidates: &CandidatePlan,
    value: i64,
) -> Result<Vec<Span>, String> {
    if candidates.guarantee != OutputGuarantee::CandidateBitmap {
        return Err("equality refinement requires a candidate guarantee".into());
    }
    let mut ranges = Vec::new();
    for &block in &candidates.blocks {
        let start = block
            .checked_mul(candidates.block_rows)
            .ok_or("candidate block offset overflow")?;
        let end = (start + candidates.block_rows).min(values.len());
        if start >= values.len() {
            return Err("candidate block is outside the column".into());
        }
        for (offset, candidate) in values[start..end].iter().enumerate() {
            if *candidate == Some(value) {
                push_range(&mut ranges, start + offset, start + offset + 1);
            }
        }
    }
    Ok(ranges)
}

pub fn refine_in(
    values: &[Option<i64>],
    candidates: &CandidatePlan,
    targets: &[i64],
) -> Result<Vec<Span>, String> {
    if candidates.guarantee != OutputGuarantee::CandidateBitmap {
        return Err("IN refinement requires a candidate guarantee".into());
    }
    let mut ranges = Vec::new();
    for &block in &candidates.blocks {
        let start = block
            .checked_mul(candidates.block_rows)
            .ok_or("candidate block offset overflow")?;
        let end = (start + candidates.block_rows).min(values.len());
        if start >= values.len() {
            return Err("candidate block is outside the column".into());
        }
        for (offset, candidate) in values[start..end].iter().enumerate() {
            if candidate.is_some_and(|value| targets.contains(&value)) {
                push_range(&mut ranges, start + offset, start + offset + 1);
            }
        }
    }
    Ok(ranges)
}

fn bloom_hashes(value: i64) -> (u64, u64) {
    let first = splitmix64(value as u64 ^ 0x9e37_79b9_7f4a_7c15);
    (first, splitmix64(first ^ 0xd6e8_feb8_6659_fd93))
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
