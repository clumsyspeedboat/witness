//! A JSON bridge onto the real Witness pipeline, for interactive exploration.
//!
//! This binary exists so a notebook, a shell, or any other tool can drive the
//! *actual* encoder, invariant calculus, compiler, and runtime rather than
//! reimplementing them. It adds no logic of its own: every subcommand is a thin
//! projection of `encode`, `derive_invariants`, `compile`, and `ReadSession`
//! into JSON, so what it prints is what the compiler really decided.
//!
//! It deliberately lives outside `src/access_compiler/`, whose source text the
//! exact source fingerprint hashes. Adding or changing this file cannot refreeze
//! a generated kernel.
//!
//! Usage: `witness_explore <subcommand> '<json>'`
//!
//!   encode  {"values": [...], "recipe": "<spec>"}
//!   facts   {"values": [...], "recipe": "<spec>"}
//!   compile {"values": [...], "recipe": "<spec>", "query": {...}}
//!   run     {"values": [...], "recipe": "<spec>", "query": {...}}
//!   recipes {}
//!
//! Recipe specs are written as nested calls, e.g. `UnsignedDelta(256, BitPack)`,
//! `Frame(Delta(1024, BitPack))`, `Dictionary(BitPack)`, `Rle(64, BitPack)`.

use std::collections::BTreeMap;
use std::error::Error;

use witness::access_compiler::{
    Answer, ClosureMode, ClosureSpec, EncodedColumn, InputColumn, Predicate, Query, Recipe, Span,
    compile, derive_invariants, encode, execute_interpreted,
};

// ── minimal JSON emitter ────────────────────────────────────────────────────
// The crate is dependency-free by design outside the `experiment` feature, and
// pulling serde in for a debugging aid would be the wrong trade. These helpers
// emit the small, fixed shapes this bridge needs.

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn object(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{}:{}", quote(key), value))
        .collect();
    format!("{{{}}}", body.join(","))
}

fn array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

// ── minimal JSON reader ─────────────────────────────────────────────────────
// Only what the subcommands accept: flat objects whose values are numbers,
// strings, arrays of numbers, or one nested object (the query).

fn field<'a>(source: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = source.find(&needle)? + needle.len();
    let rest = source[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let end = match rest.as_bytes().first()? {
        b'[' => rest.find(']')? + 1,
        b'{' => {
            let mut depth = 0usize;
            let mut cut = 0usize;
            for (index, byte) in rest.bytes().enumerate() {
                match byte {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            cut = index + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            cut
        }
        b'"' => rest[1..].find('"')? + 2,
        _ => rest.find([',', '}']).unwrap_or(rest.len()),
    };
    Some(rest[..end].trim())
}

fn string_field(source: &str, key: &str) -> Option<String> {
    field(source, key).map(|raw| raw.trim_matches('"').to_string())
}

fn number_field(source: &str, key: &str) -> Option<i64> {
    field(source, key)?.trim_matches('"').parse().ok()
}

fn values_field(source: &str, key: &str) -> Option<Vec<Option<i64>>> {
    let raw = field(source, key)?;
    let inner = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|item| {
            let item = item.trim();
            if item == "null" {
                Some(None)
            } else {
                item.parse::<i64>().ok().map(Some)
            }
        })
        .collect()
}

// ── recipe parsing ──────────────────────────────────────────────────────────

fn parse_recipe(spec: &str) -> Result<Recipe, String> {
    let spec = spec.trim();
    let (head, argument) = match spec.find('(') {
        None => (spec, None),
        Some(open) => {
            if !spec.ends_with(')') {
                return Err(format!("unbalanced recipe spec: {spec}"));
            }
            (&spec[..open], Some(&spec[open + 1..spec.len() - 1]))
        }
    };
    let split_leading_number = |body: &str| -> Result<(usize, String), String> {
        let (first, rest) = body
            .split_once(',')
            .ok_or_else(|| format!("{head} needs an interval and a child recipe"))?;
        let interval: usize = first
            .trim()
            .parse()
            .map_err(|_| format!("{head}: bad interval {first}"))?;
        Ok((interval, rest.trim().to_string()))
    };
    match head.trim() {
        "BitPack" => Ok(Recipe::BitPack),
        "Dictionary" => Ok(Recipe::Dictionary(Box::new(parse_recipe(
            argument.ok_or("Dictionary needs a child recipe")?,
        )?))),
        "Frame" => Ok(Recipe::Frame(Box::new(parse_recipe(
            argument.ok_or("Frame needs a child recipe")?,
        )?))),
        "Nullable" => {
            let (rank_interval, child) =
                split_leading_number(argument.ok_or("Nullable needs (interval, child)")?)?;
            Ok(Recipe::Nullable {
                rank_interval,
                values: Box::new(parse_recipe(&child)?),
            })
        }
        "Patch" => {
            let (index_interval, child) =
                split_leading_number(argument.ok_or("Patch needs (interval, child)")?)?;
            Ok(Recipe::Patch {
                index_interval,
                values: Box::new(parse_recipe(&child)?),
            })
        }
        "For" => Ok(Recipe::For(Box::new(parse_recipe(
            argument.ok_or("For needs a child recipe")?,
        )?))),
        "Delta" => {
            let (restart_interval, child) =
                split_leading_number(argument.ok_or("Delta needs (interval, child)")?)?;
            Ok(Recipe::Delta {
                restart_interval,
                deltas: Box::new(parse_recipe(&child)?),
            })
        }
        "UnsignedDelta" => {
            let (restart_interval, child) =
                split_leading_number(argument.ok_or("UnsignedDelta needs (interval, child)")?)?;
            Ok(Recipe::UnsignedDelta {
                restart_interval,
                deltas: Box::new(parse_recipe(&child)?),
            })
        }
        "Rle" => {
            let (index_interval, child) =
                split_leading_number(argument.ok_or("Rle needs (interval, child)")?)?;
            Ok(Recipe::Rle {
                index_interval,
                values: Box::new(parse_recipe(&child)?),
            })
        }
        other => Err(format!("unknown recipe head {other}")),
    }
}

// ── projections ─────────────────────────────────────────────────────────────

fn build(request: &str) -> Result<EncodedColumn, Box<dyn Error>> {
    let values = values_field(request, "values").ok_or("missing \"values\"")?;
    let spec = string_field(request, "recipe").ok_or("missing \"recipe\"")?;
    let recipe = parse_recipe(&spec)?;
    Ok(encode(
        &recipe,
        InputColumn {
            values,
            patch_rows: Default::default(),
        },
    )?)
}

fn parse_query(request: &str, rows: usize) -> Result<Query, Box<dyn Error>> {
    let raw = field(request, "query").ok_or("missing \"query\"")?;
    let kind = string_field(raw, "kind").ok_or("query needs a \"kind\"")?;
    let span = Span::new(0, rows)?;
    Ok(match kind.as_str() {
        "get" => Query::Get {
            row: number_field(raw, "row").unwrap_or(0) as usize,
        },
        "sum" => Query::Sum { rows: span },
        "between" => Query::Between {
            rows: span,
            low: number_field(raw, "low").ok_or("between needs \"low\"")?,
            high: number_field(raw, "high").ok_or("between needs \"high\"")?,
        },
        "filter_between" => Query::Filter {
            predicate: Predicate::Between {
                low: number_field(raw, "low").ok_or("filter needs \"low\"")?,
                high: number_field(raw, "high").ok_or("filter needs \"high\"")?,
            },
        },
        "filter_equals" => Query::Filter {
            predicate: Predicate::Equals {
                value: number_field(raw, "value").ok_or("filter needs \"value\"")?,
            },
        },
        other => return Err(format!("unknown query kind {other}").into()),
    })
}

fn page_json(column: &EncodedColumn) -> String {
    let layout = column.page.layout();
    let checked = column.page.invariants();
    let fields: Vec<String> = layout
        .fields
        .iter()
        .map(|field| {
            let (kind, offset) = match field.location {
                witness::access_compiler::FieldLocation::Direct { offset } => ("direct", offset),
                witness::access_compiler::FieldLocation::Framed { decoded_offset, .. } => {
                    ("framed", decoded_offset)
                }
            };
            object(&[
                ("id", field.id.0.to_string()),
                ("name", quote(&field.name)),
                ("length", field.length.to_string()),
                ("alignment", field.alignment.to_string()),
                ("read_granularity", field.read_granularity.to_string()),
                ("location", quote(kind)),
                ("offset", offset.to_string()),
            ])
        })
        .collect();
    object(&[
        ("page_bytes", column.page.bytes().len().to_string()),
        ("rows", column.truth.len().to_string()),
        ("fields", array(&fields)),
        ("frames", layout.frames.len().to_string()),
        ("dependencies", layout.dependencies.len().to_string()),
        ("checked_non_decreasing", checked.non_decreasing.to_string()),
        (
            "checked_non_decreasing_non_null",
            checked.non_decreasing_non_null.to_string(),
        ),
        (
            "checked_null_placement",
            quote(&format!("{:?}", checked.null_placement)),
        ),
    ])
}

fn facts_json(column: &EncodedColumn) -> Result<String, Box<dyn Error>> {
    let set = derive_invariants(
        &column.decoder,
        column.page.layout(),
        column.page.invariants(),
    )?;
    let facts: Vec<String> = set
        .iter()
        .map(|fact| {
            object(&[
                ("node", fact.node.0.to_string()),
                ("scope", quote(&format!("{:?}", fact.scope))),
                ("property", quote(&format!("{:?}", fact.property))),
                ("evidence", quote(&format!("{:?}", fact.evidence))),
                (
                    "assumptions",
                    array(
                        &fact
                            .assumptions
                            .iter()
                            .map(|a| quote(&format!("{a:?}")))
                            .collect::<Vec<_>>(),
                    ),
                ),
            ])
        })
        .collect();
    Ok(object(&[
        ("root", column.decoder.root().0.to_string()),
        ("fact_count", facts.len().to_string()),
        ("facts", array(&facts)),
    ]))
}

fn plan_json(column: &EncodedColumn, query: Query) -> Result<String, Box<dyn Error>> {
    let plan = compile(column, query)?;
    let nodes: Vec<String> = plan
        .nodes
        .iter()
        .map(|node| {
            let (closure_kind, closure_bytes, closure_note) = match &node.byte_closure {
                ClosureSpec::Exact(closure) => {
                    ("exact", closure.delivered_bytes.to_string(), String::new())
                }
                ClosureSpec::RuntimeRefined { reason, .. } => {
                    ("runtime_refined", "null".to_string(), reason.clone())
                }
            };
            let authorization = match &node.authorization {
                witness::access_compiler::Authorization::Unconditional => quote("unconditional"),
                witness::access_compiler::Authorization::Fact {
                    scope, property, ..
                } => quote(&format!("{scope:?}::{property:?}")),
            };
            object(&[
                ("id", node.id.0.to_string()),
                ("op", quote(&format!("{:?}", node.op))),
                ("rows", quote(&format!("{:?}", node.rows))),
                (
                    "required_field_bytes",
                    node.required_fields.bytes().to_string(),
                ),
                ("closure", quote(closure_kind)),
                ("closure_bytes", closure_bytes),
                ("closure_note", quote(&closure_note)),
                ("guarantee", quote(&format!("{:?}", node.guarantee))),
                ("cites", authorization),
            ])
        })
        .collect();
    Ok(object(&[
        ("query", quote(&format!("{:?}", plan.query))),
        ("node_count", nodes.len().to_string()),
        ("output", quote(&format!("{:?}", plan.output))),
        ("nodes", array(&nodes)),
    ]))
}

fn run_json(column: &EncodedColumn, query: Query) -> Result<String, Box<dyn Error>> {
    let execution = execute_interpreted(column, &query, ClosureMode::Selective)?;
    let metrics = execution.metrics;
    let answer = match &execution.answer {
        Answer::Value(value) => format!("{{\"kind\":\"value\",\"value\":{value:?}}}")
            .replace("None", "null")
            .replace("Some(", "")
            .replace(')', ""),
        Answer::Sum(total) => object(&[("kind", quote("sum")), ("value", total.to_string())]),
        Answer::Count(count) => object(&[("kind", quote("count")), ("value", count.to_string())]),
        Answer::Ranges(ranges) => object(&[
            ("kind", quote("ranges")),
            (
                "ranges",
                array(
                    &ranges
                        .iter()
                        .map(|span| format!("[{},{}]", span.start, span.end))
                        .collect::<Vec<_>>(),
                ),
            ),
            (
                "rows",
                ranges
                    .iter()
                    .map(|span| span.len())
                    .sum::<usize>()
                    .to_string(),
            ),
        ]),
    };
    Ok(object(&[
        ("answer", answer),
        ("logical_bytes", metrics.logical_bytes.to_string()),
        ("delivered_bytes", metrics.delivered_bytes.to_string()),
        ("transferred_bytes", metrics.transferred_bytes.to_string()),
        (
            "transfer_operations",
            metrics.transfer_operations.to_string(),
        ),
        ("frames_decoded", metrics.frames_decoded.to_string()),
        ("page_bytes", column.page.bytes().len().to_string()),
    ]))
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let subcommand = arguments.first().map(String::as_str).unwrap_or("recipes");
    let request = arguments.get(1).cloned().unwrap_or_else(|| "{}".into());

    let output = match subcommand {
        "recipes" => {
            let catalogue: BTreeMap<&str, &str> = [
                ("BitPack", "fixed-width miniblocks; no order fact"),
                (
                    "For(child)",
                    "i64 base + child codes; affine order-preserving shift",
                ),
                (
                    "Delta(r, child)",
                    "signed per-step deltas; NO monotonicity proof",
                ),
                (
                    "UnsignedDelta(r, child)",
                    "unsigned per-step deltas; proves order within each restart span",
                ),
                (
                    "Dictionary(child)",
                    "sorted unique table; order-preserving code mapping",
                ),
                ("Rle(r, child)", "run values + lengths + sparse run index"),
                (
                    "Nullable(r, child)",
                    "validity + rank index + compact child",
                ),
                (
                    "Patch(r, child)",
                    "main child + exception positions; derives NO order fact",
                ),
                (
                    "Frame(child)",
                    "one Zstd frame; keeps value facts, erases cheap seek",
                ),
            ]
            .into_iter()
            .collect();
            array(
                &catalogue
                    .iter()
                    .map(|(spec, note)| object(&[("recipe", quote(spec)), ("note", quote(note))]))
                    .collect::<Vec<_>>(),
            )
        }
        "encode" => page_json(&build(&request)?),
        "facts" => facts_json(&build(&request)?)?,
        "compile" => {
            let column = build(&request)?;
            let query = parse_query(&request, column.truth.len())?;
            plan_json(&column, query)?
        }
        "run" => {
            let column = build(&request)?;
            let query = parse_query(&request, column.truth.len())?;
            run_json(&column, query)?
        }
        other => return Err(format!("unknown subcommand {other}").into()),
    };
    println!("{output}");
    Ok(())
}
