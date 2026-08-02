use super::{FieldId, NodeId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaCoding {
    ZigZag,
    Unsigned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecoderNode {
    BitUnpack {
        stream: FieldId,
        width: u8,
        len: usize,
        miniblock_rows: usize,
        miniblock_bytes: usize,
    },
    Rle {
        values: NodeId,
        run_lengths: FieldId,
        run_index: FieldId,
        len: usize,
        runs: usize,
        index_interval: usize,
    },
    Dictionary {
        ids: NodeId,
        dictionary: FieldId,
        entries: usize,
        sorted_unique: bool,
    },
    Delta {
        deltas: NodeId,
        restarts: FieldId,
        restart_interval: usize,
        len: usize,
        coding: DeltaCoding,
    },
    For {
        base: FieldId,
        values: NodeId,
    },
    Patch {
        values: NodeId,
        positions: FieldId,
        position_index: FieldId,
        exceptions: FieldId,
        count: usize,
        index_interval: usize,
    },
    Nullable {
        validity: FieldId,
        rank_index: FieldId,
        values: NodeId,
        logical_len: usize,
        nonnull_len: usize,
        rank_interval: usize,
    },
}

impl DecoderNode {
    pub fn len(&self, nodes: &[DecoderNode]) -> Result<usize, String> {
        match self {
            Self::BitUnpack { len, .. }
            | Self::Rle { len, .. }
            | Self::Delta { len, .. }
            | Self::Nullable {
                logical_len: len, ..
            } => Ok(*len),
            Self::Dictionary { ids, .. } => child_len(nodes, *ids),
            Self::For { values, .. } | Self::Patch { values, .. } => child_len(nodes, *values),
        }
    }

    pub fn children(&self) -> impl Iterator<Item = NodeId> {
        let child = match self {
            Self::BitUnpack { .. } => None,
            Self::Rle { values, .. } => Some(*values),
            Self::Dictionary { ids, .. } => Some(*ids),
            Self::Delta { deltas, .. } => Some(*deltas),
            Self::For { values, .. } | Self::Patch { values, .. } => Some(*values),
            Self::Nullable { values, .. } => Some(*values),
        };
        child.into_iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecoderIr {
    nodes: Vec<DecoderNode>,
    root: NodeId,
}

impl DecoderIr {
    pub fn new(nodes: Vec<DecoderNode>, root: NodeId) -> Result<Self, String> {
        if nodes.get(root.0).is_none() {
            return Err("decoder root is outside node arena".into());
        }
        for (index, node) in nodes.iter().enumerate() {
            for child in node.children() {
                if child.0 >= index {
                    return Err(format!(
                        "decoder node {index} has non-topological child {}",
                        child.0
                    ));
                }
            }
            validate_node(index, node, &nodes)?;
        }
        Ok(Self { nodes, root })
    }

    pub fn nodes(&self) -> &[DecoderNode] {
        &self.nodes
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn node(&self, id: NodeId) -> Result<&DecoderNode, String> {
        self.nodes
            .get(id.0)
            .ok_or_else(|| format!("decoder node {} is absent", id.0))
    }

    pub fn len(&self) -> Result<usize, String> {
        self.node(self.root)?.len(&self.nodes)
    }

    pub fn is_empty(&self) -> Result<bool, String> {
        self.len().map(|len| len == 0)
    }
}

fn child_len(nodes: &[DecoderNode], id: NodeId) -> Result<usize, String> {
    nodes
        .get(id.0)
        .ok_or_else(|| format!("decoder child {} is absent", id.0))?
        .len(nodes)
}

fn validate_node(index: usize, node: &DecoderNode, nodes: &[DecoderNode]) -> Result<(), String> {
    match node {
        DecoderNode::BitUnpack {
            width,
            miniblock_rows,
            miniblock_bytes,
            ..
        } if *width > 64 || *miniblock_rows == 0 || *miniblock_bytes == 0 => {
            Err(format!("invalid BitUnpack parameters at node {index}"))
        }
        DecoderNode::Rle {
            values,
            runs,
            index_interval,
            ..
        } if *runs == 0 || *index_interval == 0 || child_len(nodes, *values)? != *runs => {
            Err(format!("invalid RLE parameters at node {index}"))
        }
        DecoderNode::Dictionary { ids, entries, .. }
            if *entries == 0 || child_len(nodes, *ids)? == 0 =>
        {
            Err(format!("invalid Dictionary parameters at node {index}"))
        }
        DecoderNode::Delta {
            deltas,
            restart_interval,
            len,
            ..
        } if *restart_interval == 0 || child_len(nodes, *deltas)? != *len => {
            Err(format!("invalid Delta parameters at node {index}"))
        }
        DecoderNode::For { values, .. } if child_len(nodes, *values)? == 0 => {
            Err(format!("invalid FOR parameters at node {index}"))
        }
        DecoderNode::Patch {
            values,
            count,
            index_interval,
            ..
        } if *count > child_len(nodes, *values)? || *index_interval == 0 => {
            Err(format!("invalid Patch parameters at node {index}"))
        }
        DecoderNode::Nullable {
            values,
            logical_len,
            nonnull_len,
            rank_interval,
            ..
        } if *logical_len == 0
            || *nonnull_len > *logical_len
            || *rank_interval == 0
            || child_len(nodes, *values)? != *nonnull_len =>
        {
            Err(format!("invalid Nullable parameters at node {index}"))
        }
        _ => Ok(()),
    }
}
