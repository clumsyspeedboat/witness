use std::collections::BTreeMap;
use std::error::Error;

use crate::access_compiler::{EncodedColumn, InputColumn, Recipe, encode};

use super::datasets::{predicate_sources, study_columns};
use super::types::Column;

pub const DEFAULT_REAL_ACCESS_ROWS: usize = 131_072;

pub fn real_access_rows() -> Result<usize, Box<dyn Error>> {
    let rows = std::env::var("WITNESS_MAX_ROWS")
        .map_or(Ok(DEFAULT_REAL_ACCESS_ROWS), |value| value.parse::<usize>())?;
    if rows < 1_024 {
        return Err("WITNESS_MAX_ROWS must be at least 1024".into());
    }
    Ok(rows)
}

#[derive(Clone, Debug)]
pub struct CandidateSize {
    pub recipe: String,
    pub bytes: usize,
}

#[derive(Clone, Debug)]
pub struct RealAccessColumn {
    pub group: String,
    pub source: String,
    pub name: String,
    pub nulls: usize,
    /// Smallest serialized page among the fixed size-selection candidates.
    pub size_selected: EncodedColumn,
    /// Smallest unframed candidate whose dependencies are bounded by indexes.
    pub access_ready: EncodedColumn,
    pub candidates: Vec<CandidateSize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealAccessPair {
    pub class: String,
    pub selector_kind: String,
    pub group: String,
    pub source: String,
    pub selector: usize,
    pub value: usize,
}

pub fn real_access_columns() -> Result<Vec<RealAccessColumn>, Box<dyn Error>> {
    let max_rows = real_access_rows()?;
    let mut output = Vec::new();
    for column in study_columns()? {
        if column.group == "synthetic" || !column.exact_i64 || column.values.len() < 1_024 {
            continue;
        }
        output.push(encode_column(column, max_rows)?);
    }
    output.sort_by(|left, right| {
        (&left.group, &left.source, &left.name).cmp(&(&right.group, &right.source, &right.name))
    });
    if output.len() < 10 {
        return Err(format!("real access corpus is unexpectedly small: {}", output.len()).into());
    }
    Ok(output)
}

pub fn real_access_pairs(columns: &[RealAccessColumn]) -> Vec<RealAccessPair> {
    let mut sources: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    for (index, column) in columns.iter().enumerate() {
        sources
            .entry((&column.group, &column.source))
            .or_default()
            .push(index);
    }
    let mut pairs = Vec::new();
    for ((group, source), indices) in sources {
        if indices.len() < 2 {
            continue;
        }
        let selector = indices
            .iter()
            .copied()
            .find(|&index| columns[index].name == "timestamp")
            .unwrap_or(indices[0]);
        for value in indices.into_iter().filter(|&index| index != selector) {
            if columns[selector].size_selected.truth.len()
                == columns[value].size_selected.truth.len()
            {
                pairs.push(RealAccessPair {
                    class: group.to_string(),
                    selector_kind: selector_kind(&columns[selector].name).to_string(),
                    group: group.to_string(),
                    source: source.to_string(),
                    selector,
                    value,
                });
            }
        }
    }
    pairs
}

pub fn predicate_access_corpus()
-> Result<(Vec<RealAccessColumn>, Vec<RealAccessPair>), Box<dyn Error>> {
    let mut columns = Vec::new();
    let mut pairs = Vec::new();
    let max_rows = real_access_rows()?;
    for source in predicate_sources(max_rows)? {
        let selector = columns.len();
        columns.push(encode_column(source.selector, max_rows)?);
        for value_column in source.values {
            let value = columns.len();
            columns.push(encode_column(value_column, max_rows)?);
            pairs.push(RealAccessPair {
                class: source.class.to_string(),
                selector_kind: source.selector_kind.to_string(),
                group: source.group.clone(),
                source: source.source.clone(),
                selector,
                value,
            });
        }
    }
    if pairs.len() < 30 {
        return Err(format!(
            "predicate corpus is unexpectedly small: {} pairs",
            pairs.len()
        )
        .into());
    }
    Ok((columns, pairs))
}

fn encode_column(mut column: Column, max_rows: usize) -> Result<RealAccessColumn, Box<dyn Error>> {
    column.values.truncate(max_rows);
    let values = column.values;
    let nulls = values.iter().filter(|value| value.is_none()).count();
    let mut candidates = Vec::new();
    let mut winner = None;
    for recipe in size_selection_menu(nulls > 0) {
        let name = recipe.name();
        let Ok(encoded) = encode(&recipe, InputColumn::nullable(values.clone())) else {
            continue;
        };
        candidates.push(CandidateSize {
            recipe: name.clone(),
            bytes: encoded.page.bytes().len(),
        });
        let key = (encoded.page.bytes().len(), name);
        if winner
            .as_ref()
            .is_none_or(|(best, _): &((usize, String), EncodedColumn)| key < *best)
        {
            winner = Some((key, encoded));
        }
    }
    candidates.sort_by(|left, right| {
        left.bytes
            .cmp(&right.bytes)
            .then_with(|| left.recipe.cmp(&right.recipe))
    });
    let access_ready = smallest_encoding(&values, access_ready_menu(nulls > 0))?;
    Ok(RealAccessColumn {
        group: column.group,
        source: column.source,
        name: column.name,
        nulls,
        size_selected: winner.ok_or("real column had no executable encoding")?.1,
        access_ready,
        candidates,
    })
}

fn smallest_encoding(
    values: &[Option<i64>],
    recipes: Vec<Recipe>,
) -> Result<EncodedColumn, Box<dyn Error>> {
    let mut winner = None;
    for recipe in recipes {
        let name = recipe.name();
        let Ok(encoded) = encode(&recipe, InputColumn::nullable(values.to_vec())) else {
            continue;
        };
        let key = (encoded.page.bytes().len(), name);
        if winner
            .as_ref()
            .is_none_or(|(best, _): &((usize, String), EncodedColumn)| key < *best)
        {
            winner = Some((key, encoded));
        }
    }
    winner
        .map(|(_, encoded)| encoded)
        .ok_or_else(|| "column had no access-ready encoding".into())
}

fn selector_kind(name: &str) -> &'static str {
    if name.contains("timestamp") {
        "timestamp"
    } else {
        "identifier"
    }
}

fn size_selection_menu(nullable: bool) -> Vec<Recipe> {
    let bitpack = || Recipe::BitPack;
    let for_bitpack = || Recipe::For(Box::new(bitpack()));
    let delta = || Recipe::Delta {
        restart_interval: 256,
        deltas: Box::new(bitpack()),
    };
    let unsigned_delta = || Recipe::UnsignedDelta {
        restart_interval: 256,
        deltas: Box::new(bitpack()),
    };
    let bases = vec![
        for_bitpack(),
        delta(),
        unsigned_delta(),
        Recipe::For(Box::new(delta())),
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(for_bitpack()),
        },
        Recipe::Dictionary(Box::new(bitpack())),
        Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 32,
            values: Box::new(bitpack()),
        })),
    ];
    let wrap = |recipe: Recipe| {
        if nullable {
            Recipe::Nullable {
                rank_interval: 256,
                values: Box::new(recipe),
            }
        } else {
            recipe
        }
    };
    bases
        .into_iter()
        .flat_map(|recipe| {
            let direct = wrap(recipe.clone());
            let framed = Recipe::Frame(Box::new(wrap(recipe)));
            [direct, framed]
        })
        .collect()
}

fn access_ready_menu(nullable: bool) -> Vec<Recipe> {
    let bitpack = || Recipe::BitPack;
    let for_bitpack = || Recipe::For(Box::new(bitpack()));
    let wrap = |recipe: Recipe| {
        if nullable {
            Recipe::Nullable {
                rank_interval: 256,
                values: Box::new(recipe),
            }
        } else {
            recipe
        }
    };
    vec![
        for_bitpack(),
        Recipe::Delta {
            restart_interval: 32,
            deltas: Box::new(bitpack()),
        },
        Recipe::UnsignedDelta {
            restart_interval: 32,
            deltas: Box::new(bitpack()),
        },
        Recipe::Rle {
            index_interval: 32,
            values: Box::new(for_bitpack()),
        },
        Recipe::Dictionary(Box::new(bitpack())),
        Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 32,
            values: Box::new(bitpack()),
        })),
    ]
    .into_iter()
    .map(wrap)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The corpus is fetched, not vendored (see THIRD_PARTY_DATA.md), so tests
    /// that need it skip on a clone that has not run `make fetch-study`
    /// instead of failing.
    fn corpus_present() -> bool {
        std::path::Path::new("experiments/eval/data/nab").exists()
    }

    #[test]
    fn menu_is_deterministic_and_contains_direct_and_framed_choices() {
        let names = size_selection_menu(false)
            .into_iter()
            .map(|recipe| recipe.name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 14);
        assert!(names.iter().any(|name| name == "FOR(BitPack)"));
        assert!(names.iter().any(|name| name == "UnsignedDelta(BitPack)"));
        assert!(names.iter().any(|name| name == "Frame(FOR(BitPack))"));
    }

    #[test]
    fn selection_policies_match_their_declared_contracts() {
        let column = Column {
            group: "test".into(),
            source: "synthetic".into(),
            name: "value".into(),
            mode: "i64".into(),
            values: (0..4_096)
                .map(|row| (row % 31 != 0).then_some(1_000 + i64::from(row / 4)))
                .collect(),
            float_values: None,
            exact_i64: true,
        };
        let selected = encode_column(column, 4_096).unwrap();
        let first = &selected.candidates[0];

        assert_eq!(selected.size_selected.page.bytes().len(), first.bytes);
        assert_eq!(selected.size_selected.recipe.name(), first.recipe);
        assert!(!matches!(&selected.access_ready.recipe, Recipe::Frame(_)));
        assert!(bounded_access_dependency(&selected.access_ready.recipe));
    }

    #[test]
    fn pairing_prefers_timestamps_and_never_changes_row_domains() {
        if !corpus_present() {
            return;
        }
        let columns = real_access_columns().unwrap();
        let pairs = real_access_pairs(&columns);
        assert!(pairs.len() >= 8);
        for pair in pairs {
            assert_eq!(
                columns[pair.selector].size_selected.truth.len(),
                columns[pair.value].size_selected.truth.len()
            );
            if columns
                .iter()
                .any(|column| column.source == pair.source && column.name == "timestamp")
            {
                assert_eq!(columns[pair.selector].name, "timestamp");
            }
        }
    }

    #[test]
    fn predicate_corpus_has_independent_sources_and_both_fallback_paths() {
        if !corpus_present() {
            return;
        }
        let (columns, pairs) = predicate_access_corpus().unwrap();
        let sources = pairs
            .iter()
            .map(|pair| (&pair.group, &pair.source))
            .collect::<BTreeSet<_>>();
        let classes = pairs
            .iter()
            .map(|pair| pair.class.as_str())
            .collect::<BTreeSet<_>>();
        assert!(sources.len() >= 25);
        assert_eq!(classes.len(), 3);
        assert!(pairs.iter().any(|pair| pair.selector_kind == "identifier"));
        assert!(pairs.iter().any(|pair| {
            columns[pair.selector]
                .size_selected
                .page
                .invariants()
                .non_decreasing
        }));
        assert!(pairs.iter().any(|pair| {
            !columns[pair.selector]
                .size_selected
                .page
                .invariants()
                .non_decreasing
        }));
        assert!(
            columns
                .iter()
                .all(|column| !matches!(&column.access_ready.recipe, Recipe::Frame(_)))
        );
        assert!(
            columns
                .iter()
                .all(|column| bounded_access_dependency(&column.access_ready.recipe))
        );
    }

    fn bounded_access_dependency(recipe: &Recipe) -> bool {
        match recipe {
            Recipe::Delta {
                restart_interval,
                deltas,
            }
            | Recipe::UnsignedDelta {
                restart_interval,
                deltas,
            } => *restart_interval <= 32 && bounded_access_dependency(deltas),
            Recipe::For(values)
            | Recipe::Dictionary(values)
            | Recipe::Frame(values)
            | Recipe::Patch { values, .. }
            | Recipe::Nullable { values, .. }
            | Recipe::Rle { values, .. } => bounded_access_dependency(values),
            Recipe::BitPack => true,
        }
    }
}
