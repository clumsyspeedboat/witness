//! Opt-in structural work estimates, not a timing model or a correctness rule.
//!
//! The fixed-priority planners remain the ablation. These estimates may only
//! replace a licensed shortcut with a scan. They use no query observations,
//! measured constants, or oracle values. All arithmetic saturates: an extreme
//! descriptor can make a shortcut unattractive, never wrap it into a cheap one.

use super::{PageAction, RootDictionary};
use crate::access_compiler::{AccessSet, DecoderNode, EncodedColumn, NodeId, Span};

#[derive(Clone, Copy, Debug, Default)]
struct NodeWork {
    rows: usize,
    probe: usize,
    scan: usize,
}

/// Cached once alongside root facts; querying does not walk the codec tree or
/// construct physical closures merely to decide whether a shortcut is cheap.
#[derive(Clone, Debug, Default)]
pub(super) struct AccessCost {
    root: NodeWork,
    search_rows: usize,
    search_probe: usize,
    search_index: usize,
    frame_work: usize,
    largest_frame: usize,
    dictionary_entries: Option<usize>,
}

impl AccessCost {
    pub(super) fn gather(
        column: &EncodedColumn,
        dictionary: Option<RootDictionary>,
    ) -> Result<Self, String> {
        let decoder = &column.decoder;
        let mut work = Vec::<NodeWork>::with_capacity(decoder.nodes().len());
        for node in decoder.nodes() {
            let child = |id: NodeId| work[id.0];
            let profile = match *node {
                DecoderNode::BitUnpack { len, .. } => NodeWork {
                    rows: len,
                    probe: 1,
                    scan: len,
                },
                DecoderNode::For { values, .. } => {
                    let child = child(values);
                    NodeWork {
                        probe: child.probe.saturating_add(1),
                        // FOR scans compare codes and add a base per batch.
                        scan: child.scan.saturating_add(child.rows.div_ceil(1024)),
                        ..child
                    }
                }
                DecoderNode::Dictionary { ids, entries, .. } => {
                    let child = child(ids);
                    NodeWork {
                        probe: child.probe.saturating_add(1),
                        // A scan tests each table entry once, then reads ids.
                        scan: child.scan.saturating_add(entries),
                        ..child
                    }
                }
                DecoderNode::Delta {
                    deltas,
                    restart_interval,
                    len,
                    ..
                } => {
                    let child = child(deltas);
                    NodeWork {
                        rows: len,
                        // A random probe reconstructs half a restart block on
                        // average; a scan advances through each delta once.
                        probe: child
                            .probe
                            .saturating_add(1)
                            .saturating_mul(restart_interval.min(len).div_ceil(2))
                            .saturating_add(1),
                        scan: child.scan.saturating_add(len),
                    }
                }
                DecoderNode::Rle {
                    values,
                    len,
                    runs,
                    index_interval,
                    ..
                } => {
                    let child = child(values);
                    NodeWork {
                        rows: len,
                        probe: child
                            .probe
                            .saturating_add(log_steps(runs.div_ceil(index_interval)))
                            .saturating_add(index_interval.min(runs).div_ceil(2))
                            .saturating_add(2),
                        scan: child.scan.saturating_add(runs),
                    }
                }
                DecoderNode::Nullable {
                    values,
                    logical_len,
                    rank_interval,
                    ..
                } => {
                    let child = child(values);
                    NodeWork {
                        rows: logical_len,
                        probe: child
                            .probe
                            .saturating_add(rank_interval.div_ceil(64))
                            .saturating_add(2),
                        scan: child.scan.saturating_add(logical_len),
                    }
                }
                DecoderNode::Patch {
                    values,
                    count,
                    index_interval,
                    ..
                } => {
                    let child = child(values);
                    NodeWork {
                        probe: child
                            .probe
                            .saturating_add(log_steps(count.div_ceil(index_interval)))
                            .saturating_add(index_interval.min(count)),
                        scan: child.scan.saturating_add(count),
                        ..child
                    }
                }
            };
            work.push(profile);
        }
        let root = work[decoder.root().0];
        let mut result = Self {
            root,
            search_rows: root.rows,
            search_probe: root.probe,
            dictionary_entries: dictionary.map(|dictionary| dictionary.entries),
            ..Self::default()
        };
        // Match vector::RunSearch: an RLE root, or dictionary over RLE ids,
        // searches run values rather than doing a row-to-run lookup per probe.
        let (node, mapping) = match *decoder.node(decoder.root())? {
            DecoderNode::Dictionary { ids, .. } => (ids, 1_usize),
            _ => (decoder.root(), 0),
        };
        if let DecoderNode::Rle {
            values,
            runs,
            index_interval,
            ..
        } = *decoder.node(node)?
        {
            result.search_rows = runs;
            result.search_probe = work[values.0].probe.saturating_add(mapping);
            result.search_index = index_interval.min(runs).saturating_add(4);
        }
        let layout = column.page.layout();
        let mut fields = AccessSet::default();
        for field in &layout.fields {
            if field.id != layout.metadata {
                fields.insert(field.id, Span::new(0, field.length)?);
            }
        }
        // The complete field closure accounts for shared frames/dependencies
        // once. Frame decode work is expressed in machine words, and transfer
        // work in cache lines; these are units, not fitted nanoseconds.
        let closure = layout.closure(&fields)?;
        for frame in &layout.frames {
            let end = frame.offset.saturating_add(frame.compressed_length);
            if closure
                .delivered
                .iter()
                .any(|span| span.start < end && frame.offset < span.end)
            {
                let units = frame
                    .decoded_length
                    .div_ceil(8)
                    .saturating_add(frame.compressed_length.div_ceil(64));
                result.frame_work = result.frame_work.saturating_add(units);
                result.largest_frame = result.largest_frame.max(units);
            }
        }
        Ok(result)
    }

    pub(super) fn prefers_scan(&self, action: &PageAction) -> bool {
        let (units, probes) = match action {
            PageAction::Search { nulls, .. } => {
                // Nullable roots do not use vector::RunSearch. The four extra
                // probes are execution-stage checks of the computed interval.
                let (rows, probe, index) = if *nulls == super::NullPlacement::NoNulls {
                    (self.search_rows, self.search_probe, self.search_index)
                } else {
                    (self.root.rows, self.root.probe, 0)
                };
                let probes = log_steps(rows).saturating_mul(2);
                let units = probes
                    .saturating_mul(probe)
                    .saturating_add(index)
                    .saturating_add(self.root.probe.saturating_mul(4));
                (units, probes.saturating_add(4))
            }
            PageAction::SearchPiecewise { max_rows, .. } => {
                let blocks = self.root.rows.div_ceil(*max_rows);
                let probes = log_steps((*max_rows).min(self.root.rows))
                    .saturating_mul(2)
                    .saturating_mul(blocks);
                (probes.saturating_mul(self.root.probe), probes)
            }
            PageAction::Translate(_) => {
                // Both alternatives read ids. Translation adds two searches
                // and four table-boundary checks; a scan tests all entries.
                return self.dictionary_entries.is_some_and(|entries| {
                    log_steps(entries).saturating_mul(2).saturating_add(4) >= entries
                });
            }
            // Exclusions and acceptances are never withdrawn by this policy.
            PageAction::Skip(_) | PageAction::Accept(_) | PageAction::Scan => return false,
        };
        let search_frames = probes
            .saturating_mul(self.largest_frame)
            .min(self.frame_work);
        units.saturating_add(search_frames) >= self.root.scan.saturating_add(self.frame_work)
    }
}

fn log_steps(rows: usize) -> usize {
    // Upper bound on the comparisons in lower_bound over `rows` entries.
    (usize::BITS - rows.leading_zeros()) as usize
}
