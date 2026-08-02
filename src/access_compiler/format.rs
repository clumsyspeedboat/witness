use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::{
    DependencyRule, FieldId, FieldLayout, FieldLocation, FrameCodec, FrameId, FrameLayout,
    LayoutIr, NullPlacement,
};

const MAGIC: &[u8; 8] = b"ACPAGE01";
const VERSION: u32 = 3;
const LEGACY_VERSION: u32 = 2;
const LEGACY_HEADER_BASE: usize = 32;
const HEADER_BASE: usize = 48;
const FLAGS_AT: usize = 32;
const CHECKSUM_AT: usize = 40;
const NON_DECREASING: u32 = 1;
const NON_DECREASING_NON_NULL: u32 = 1 << 1;
const HAS_NULLS: u32 = 1 << 2;
const NULLS_FIRST: u32 = 1 << 3;
const NULLS_LAST: u32 = 1 << 4;
const NULL_INFO_KNOWN: u32 = 1 << 5;
const KNOWN_FLAGS: u32 = NON_DECREASING
    | NON_DECREASING_NON_NULL
    | HAS_NULLS
    | NULLS_FIRST
    | NULLS_LAST
    | NULL_INFO_KNOWN;
const FIELD_DESC: usize = 48;
const FRAME_DESC: usize = 32;
const DEPENDENCY_DESC: usize = 32;
const PAGE_ALIGNMENT: usize = 64;

#[derive(Clone, Debug)]
pub struct SerializedPage {
    bytes: Vec<u8>,
    layout: LayoutIr,
    invariants: CheckedInvariants,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckedInvariants {
    pub non_decreasing: bool,
    pub non_decreasing_non_null: bool,
    pub null_placement: NullPlacement,
}

impl Default for CheckedInvariants {
    fn default() -> Self {
        Self {
            non_decreasing: false,
            non_decreasing_non_null: false,
            null_placement: NullPlacement::Arbitrary,
        }
    }
}

impl SerializedPage {
    pub fn parse(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() < LEGACY_HEADER_BASE || bytes.get(..8) != Some(MAGIC) {
            return Err("invalid access-page magic or truncated header".into());
        }
        let version = read_u32(&bytes, 8)?;
        let header_base = match version {
            LEGACY_VERSION => LEGACY_HEADER_BASE,
            VERSION => HEADER_BASE,
            _ => return Err("unsupported access-page version".into()),
        };
        if bytes.len() < header_base {
            return Err("access-page base header is truncated".into());
        }
        let field_count = read_u32(&bytes, 12)? as usize;
        let frame_count = read_u32(&bytes, 16)? as usize;
        let dependency_count = read_u32(&bytes, 20)? as usize;
        let file_length = to_usize(read_u64(&bytes, 24)?)?;
        if file_length != bytes.len() || field_count == 0 {
            return Err("access-page length or field count is invalid".into());
        }
        let header_length = header_length(header_base, field_count, frame_count, dependency_count)?;
        if header_length > bytes.len() {
            return Err("access-page directory is truncated".into());
        }
        let invariants = if version == VERSION {
            let flags = read_u32(&bytes, FLAGS_AT)?;
            if flags & !KNOWN_FLAGS != 0 {
                return Err("access-page descriptor has unknown invariant flags".into());
            }
            if read_u64(&bytes, CHECKSUM_AT)? != descriptor_checksum(&bytes[..header_length]) {
                return Err("access-page descriptor checksum mismatch".into());
            }
            let invariants = CheckedInvariants {
                non_decreasing: flags & NON_DECREASING != 0,
                non_decreasing_non_null: flags & NON_DECREASING_NON_NULL != 0
                    || flags & NON_DECREASING != 0,
                null_placement: if flags & NULL_INFO_KNOWN != 0 {
                    decode_null_placement(flags)?
                } else if flags & NON_DECREASING != 0 {
                    NullPlacement::NoNulls
                } else {
                    NullPlacement::Arbitrary
                },
            };
            validate_invariants(invariants)?;
            invariants
        } else {
            CheckedInvariants::default()
        };
        let mut fields = Vec::with_capacity(field_count);
        for index in 0..field_count {
            let at = header_base + index * FIELD_DESC;
            let id = read_u32(&bytes, at)? as usize;
            if id != index {
                return Err("access-page field ids are not canonical".into());
            }
            let location_id = read_u32(&bytes, at + 8)? as usize;
            let length = to_usize(read_u64(&bytes, at + 16)?)?;
            let location_offset = to_usize(read_u64(&bytes, at + 24)?)?;
            let location = match bytes[at + 4] {
                0 => FieldLocation::Direct {
                    offset: location_offset,
                },
                1 => FieldLocation::Framed {
                    frame: FrameId(location_id),
                    decoded_offset: location_offset,
                },
                kind => return Err(format!("invalid field location kind {kind}")),
            };
            fields.push(FieldLayout {
                id: FieldId(id),
                name: if index == 0 {
                    "metadata".into()
                } else {
                    format!("field_{index}")
                },
                length,
                alignment: read_u32(&bytes, at + 32)? as usize,
                read_granularity: read_u32(&bytes, at + 36)? as usize,
                location,
            });
        }
        let mut frames = Vec::with_capacity(frame_count);
        let frames_at = header_base + field_count * FIELD_DESC;
        for index in 0..frame_count {
            let at = frames_at + index * FRAME_DESC;
            if read_u32(&bytes, at)? as usize != index || bytes[at + 4] != 1 {
                return Err("invalid frame id or codec".into());
            }
            frames.push(FrameLayout {
                id: FrameId(index),
                codec: FrameCodec::Zstd,
                offset: to_usize(read_u64(&bytes, at + 8)?)?,
                compressed_length: to_usize(read_u64(&bytes, at + 16)?)?,
                decoded_length: to_usize(read_u64(&bytes, at + 24)?)?,
                alignment: PAGE_ALIGNMENT,
            });
        }
        let dependencies_at = frames_at + frame_count * FRAME_DESC;
        let mut dependencies = Vec::with_capacity(dependency_count);
        for index in 0..dependency_count {
            let at = dependencies_at + index * DEPENDENCY_DESC;
            let source = FieldId(read_u32(&bytes, at + 4)? as usize);
            let prerequisite = FieldId(read_u32(&bytes, at + 8)? as usize);
            let first = to_usize(read_u64(&bytes, at + 16)?)?;
            let second = to_usize(read_u64(&bytes, at + 24)?)?;
            dependencies.push(match bytes[at] {
                0 => DependencyRule::DependentField {
                    source,
                    prerequisite,
                },
                1 => DependencyRule::IndexedStream {
                    data: source,
                    index: prerequisite,
                    data_block_bytes: first,
                    index_entry_bytes: second,
                },
                2 => DependencyRule::Restart {
                    data: source,
                    restarts: prerequisite,
                    data_bytes_per_restart: first,
                    bytes_per_restart: second,
                },
                kind => return Err(format!("invalid dependency kind {kind}")),
            });
        }
        let layout = LayoutIr {
            metadata: FieldId(0),
            fields,
            frames,
            dependencies,
            file_length,
        };
        layout.validate()?;
        if layout.fields[0].length != header_length
            || layout.fields[0].location != (FieldLocation::Direct { offset: 0 })
        {
            return Err("metadata field does not describe the complete header".into());
        }
        Ok(Self {
            bytes,
            layout,
            invariants,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn layout(&self) -> &LayoutIr {
        &self.layout
    }

    pub fn invariants(&self) -> CheckedInvariants {
        self.invariants
    }

    pub fn write(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        fs::write(path, &self.bytes)
    }
}

#[derive(Clone, Debug)]
struct FieldSpec {
    name: String,
    bytes: Vec<u8>,
    alignment: usize,
    read_granularity: usize,
    frame_group: Option<usize>,
}

#[derive(Default)]
pub(crate) struct PageBuilder {
    fields: Vec<FieldSpec>,
    dependencies: Vec<DependencyRule>,
    next_frame_group: usize,
    invariants: CheckedInvariants,
}

impl PageBuilder {
    pub(crate) fn new(invariants: CheckedInvariants) -> Self {
        Self {
            fields: vec![FieldSpec {
                name: "metadata".into(),
                bytes: Vec::new(),
                alignment: PAGE_ALIGNMENT,
                read_granularity: PAGE_ALIGNMENT,
                frame_group: None,
            }],
            dependencies: Vec::new(),
            next_frame_group: 0,
            invariants,
        }
    }

    pub(crate) fn field_count(&self) -> usize {
        self.fields.len()
    }

    pub(crate) fn add_field(
        &mut self,
        name: impl Into<String>,
        bytes: Vec<u8>,
        alignment: usize,
        read_granularity: usize,
    ) -> Result<FieldId, String> {
        if alignment == 0 || read_granularity == 0 {
            return Err("field alignment and granularity must be positive".into());
        }
        let id = FieldId(self.fields.len());
        self.fields.push(FieldSpec {
            name: name.into(),
            bytes,
            alignment,
            read_granularity,
            frame_group: None,
        });
        self.dependencies.push(DependencyRule::DependentField {
            source: id,
            prerequisite: FieldId(0),
        });
        Ok(id)
    }

    pub(crate) fn frame_fields(&mut self, start: usize, end: usize) -> Result<(), String> {
        if start == 0 || start >= end || end > self.fields.len() {
            return Err("invalid frame field range".into());
        }
        let group = self.next_frame_group;
        self.next_frame_group += 1;
        for field in &mut self.fields[start..end] {
            if field.frame_group.replace(group).is_some() {
                return Err("field belongs to more than one frame".into());
            }
        }
        Ok(())
    }

    pub(crate) fn add_dependency(&mut self, rule: DependencyRule) {
        self.dependencies.push(rule);
    }

    pub(crate) fn finish(mut self) -> Result<SerializedPage, String> {
        let mut groups: BTreeMap<usize, Vec<FieldId>> = BTreeMap::new();
        for (index, field) in self.fields.iter().enumerate().skip(1) {
            if let Some(group) = field.frame_group {
                groups.entry(group).or_default().push(FieldId(index));
            }
        }
        let field_count = self.fields.len();
        let frame_count = groups.len();
        let header_length = header_length(
            HEADER_BASE,
            field_count,
            frame_count,
            self.dependencies.len(),
        )?;
        self.fields[0].bytes.resize(header_length, 0);

        let mut field_locations = vec![None; field_count];
        field_locations[0] = Some(FieldLocation::Direct { offset: 0 });
        let mut cursor = header_length;
        for (index, field) in self.fields.iter().enumerate().skip(1) {
            if field.frame_group.is_none() {
                cursor = align_up(cursor, field.alignment)?;
                field_locations[index] = Some(FieldLocation::Direct { offset: cursor });
                cursor = cursor
                    .checked_add(field.bytes.len())
                    .ok_or("direct field offset overflow")?;
            }
        }

        let mut compressed_frames = Vec::new();
        let mut frames = Vec::new();
        for (frame_index, field_ids) in groups.values().enumerate() {
            let mut decoded = Vec::new();
            for field_id in field_ids {
                let field = &self.fields[field_id.0];
                let decoded_offset = align_up(decoded.len(), field.alignment)?;
                decoded.resize(decoded_offset, 0);
                field_locations[field_id.0] = Some(FieldLocation::Framed {
                    frame: FrameId(frame_index),
                    decoded_offset,
                });
                decoded.extend_from_slice(&field.bytes);
            }
            let compressed = zstd::bulk::compress(&decoded, 3)
                .map_err(|error| format!("Zstd frame compression failed: {error}"))?;
            cursor = align_up(cursor, PAGE_ALIGNMENT)?;
            frames.push(FrameLayout {
                id: FrameId(frame_index),
                codec: FrameCodec::Zstd,
                offset: cursor,
                compressed_length: compressed.len(),
                decoded_length: decoded.len(),
                alignment: PAGE_ALIGNMENT,
            });
            cursor = cursor
                .checked_add(compressed.len())
                .ok_or("frame offset overflow")?;
            compressed_frames.push(compressed);
        }

        let mut bytes = vec![0_u8; cursor];
        for (index, field) in self.fields.iter().enumerate().skip(1) {
            if let Some(FieldLocation::Direct { offset }) = field_locations[index] {
                bytes[offset..offset + field.bytes.len()].copy_from_slice(&field.bytes);
            }
        }
        for (frame, compressed) in frames.iter().zip(&compressed_frames) {
            bytes[frame.offset..frame.offset + compressed.len()].copy_from_slice(compressed);
        }

        let fields = self
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| FieldLayout {
                id: FieldId(index),
                name: field.name.clone(),
                length: field.bytes.len(),
                alignment: field.alignment,
                read_granularity: field.read_granularity,
                location: field_locations[index].clone().unwrap(),
            })
            .collect::<Vec<_>>();
        let layout = LayoutIr {
            metadata: FieldId(0),
            fields,
            frames,
            dependencies: self.dependencies,
            file_length: bytes.len(),
        };
        layout.validate()?;
        write_header(&mut bytes, &layout, self.invariants)?;
        let parsed = SerializedPage::parse(bytes.clone())?;
        Ok(SerializedPage {
            bytes,
            layout,
            invariants: parsed.invariants,
        })
    }
}

fn write_header(
    bytes: &mut [u8],
    layout: &LayoutIr,
    invariants: CheckedInvariants,
) -> Result<(), String> {
    bytes[..8].copy_from_slice(MAGIC);
    put_u32(bytes, 8, VERSION)?;
    put_u32(bytes, 12, layout.fields.len() as u32)?;
    put_u32(bytes, 16, layout.frames.len() as u32)?;
    put_u32(bytes, 20, layout.dependencies.len() as u32)?;
    put_u64(bytes, 24, layout.file_length as u64)?;
    validate_invariants(invariants)?;
    let mut flags = NULL_INFO_KNOWN;
    flags |= u32::from(invariants.non_decreasing) * NON_DECREASING;
    flags |= u32::from(invariants.non_decreasing_non_null) * NON_DECREASING_NON_NULL;
    flags |= match invariants.null_placement {
        NullPlacement::NoNulls => 0,
        NullPlacement::First => HAS_NULLS | NULLS_FIRST,
        NullPlacement::Last => HAS_NULLS | NULLS_LAST,
        NullPlacement::Arbitrary => HAS_NULLS,
    };
    put_u32(bytes, FLAGS_AT, flags)?;
    put_u64(bytes, CHECKSUM_AT, 0)?;
    for field in &layout.fields {
        let at = HEADER_BASE + field.id.0 * FIELD_DESC;
        put_u32(bytes, at, field.id.0 as u32)?;
        let (kind, location_id, location_offset) = match field.location {
            FieldLocation::Direct { offset } => (0, 0, offset),
            FieldLocation::Framed {
                frame,
                decoded_offset,
            } => (1, frame.0, decoded_offset),
        };
        bytes[at + 4] = kind;
        put_u32(bytes, at + 8, location_id as u32)?;
        put_u64(bytes, at + 16, field.length as u64)?;
        put_u64(bytes, at + 24, location_offset as u64)?;
        put_u32(bytes, at + 32, field.alignment as u32)?;
        put_u32(bytes, at + 36, field.read_granularity as u32)?;
    }
    let frames_at = HEADER_BASE + layout.fields.len() * FIELD_DESC;
    for frame in &layout.frames {
        let at = frames_at + frame.id.0 * FRAME_DESC;
        put_u32(bytes, at, frame.id.0 as u32)?;
        bytes[at + 4] = 1;
        put_u64(bytes, at + 8, frame.offset as u64)?;
        put_u64(bytes, at + 16, frame.compressed_length as u64)?;
        put_u64(bytes, at + 24, frame.decoded_length as u64)?;
    }
    let dependencies_at = frames_at + layout.frames.len() * FRAME_DESC;
    for (index, dependency) in layout.dependencies.iter().enumerate() {
        let at = dependencies_at + index * DEPENDENCY_DESC;
        let (kind, source, prerequisite, first, second) = match *dependency {
            DependencyRule::DependentField {
                source,
                prerequisite,
            } => (0, source, prerequisite, 0, 0),
            DependencyRule::IndexedStream {
                data,
                index,
                data_block_bytes,
                index_entry_bytes,
            } => (1, data, index, data_block_bytes, index_entry_bytes),
            DependencyRule::Restart {
                data,
                restarts,
                data_bytes_per_restart,
                bytes_per_restart,
            } => (2, data, restarts, data_bytes_per_restart, bytes_per_restart),
        };
        bytes[at] = kind;
        put_u32(bytes, at + 4, source.0 as u32)?;
        put_u32(bytes, at + 8, prerequisite.0 as u32)?;
        put_u64(bytes, at + 16, first as u64)?;
        put_u64(bytes, at + 24, second as u64)?;
    }
    let header_length = layout.fields[layout.metadata.0].length;
    let checksum = descriptor_checksum(&bytes[..header_length]);
    put_u64(bytes, CHECKSUM_AT, checksum)?;
    Ok(())
}

fn header_length(
    header_base: usize,
    fields: usize,
    frames: usize,
    dependencies: usize,
) -> Result<usize, String> {
    let raw = header_base
        .checked_add(
            fields
                .checked_mul(FIELD_DESC)
                .ok_or("field header overflow")?,
        )
        .and_then(|size| size.checked_add(frames.checked_mul(FRAME_DESC)?))
        .and_then(|size| size.checked_add(dependencies.checked_mul(DEPENDENCY_DESC)?))
        .ok_or("access-page header overflow")?;
    align_up(raw, PAGE_ALIGNMENT)
}

fn descriptor_checksum(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .enumerate()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, (offset, byte)| {
            let byte = if (CHECKSUM_AT..CHECKSUM_AT + 8).contains(&offset) {
                0
            } else {
                *byte
            };
            (hash ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3)
        })
}

fn align_up(value: usize, alignment: usize) -> Result<usize, String> {
    value
        .checked_add(alignment - 1)
        .map(|value| value / alignment * alignment)
        .ok_or("alignment overflow".into())
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let end = at.checked_add(4).ok_or("u32 offset overflow")?;
    Ok(u32::from_le_bytes(
        bytes
            .get(at..end)
            .ok_or("truncated u32")?
            .try_into()
            .unwrap(),
    ))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, String> {
    let end = at.checked_add(8).ok_or("u64 offset overflow")?;
    Ok(u64::from_le_bytes(
        bytes
            .get(at..end)
            .ok_or("truncated u64")?
            .try_into()
            .unwrap(),
    ))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) -> Result<(), String> {
    let end = at.checked_add(4).ok_or("u32 offset overflow")?;
    bytes
        .get_mut(at..end)
        .ok_or("truncated u32 destination")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) -> Result<(), String> {
    let end = at.checked_add(8).ok_or("u64 offset overflow")?;
    bytes
        .get_mut(at..end)
        .ok_or("truncated u64 destination")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn to_usize(value: u64) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| "header value does not fit usize".into())
}

fn decode_null_placement(flags: u32) -> Result<NullPlacement, String> {
    match (
        flags & HAS_NULLS != 0,
        flags & NULLS_FIRST != 0,
        flags & NULLS_LAST != 0,
    ) {
        (false, false, false) => Ok(NullPlacement::NoNulls),
        (true, true, false) => Ok(NullPlacement::First),
        (true, false, true) => Ok(NullPlacement::Last),
        (true, false, false) => Ok(NullPlacement::Arbitrary),
        _ => Err("access-page descriptor has inconsistent null-placement flags".into()),
    }
}

fn validate_invariants(invariants: CheckedInvariants) -> Result<(), String> {
    if invariants.non_decreasing
        && (!invariants.non_decreasing_non_null
            || invariants.null_placement != NullPlacement::NoNulls)
    {
        return Err("full monotonicity requires dense non-null values".into());
    }
    Ok(())
}
