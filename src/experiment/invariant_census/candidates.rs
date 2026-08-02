use crate::access_compiler::{
    AccessInvariant, ClosureMode, DecoderNode, EncodedColumn, Evidence, FactScope, InputColumn,
    MappingInvariant, Property, ReadSession, Recipe, ValueInvariant, derive_invariants, encode,
};

use super::model::CandidateRow;

pub fn analyze_candidates(
    column: usize,
    values: &[Option<i64>],
) -> Result<Vec<CandidateRow>, String> {
    let nullable = values.iter().any(Option::is_none);
    let mut rows = Vec::new();
    for (label, recipe) in recipes(nullable, values.len()) {
        let Ok(encoded) = encode(&recipe, InputColumn::nullable(values.to_vec())) else {
            continue;
        };
        let facts = derive_invariants(
            &encoded.decoder,
            encoded.page.layout(),
            encoded.page.invariants(),
        )?;
        validate_semantic_facts(&encoded.truth, encoded.decoder.root(), &facts)?;
        validate_mapping_payloads(&encoded)?;
        let root = encoded.decoder.root();
        let structural_monotone = facts.iter().any(|fact| {
            fact.node == root
                && matches!(fact.scope, FactScope::AllRows | FactScope::NonNullRows)
                && fact.property == Property::Value(ValueInvariant::NonDecreasing)
                && matches!(fact.evidence, Evidence::Structural(_))
        });
        let checked_monotone = facts.iter().any(|fact| {
            fact.node == root
                && matches!(fact.scope, FactScope::AllRows | FactScope::NonNullRows)
                && fact.property == Property::Value(ValueInvariant::NonDecreasing)
                && matches!(fact.evidence, Evidence::CheckedDescriptor(_))
        });
        let structural_piecewise_monotone = facts.iter().any(|fact| {
            fact.node == root
                && matches!(
                    fact.property,
                    Property::Value(ValueInvariant::PiecewiseNonDecreasing { .. })
                )
                && matches!(fact.evidence, Evidence::Structural(_))
        });
        let order_preserving_mapping = facts
            .iter()
            .any(|fact| fact.property == Property::Mapping(MappingInvariant::OrderPreserving));
        let framed = facts.iter().any(|fact| {
            matches!(
                fact.property,
                Property::Access(AccessInvariant::EnclosingFrame { .. })
            )
        });
        let restart_bound = facts.iter().find_map(|fact| match fact.property {
            Property::Access(AccessInvariant::RestartBound { rows }) => Some(rows),
            _ => None,
        });
        let access_ready = !framed && restart_bound.is_none_or(|rows| rows <= 256);
        rows.push(CandidateRow {
            column,
            recipe: label,
            bytes: encoded.page.bytes().len(),
            structural_monotone,
            structural_piecewise_monotone,
            checked_monotone,
            order_preserving_mapping,
            framed,
            restart_bound,
            access_ready,
            semantic_facts: facts
                .iter()
                .filter(|fact| matches!(fact.property, Property::Value(_)))
                .count(),
            mapping_facts: facts
                .iter()
                .filter(|fact| matches!(fact.property, Property::Mapping(_)))
                .count(),
            access_facts: facts
                .iter()
                .filter(|fact| matches!(fact.property, Property::Access(_)))
                .count(),
        });
    }
    rows.sort_by(|left, right| {
        left.bytes
            .cmp(&right.bytes)
            .then_with(|| left.recipe.cmp(&right.recipe))
    });
    Ok(rows)
}

fn validate_mapping_payloads(encoded: &EncodedColumn) -> Result<(), String> {
    let mut session = ReadSession::new(&encoded.page, ClosureMode::Selective);
    for node in encoded.decoder.nodes() {
        if let DecoderNode::Dictionary {
            dictionary,
            entries,
            ..
        } = node
        {
            let values = session.read_i64_values(*dictionary, 0, *entries)?;
            if values.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err("dictionary order certificate failed payload validation".into());
            }
        }
    }
    Ok(())
}

fn recipes(nullable: bool, rows: usize) -> Vec<(String, Recipe)> {
    let bitpack = || Recipe::BitPack;
    let whole_restart = rows.next_multiple_of(128);
    let direct = vec![
        ("FOR(BitPack)".into(), Recipe::For(Box::new(bitpack()))),
        (
            "Delta@256(BitPack)".into(),
            Recipe::Delta {
                restart_interval: 256,
                deltas: Box::new(bitpack()),
            },
        ),
        (
            "UnsignedDelta@256(BitPack)".into(),
            Recipe::UnsignedDelta {
                restart_interval: 256,
                deltas: Box::new(bitpack()),
            },
        ),
        (
            format!("UnsignedDelta@{whole_restart}(BitPack)"),
            Recipe::UnsignedDelta {
                restart_interval: whole_restart,
                deltas: Box::new(bitpack()),
            },
        ),
        (
            "Dictionary(BitPack)".into(),
            Recipe::Dictionary(Box::new(bitpack())),
        ),
        (
            "Dictionary(RLE(BitPack))".into(),
            Recipe::Dictionary(Box::new(Recipe::Rle {
                index_interval: 32,
                values: Box::new(bitpack()),
            })),
        ),
        (
            "RLE(FOR(BitPack))".into(),
            Recipe::Rle {
                index_interval: 32,
                values: Box::new(Recipe::For(Box::new(bitpack()))),
            },
        ),
    ];
    direct
        .into_iter()
        .flat_map(|(label, recipe)| {
            let wrap = |candidate| {
                if nullable {
                    Recipe::Nullable {
                        rank_interval: 256,
                        values: Box::new(candidate),
                    }
                } else {
                    candidate
                }
            };
            [
                (label.clone(), wrap(recipe.clone())),
                (
                    format!("Frame({label})"),
                    Recipe::Frame(Box::new(wrap(recipe))),
                ),
            ]
        })
        .collect()
}

fn validate_semantic_facts(
    truth: &[Option<i64>],
    root: crate::access_compiler::NodeId,
    facts: &crate::access_compiler::InvariantSet,
) -> Result<(), String> {
    for fact in facts.iter().filter(|fact| fact.node == root) {
        let sound = match fact.property {
            Property::Value(ValueInvariant::NonDecreasing) => match fact.scope {
                FactScope::AllRows => {
                    truth.iter().all(Option::is_some)
                        && truth.windows(2).all(|pair| pair[0] <= pair[1])
                }
                FactScope::NonNullRows => truth
                    .iter()
                    .flatten()
                    .copied()
                    .collect::<Vec<_>>()
                    .windows(2)
                    .all(|pair| pair[0] <= pair[1]),
                _ => true,
            },
            Property::Value(ValueInvariant::PiecewiseNonDecreasing { max_rows }) => {
                truth.iter().all(Option::is_some)
                    && truth
                        .chunks(max_rows)
                        .all(|chunk| chunk.windows(2).all(|pair| pair[0] <= pair[1]))
            }
            _ => true,
        };
        if !sound {
            return Err(format!("unsound derived fact: {fact:?}"));
        }
    }
    Ok(())
}
