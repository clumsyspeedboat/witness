use std::collections::BTreeMap;

use super::{FieldId, FrameId};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Result<Self, String> {
        if start > end {
            return Err(format!("invalid span {start}..{end}"));
        }
        Ok(Self { start, end })
    }

    pub fn len(self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccessSet(BTreeMap<FieldId, Vec<Span>>);

impl AccessSet {
    pub fn insert(&mut self, field: FieldId, span: Span) -> bool {
        if span.is_empty() {
            return false;
        }
        let spans = self.0.entry(field).or_default();
        if spans
            .iter()
            .any(|covered| covered.start <= span.start && span.end <= covered.end)
        {
            return false;
        }
        let before = spans.clone();
        spans.push(span);
        merge_spans(spans);
        *spans != before
    }

    pub fn spans(&self, field: FieldId) -> &[Span] {
        self.0.get(&field).map_or(&[], Vec::as_slice)
    }

    pub fn iter(&self) -> impl Iterator<Item = (FieldId, &[Span])> {
        self.0
            .iter()
            .map(|(field, spans)| (*field, spans.as_slice()))
    }

    pub fn bytes(&self) -> usize {
        self.0.values().flatten().copied().map(Span::len).sum()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameCodec {
    Zstd,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FieldLocation {
    Direct {
        offset: usize,
    },
    Framed {
        frame: FrameId,
        decoded_offset: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldLayout {
    pub id: FieldId,
    pub name: String,
    pub length: usize,
    pub alignment: usize,
    pub read_granularity: usize,
    pub location: FieldLocation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameLayout {
    pub id: FrameId,
    pub codec: FrameCodec,
    pub offset: usize,
    pub compressed_length: usize,
    pub decoded_length: usize,
    pub alignment: usize,
    /// The frame continues the decoded stream of the frame before it. A
    /// chunked frame is a head frame followed by continuation frames, each
    /// compressed on its own, so a read decompresses only the chunks it spans.
    pub continues: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DependencyRule {
    DependentField {
        source: FieldId,
        prerequisite: FieldId,
    },
    IndexedStream {
        data: FieldId,
        index: FieldId,
        data_block_bytes: usize,
        index_entry_bytes: usize,
    },
    Restart {
        data: FieldId,
        restarts: FieldId,
        data_bytes_per_restart: usize,
        bytes_per_restart: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutIr {
    pub metadata: FieldId,
    pub fields: Vec<FieldLayout>,
    pub frames: Vec<FrameLayout>,
    pub dependencies: Vec<DependencyRule>,
    pub file_length: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ByteClosure {
    pub fields: AccessSet,
    pub delivered: Vec<Span>,
    pub logical_bytes: usize,
    pub delivered_bytes: usize,
}

impl LayoutIr {
    /// The frames holding one decoded stream: `head` and every continuation
    /// frame after it. An unchunked frame is a group of one.
    pub fn frame_group(&self, head: FrameId) -> Result<&[FrameLayout], String> {
        let first = self
            .frames
            .get(head.0)
            .ok_or("layout references an absent frame")?;
        if first.continues {
            return Err("layout references a continuation frame directly".into());
        }
        let end = self.frames[head.0 + 1..]
            .iter()
            .position(|frame| !frame.continues)
            .map_or(self.frames.len(), |offset| head.0 + 1 + offset);
        Ok(&self.frames[head.0..end])
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.fields.get(self.metadata.0).is_none() {
            return Err("layout metadata field is absent".into());
        }
        for (index, field) in self.fields.iter().enumerate() {
            if field.id.0 != index || field.alignment == 0 || field.read_granularity == 0 {
                return Err(format!("invalid layout field {index}"));
            }
            match field.location {
                FieldLocation::Direct { offset } => {
                    if offset % field.alignment != 0 || offset + field.length > self.file_length {
                        return Err(format!("direct field {index} is outside its layout"));
                    }
                }
                FieldLocation::Framed {
                    frame,
                    decoded_offset,
                } => {
                    let decoded_length = self
                        .frame_group(frame)
                        .map_err(|error| format!("field {index}: {error}"))?
                        .iter()
                        .map(|frame| frame.decoded_length)
                        .sum::<usize>();
                    if decoded_offset + field.length > decoded_length {
                        return Err(format!("field {index} exceeds decoded frame"));
                    }
                }
            }
        }
        for (index, frame) in self.frames.iter().enumerate() {
            if frame.id.0 != index
                || frame.alignment == 0
                || frame.offset % frame.alignment != 0
                || frame.offset + frame.compressed_length > self.file_length
                || (index == 0 && frame.continues)
            {
                return Err(format!("invalid frame {index}"));
            }
            // Chunk lookup divides by the head's decoded length, so every
            // chunk but the last must match it and the last may not exceed it.
            if frame.continues {
                let head = self.frames[..index]
                    .iter()
                    .rposition(|frame| !frame.continues)
                    .ok_or("continuation frame has no head")?;
                let chunk = self.frames[head].decoded_length;
                let last = self
                    .frames
                    .get(index + 1)
                    .is_none_or(|next| !next.continues);
                if self.frames[index - 1].decoded_length != chunk
                    || frame.decoded_length > chunk
                    || (!last && frame.decoded_length != chunk)
                {
                    return Err(format!("frame {index} breaks its group's chunk size"));
                }
            }
        }
        for rule in &self.dependencies {
            let (source, prerequisite, valid_parameters) = match *rule {
                DependencyRule::DependentField {
                    source,
                    prerequisite,
                } => (source, prerequisite, true),
                DependencyRule::IndexedStream {
                    data,
                    index,
                    data_block_bytes,
                    index_entry_bytes,
                } => (data, index, data_block_bytes > 0 && index_entry_bytes > 0),
                DependencyRule::Restart {
                    data,
                    restarts,
                    data_bytes_per_restart,
                    bytes_per_restart,
                } => (
                    data,
                    restarts,
                    data_bytes_per_restart > 0 && bytes_per_restart > 0,
                ),
            };
            if self.fields.get(source.0).is_none()
                || self.fields.get(prerequisite.0).is_none()
                || !valid_parameters
            {
                return Err("invalid layout dependency".into());
            }
        }
        Ok(())
    }

    pub fn closure(&self, seed: &AccessSet) -> Result<ByteClosure, String> {
        self.validate()?;
        let mut fields = seed.clone();
        loop {
            let mut changed = false;
            for rule in &self.dependencies {
                changed |= apply_dependency(rule, &mut fields, &self.fields)?;
            }
            if !changed {
                break;
            }
        }
        let mut delivered = Vec::new();
        for (field_id, spans) in fields.iter() {
            let field = self
                .fields
                .get(field_id.0)
                .ok_or_else(|| format!("access references absent field {}", field_id.0))?;
            for span in spans {
                if span.end > field.length {
                    return Err(format!("access exceeds field {}", field_id.0));
                }
                match field.location {
                    FieldLocation::Direct { offset } => {
                        let start = span.start / field.read_granularity * field.read_granularity;
                        let end = span
                            .end
                            .div_ceil(field.read_granularity)
                            .saturating_mul(field.read_granularity)
                            .min(field.length);
                        delivered.push(Span::new(offset + start, offset + end)?);
                    }
                    FieldLocation::Framed {
                        frame,
                        decoded_offset,
                    } => {
                        let decoded =
                            Span::new(decoded_offset + span.start, decoded_offset + span.end)?;
                        for chunk in frame_chunks(self.frame_group(frame)?, decoded) {
                            delivered.push(Span::new(
                                chunk.offset,
                                chunk.offset + chunk.compressed_length,
                            )?);
                        }
                    }
                }
            }
        }
        merge_spans(&mut delivered);
        let logical_bytes = fields.bytes();
        let delivered_bytes = delivered.iter().copied().map(Span::len).sum();
        Ok(ByteClosure {
            fields,
            delivered,
            logical_bytes,
            delivered_bytes,
        })
    }
}

/// The frames of `group` whose decoded bytes intersect `decoded`, in order. A
/// group of one is returned whole, so an unchunked frame behaves as before.
pub fn frame_chunks(group: &[FrameLayout], decoded: Span) -> impl Iterator<Item = &FrameLayout> {
    let whole = group.len() == 1;
    let mut start = 0;
    group.iter().filter(move |frame| {
        let end = start + frame.decoded_length;
        let hit = whole
            || if decoded.is_empty() {
                start <= decoded.start && decoded.start < end
            } else {
                start < decoded.end && decoded.start < end
            };
        start = end;
        hit
    })
}

fn apply_dependency(
    rule: &DependencyRule,
    fields: &mut AccessSet,
    layouts: &[FieldLayout],
) -> Result<bool, String> {
    match *rule {
        DependencyRule::DependentField {
            source,
            prerequisite,
        } => {
            if fields.spans(source).is_empty() {
                return Ok(false);
            }
            let length = layouts
                .get(prerequisite.0)
                .ok_or("dependent prerequisite field is absent")?
                .length;
            Ok(fields.insert(prerequisite, Span::new(0, length)?))
        }
        DependencyRule::IndexedStream {
            data,
            index,
            data_block_bytes,
            index_entry_bytes,
        } => {
            let spans = fields.spans(data).to_vec();
            let mut changed = false;
            for span in spans {
                let first = span.start / data_block_bytes;
                let end = span.end.div_ceil(data_block_bytes);
                changed |= fields.insert(
                    index,
                    Span::new(first * index_entry_bytes, end * index_entry_bytes)?,
                );
            }
            Ok(changed)
        }
        DependencyRule::Restart {
            data,
            restarts,
            data_bytes_per_restart,
            bytes_per_restart,
        } => {
            let spans = fields.spans(data).to_vec();
            let mut changed = false;
            for span in spans {
                let first = span.start / data_bytes_per_restart;
                let end = span.end.div_ceil(data_bytes_per_restart);
                changed |=
                    fields.insert(data, Span::new(first * data_bytes_per_restart, span.end)?);
                changed |= fields.insert(
                    restarts,
                    Span::new(first * bytes_per_restart, end * bytes_per_restart)?,
                );
            }
            Ok(changed)
        }
    }
}

fn merge_spans(spans: &mut Vec<Span>) {
    spans.sort_unstable();
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        if let Some(last) = merged.last_mut()
            && span.start <= last.end
        {
            last.end = last.end.max(span.end);
        } else {
            merged.push(span);
        }
    }
    *spans = merged;
}
