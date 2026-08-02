use std::fs;

use witness::access_compiler::{encode, generate_rust_module, primitive_rule_fingerprint};
use witness::experiment::documentation::example_columns;

fn claim<'a>(manifest: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name},");
    manifest
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing claim {name}"))
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn generated_documentation_inputs_have_not_drifted() {
    let examples = example_columns();
    let columns = examples
        .iter()
        .map(|example| encode(&example.recipe, example.input()).unwrap())
        .collect::<Vec<_>>();

    let generated = generate_rust_module(&columns).unwrap();
    let checked =
        fs::read_to_string("experiments/generated/documentation_example/generated.rs").unwrap();
    assert_eq!(generated, checked, "regenerate documentation kernels");

    for (example, column) in examples.iter().zip(&columns) {
        let page = fs::read(format!("docs/generated/artifacts/{}.acp", example.name)).unwrap();
        assert_eq!(page, column.page.bytes(), "stale {} page", example.name);
    }

    for (manifest_name, suffix) in [
        ("rawi64.manifest.tsv", "rawi64"),
        ("rawi64-zstd.manifest.tsv", "rawi64.zst"),
        ("pcodec.manifest.tsv", "pco"),
        ("witness.manifest.tsv", "acp"),
    ] {
        let manifest =
            fs::read_to_string(format!("docs/generated/artifacts/{manifest_name}")).unwrap();
        let mut total = 0;
        for example in &examples {
            let artifact = format!("{}.{}", example.name, suffix);
            let bytes = fs::metadata(format!("docs/generated/artifacts/{artifact}"))
                .unwrap()
                .len();
            total += bytes;
            assert!(manifest.contains(&format!("{}\t{}\t{}", example.name, bytes, artifact)));
        }
        assert!(manifest.contains(&format!("TOTAL\t{total}\t-")));
    }

    let walkthrough = fs::read_to_string("docs/generated/END_TO_END_EXAMPLE.md").unwrap();
    assert!(walkthrough.contains(&format!("{:#018x}", primitive_rule_fingerprint())));
    assert!(
        examples
            .iter()
            .all(|example| walkthrough.contains(example.name))
    );
}

/// Extract `\newcommand{\WitX}{...}` bodies, counting braces so that values
/// containing `{}` or spanning lines are captured whole.
fn paper_macros(tex: &str) -> Vec<(String, String)> {
    let mut macros = Vec::new();
    let mut rest = tex;
    while let Some(start) = rest.find("\\newcommand{\\Wit") {
        rest = &rest[start + "\\newcommand{\\".len()..];
        let Some(name_end) = rest.find('}') else {
            break;
        };
        let name = rest[..name_end].to_string();
        rest = &rest[name_end + 1..];
        if !rest.starts_with('{') {
            continue;
        }
        let mut depth = 0usize;
        let mut end = None;
        for (offset, character) in rest.char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        macros.push((name, rest[1..end].to_string()));
        rest = &rest[end + 1..];
    }
    macros
}

/// The paper promises that no reported statistic is hand-entered. Its macro
/// block is the one place a number could drift from the evidence without any
/// study failing, so bind every `\Wit` macro to the generated manifest.
///
/// Every `.tex` in `paper/` is read, so it does not matter whether the claim
/// block lives in the manuscript or in a generated file it inputs.
///
/// The manuscript is not part of the public artifact, so this check is a no-op
/// wherever `paper/` is absent rather than a failure for anyone reproducing.
#[test]
fn claim_bearing_paper_macros_match_the_manifest() {
    let Ok(entries) = fs::read_dir("paper") else {
        return;
    };
    let mut sources: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|suffix| suffix == "tex") {
            sources.push(fs::read_to_string(&path).unwrap());
        }
    }
    if sources.is_empty() {
        return;
    }
    let tex = sources.join("\n");

    let mut manifest = std::collections::BTreeMap::new();
    let mut reader = csv::Reader::from_path("experiments/results/claim_manifest.csv").unwrap();
    for record in reader.records() {
        let record = record.unwrap();
        manifest.insert(record[0].to_string(), record[1].to_string());
    }

    let macros = paper_macros(&tex);
    assert!(
        macros.len() > 50,
        "only {} macros parsed out of the paper; the extractor is broken",
        macros.len()
    );

    let mut wrong = Vec::new();
    for (name, value) in &macros {
        match manifest.get(name) {
            None => wrong.push(format!(
                "{name}: defined in the paper, absent from the manifest"
            )),
            Some(expected) if normalized(expected) != normalized(value) => wrong.push(format!(
                "{name}: paper has {:?}, manifest has {:?}",
                normalized(value),
                normalized(expected)
            )),
            Some(_) => {}
        }
    }
    assert!(
        wrong.is_empty(),
        "regenerate the paper's claim block from experiments/results/claim_manifest.csv:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn claim_bearing_prose_matches_the_manifest() {
    let manifest = fs::read_to_string("experiments/results/claim_manifest.csv").unwrap();
    let median = claim(&manifest, "WitSourceDirectBoundaryMedian");
    let ci_low = claim(&manifest, "WitSourceDirectBoundaryCiLow");
    let ci_high = claim(&manifest, "WitSourceDirectBoundaryCiHigh");
    let page_ratio = claim(&manifest, "WitColdPageOverFull");
    let coalesced_calls = claim(&manifest, "WitColdCoalescedCalls");
    let coalesced_ratio = claim(&manifest, "WitColdCoalescedOverFull");

    let limitations = normalized(&fs::read_to_string("docs/LIMITATIONS.md").unwrap());
    let encodings = normalized(&fs::read_to_string("docs/ENCODINGS.md").unwrap());
    let walkthrough = normalized(&fs::read_to_string("docs/WALKTHROUGH.md").unwrap());

    let boundary = format!("median {median}x with a 95% bootstrap interval [{ci_low}, {ci_high}]");
    assert!(limitations.contains(&boundary), "stale boundary summary");
    assert!(
        encodings.contains(&format!("[{ci_low}, {ci_high}], below parity")),
        "stale encoding-scope interval"
    );

    let cold_limitation = format!(
        "{page_ratio}x a full read; coalescing issued {coalesced_calls} calls and reached {coalesced_ratio}x"
    );
    let cold_walkthrough = format!(
        "{page_ratio}x a full read; coalescing reached {coalesced_calls} calls and {coalesced_ratio}x"
    );
    assert!(
        limitations.contains(&cold_limitation),
        "stale cold-storage limitation"
    );
    assert!(
        walkthrough.contains(&cold_walkthrough),
        "stale cold-storage walkthrough"
    );
}
