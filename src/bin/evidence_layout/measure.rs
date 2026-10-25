use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

use witness::access_compiler::paged::{self, PagePolicy, PagedColumn, Planning};
use witness::access_compiler::{ClosureMode, ReadSession, primitive_rule_fingerprint};
use witness::experiment::evidence_layout::{Compression, compression_menu};
use witness::experiment::types::Column;

use crate::common::{PAGE_ROWS, compact_menu, csv, direct_menu, quote, truth};
use crate::corpus::Pair;
use crate::{corpus, workload};

const LAYOUTS: [&str; 6] = [
    "direct",
    "compact",
    "whole",
    "whole_4k",
    "payload",
    "payload_4k",
];
const PLANNERS: [Planning; 4] = [
    Planning::Facts,
    Planning::ScanOnly,
    Planning::StoredStatistics,
    Planning::StoredStatisticsPlus,
];
const ROUNDS: usize = 9;
const TARGET_NS: u128 = 2_000_000;

struct Layout {
    selector: PagedColumn,
    value: PagedColumn,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let smoke = arguments.iter().any(|arg| arg == "--smoke");
    let all = arguments.iter().any(|arg| arg == "--all");
    if arguments
        .iter()
        .any(|arg| !["--smoke", "--all"].contains(&arg.as_str()))
        || (smoke && all)
    {
        return Err("usage: evidence_layout_study [--smoke | --all]".into());
    }
    if std::env::var_os("WITNESS_PAGED_MAX_PAIRS").is_some() {
        return Err(
            "unset WITNESS_PAGED_MAX_PAIRS: this study freezes its own source split".into(),
        );
    }
    let result_dir = std::env::var("WITNESS_EVIDENCE_LAYOUT_RESULT_DIR")
        .map_err(|_| "set WITNESS_EVIDENCE_LAYOUT_RESULT_DIR to a new diagnostic directory")?;
    fs::create_dir_all(&result_dir)?;
    let mut freeze = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(format!("{result_dir}/freeze.csv"))?;
    writeln!(
        freeze,
        "key,value\nstatus,started\nrule_fingerprint,{:016x}\npage_rows,{PAGE_ROWS}\nrounds,{ROUNDS}\nchunk_bytes,4096\nsmoke,{smoke}\nall_pairs,{all}\npartition,fnv1a_group_source_mod3_zero_is_development\nselection,8_development_16_heldout_sources_one_pair_each\nbyte_ratio_threshold,0.80\nlatency_ratio_threshold,1.05",
        primitive_rule_fingerprint()
    )?;
    freeze.flush()?;
    let started = Instant::now();
    let (columns, pairs) = if smoke { synthetic() } else { corpus::pairs()? };
    let selected = if smoke || all {
        (0..pairs.len()).collect()
    } else {
        select_pairs(&pairs)
    };
    let sources = selected
        .iter()
        .map(|&id| (&pairs[id].group, &pairs[id].source))
        .collect::<BTreeSet<_>>();
    eprintln!(
        "evidence layout: {} pairs from {} sources, loaded in {:.1?}",
        selected.len(),
        sources.len(),
        started.elapsed()
    );
    let base = direct_menu();
    let policies = [
        PagePolicy::smallest(base.clone()),
        PagePolicy::smallest(compact_menu()),
        PagePolicy::smallest(compression_menu(&base, Compression::Whole, false)?),
        PagePolicy::smallest(compression_menu(
            &base,
            Compression::Chunked { bytes: 4_096 },
            false,
        )?),
        PagePolicy::smallest(compression_menu(&base, Compression::Whole, true)?),
        PagePolicy::smallest(compression_menu(
            &base,
            Compression::Chunked { bytes: 4_096 },
            true,
        )?),
    ];
    let mut sizes = csv(format!("{result_dir}/layouts.csv"))?;
    writeln!(
        sizes,
        "pair,split,group,source,selector,value,rows,pages,layout,selector_bytes,value_bytes,bytes,encode_ns,first_query_ns,first_query_predicate,plan_frames_decoded,plan_delivered_bytes,selector_recipes,value_recipes"
    )?;
    let mut queries = csv(format!("{result_dir}/queries.csv"))?;
    writeln!(
        queries,
        "pair,split,group,source,predicate,requested,matched_rows,expected_sum,layout,planner,p25_ns,median_ns,p75_ns,skipped,accepted,searched,translated,scanned"
    )?;
    for (slot, &pair_id) in selected.iter().enumerate() {
        let pair = &pairs[pair_id];
        let (selector, value) = (&columns[pair.selector], &columns[pair.value]);
        let predicates = workload::predicates(&selector.values)?;
        let split = if smoke { "smoke" } else { partition(pair) };
        let mut layouts = Vec::new();
        for (index, policy) in policies.iter().enumerate() {
            let encoded = Instant::now();
            let layout = Layout {
                selector: PagedColumn::from_values(&selector.values, PAGE_ROWS, policy)?,
                value: PagedColumn::from_values(&value.values, PAGE_ROWS, policy)?,
            };
            let encode_ns = encoded.elapsed().as_nanos();
            // The first query includes the reader's initially uncached
            // dictionary-order checks. Sealing/encoding is reported separately.
            let first = &predicates[0];
            let expected = truth(&selector.values, &value.values, first.low, first.high);
            let clock = Instant::now();
            let (sum, matched, _) = paged::sum_where(
                &layout.selector,
                &layout.value,
                first.low,
                first.high,
                Planning::Facts,
                1,
            )?;
            let first_ns = clock.elapsed().as_nanos();
            if (sum, matched) != expected {
                return Err("first-query answer mismatch".into());
            }
            // Instrumented mechanism count, independently planning every page;
            // this is not a count of the complete query's physical reads.
            let (mut frames, mut delivered) = (0, 0);
            for page in layout.selector.pages() {
                let mut session = ReadSession::new(&page.column.page, ClosureMode::Selective);
                paged::plan_between(page, &mut session, first.low, first.high, Planning::Facts)?;
                frames += session.metrics().frames_decoded;
                delivered += session.metrics().delivered_bytes;
            }
            writeln!(
                sizes,
                "{pair_id},{split},{},{},{},{},{},{},{},{},{},{},{encode_ns},{first_ns},{},{frames},{delivered},{},{}",
                quote(&pair.group),
                quote(&pair.source),
                quote(&selector.name),
                quote(&value.name),
                selector.values.len(),
                layout.selector.pages().len(),
                LAYOUTS[index],
                layout.selector.bytes(),
                layout.value.bytes(),
                layout.selector.bytes() + layout.value.bytes(),
                quote(&first.label),
                quote(&recipes(&layout.selector)),
                quote(&recipes(&layout.value))
            )?;
            layouts.push(layout);
        }
        for predicate in predicates {
            let expected = truth(
                &selector.values,
                &value.values,
                predicate.low,
                predicate.high,
            );
            let run = |arm: usize| {
                let layout = &layouts[arm / PLANNERS.len()];
                paged::sum_where(
                    &layout.selector,
                    &layout.value,
                    predicate.low,
                    predicate.high,
                    PLANNERS[arm % PLANNERS.len()],
                    1,
                )
            };
            let count = LAYOUTS.len() * PLANNERS.len();
            let mut iterations = vec![1; count];
            let mut outcomes = Vec::with_capacity(count);
            for (arm, iterations) in iterations.iter_mut().enumerate() {
                let outcome = run(arm)?;
                if (outcome.0, outcome.1) != expected {
                    return Err(format!("answer mismatch on pair {pair_id}, arm {arm}").into());
                }
                outcomes.push(outcome.2);
                let clock = Instant::now();
                black_box(run(arm)?);
                *iterations =
                    (TARGET_NS / clock.elapsed().as_nanos().max(1)).clamp(1, 1_000) as usize;
            }
            let mut samples = vec![Vec::with_capacity(ROUNDS); count];
            for round in 0..ROUNDS {
                // Rotate arm order to distribute drift across the layouts.
                for offset in 0..count {
                    let arm = (offset + round * 7 + slot) % count;
                    let mut last = None;
                    let clock = Instant::now();
                    for _ in 0..iterations[arm] {
                        last = Some(black_box(run(arm)?));
                    }
                    let elapsed = clock.elapsed().as_nanos() as f64 / iterations[arm] as f64;
                    let (sum, matched, _) = last.unwrap();
                    if (sum, matched) != expected {
                        return Err("timed answer mismatch".into());
                    }
                    samples[arm].push(elapsed);
                }
            }
            for (arm, samples) in samples.iter_mut().enumerate() {
                samples.sort_by(f64::total_cmp);
                let at = |q| samples[(samples.len() - 1) * q / 4];
                let counts = outcomes[arm];
                writeln!(
                    queries,
                    "{pair_id},{split},{},{},{},{},{},{},{},{},{:.1},{:.1},{:.1},{},{},{},{},{}",
                    quote(&pair.group),
                    quote(&pair.source),
                    quote(&predicate.label),
                    quote(&predicate.requested),
                    expected.1,
                    expected.0,
                    LAYOUTS[arm / PLANNERS.len()],
                    PLANNERS[arm % PLANNERS.len()].label(),
                    at(1),
                    at(2),
                    at(3),
                    counts.skipped,
                    counts.accepted,
                    counts.searched,
                    counts.translated,
                    counts.scanned
                )?;
            }
        }
        sizes.flush()?;
        queries.flush()?;
        eprintln!(
            "pair {}/{}: {}/{} ({split}), elapsed {:.1?}",
            slot + 1,
            selected.len(),
            pair.source,
            selector.name,
            started.elapsed()
        );
    }
    writeln!(
        freeze,
        "pairs,{}\nsources,{}\nstatus,complete\nelapsed_seconds,{:.3}",
        selected.len(),
        sources.len(),
        started.elapsed().as_secs_f64()
    )?;
    freeze.flush()?;
    Ok(())
}

fn hash(pair: &Pair) -> u64 {
    pair.group
        .bytes()
        .chain([0])
        .chain(pair.source.bytes())
        .fold(0xcbf2_9ce4_8422_2325, |h, byte| {
            (h ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3)
        })
}

fn partition(pair: &Pair) -> &'static str {
    if hash(pair).is_multiple_of(3) {
        "development"
    } else {
        "heldout"
    }
}

fn select_pairs(pairs: &[Pair]) -> Vec<usize> {
    let mut sources = BTreeMap::<(&str, &str), Vec<usize>>::new();
    for (id, pair) in pairs.iter().enumerate() {
        sources
            .entry((&pair.group, &pair.source))
            .or_default()
            .push(id);
    }
    let mut selected = Vec::new();
    for (split, limit) in [("development", 8), ("heldout", 16)] {
        let eligible = sources
            .values()
            .filter(|ids| partition(&pairs[ids[0]]) == split)
            .collect::<Vec<_>>();
        let count = limit.min(eligible.len());
        for slot in 0..count {
            let ids = eligible[slot * eligible.len() / count];
            selected.push(ids[(hash(&pairs[ids[0]]) as usize / 3) % ids.len()]);
        }
    }
    selected
}

fn recipes(column: &PagedColumn) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for page in column.pages() {
        *counts.entry(page.column.recipe.name()).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(recipe, count)| format!("{recipe}:{count}"))
        .collect::<Vec<_>>()
        .join(";")
}

fn synthetic() -> (Vec<Column>, Vec<Pair>) {
    let mut columns = Vec::new();
    let mut pairs = Vec::new();
    for case in 0..3 {
        let values = (0..PAGE_ROWS * 4 + 217)
            .map(|row| {
                if case == 0 {
                    Some(row as i64 * 3)
                } else if case == 1 {
                    (row % 11 != 0).then_some((row % 127) as i64 * 10)
                } else {
                    Some((row / 128 % 257) as i64 - 400)
                }
            })
            .collect::<Vec<_>>();
        let make = |name: &str, values| Column {
            group: "synthetic".into(),
            source: format!("case{case}"),
            name: name.into(),
            mode: "exact".into(),
            values,
            float_values: None,
            exact_i64: true,
        };
        let selector = columns.len();
        columns.push(make("selector", values));
        columns.push(make(
            "value",
            (0..PAGE_ROWS * 4 + 217)
                .map(|row| (row % 13 != 0).then_some(row as i64 % 997 - 500))
                .collect(),
        ));
        pairs.push(Pair {
            group: "synthetic".into(),
            source: format!("case{case}"),
            selector,
            value: selector + 1,
        });
    }
    (columns, pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_holds_out_whole_sources_and_is_deterministic() {
        let pairs = (0..80)
            .flat_map(|source| {
                (0..4).map(move |selector| Pair {
                    group: "test".into(),
                    source: format!("source{source:02}"),
                    selector,
                    value: 0,
                })
            })
            .collect::<Vec<_>>();
        let selected = select_pairs(&pairs);
        assert_eq!(selected, select_pairs(&pairs));
        assert_eq!(selected.len(), 24);
        assert_eq!(
            selected
                .iter()
                .map(|&id| &pairs[id].source)
                .collect::<BTreeSet<_>>()
                .len(),
            24
        );
        assert_eq!(
            selected
                .iter()
                .filter(|&&id| partition(&pairs[id]) == "development")
                .count(),
            8
        );
        assert_eq!(
            selected
                .iter()
                .filter(|&&id| partition(&pairs[id]) == "heldout")
                .count(),
            16
        );
    }
}
