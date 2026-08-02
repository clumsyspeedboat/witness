use proptest::prelude::*;
use witness::access_compiler::{
    BlockBloom, BlockMinMax, OutputGuarantee, SerializedPage, SparseFence, intersect_candidates,
    refine_eq, refine_in,
};

fn expected(values: &[Option<i64>], target: i64) -> Vec<usize> {
    values
        .iter()
        .enumerate()
        .filter_map(|(row, value)| (*value == Some(target)).then_some(row))
        .collect()
}

#[test]
fn bloom_and_minmax_in_plans_are_exact_after_refinement() {
    let values = (0..4_096)
        .map(|row| (row % 19 != 0).then_some((row % 257) as i64))
        .collect::<Vec<_>>();
    let targets = [1, 7, 193, 10_000];
    let bloom = BlockBloom::build(&values, 128, 11).unwrap();
    let minmax = BlockMinMax::build(&values, 128).unwrap();
    let expected = values
        .iter()
        .enumerate()
        .filter_map(|(row, value)| {
            value
                .is_some_and(|value| targets.contains(&value))
                .then_some(row)
        })
        .collect::<Vec<_>>();
    for plan in [bloom.probe_in(&targets), minmax.probe_in(&targets)] {
        assert_eq!(
            expanded(&refine_in(&values, &plan, &targets).unwrap()),
            expected
        );
    }
}

fn expanded(ranges: &[witness::access_compiler::Span]) -> Vec<usize> {
    ranges
        .iter()
        .flat_map(|range| range.start..range.end)
        .collect()
}

proptest! {
    #[test]
    fn bloom_and_minmax_never_drop_an_equality_match(
        values in prop::collection::vec(prop::option::of(-10_000_i64..10_000), 1..4_096),
        target in -10_100_i64..10_100,
    ) {
        let bloom = BlockBloom::build(&values, 128, 11).unwrap();
        let minmax = BlockMinMax::build(&values, 128).unwrap();
        let bloom_plan = bloom.probe_eq(target);
        let minmax_plan = minmax.probe_eq(target);
        let combined = intersect_candidates(&bloom_plan, &minmax_plan);
        prop_assert_eq!(bloom_plan.guarantee, OutputGuarantee::CandidateBitmap);
        prop_assert_eq!(expanded(&refine_eq(&values, &bloom_plan, target).unwrap()), expected(&values, target));
        prop_assert_eq!(expanded(&refine_eq(&values, &minmax_plan, target).unwrap()), expected(&values, target));
        prop_assert_eq!(expanded(&refine_eq(&values, &combined, target).unwrap()), expected(&values, target));
    }
}

#[test]
fn sparse_fence_is_conservative_for_duplicates_and_absent_values() {
    let values = (0..4_096)
        .map(|row| Some((row / 17) as i64))
        .collect::<Vec<_>>();
    let fence = SparseFence::build_equal_budget(&values, 2_048).unwrap();
    assert!(fence.bytes() <= 2_048);
    for target in [-1, 0, 37, 240, 241, 999] {
        let plan = fence.probe_eq(target);
        assert_eq!(
            expanded(&refine_eq(&values, &plan, target).unwrap()),
            expected(&values, target)
        );
    }
}

#[test]
fn sparse_fence_rejects_columns_without_an_order_certificate() {
    let unordered = vec![Some(1), Some(3), Some(2)];
    let nullable = vec![Some(1), None, Some(2)];
    assert!(SparseFence::build_equal_budget(&unordered, 64).is_err());
    assert!(SparseFence::build_equal_budget(&nullable, 64).is_err());
}

/// Binds the manifest's certified-descriptor literal to real serialized bytes.
///
/// The manifest states the size as a literal because `primitive_rule_fingerprint`
/// hashes the source text of `format.rs`, so widening its constants' visibility
/// would refreeze every generated kernel for no semantic change.
///
/// Scope, stated honestly: the crate has no version-2 serializer -- `LEGACY_*`
/// appears only on the parse path -- so `V2_BASE` cannot be bound by round-trip
/// and is asserted here as the parser's documented legacy contract. `V3_BASE`
/// *is* bound: it is recovered from every committed page below.
///
/// The load-bearing assertion is the last one. Adding a 16-byte descriptor to a
/// header that is then aligned to 64 bytes costs 0 or 64 bytes on any single
/// page, never 16; but across pages whose unaligned length is spread over the
/// alignment period, the mean cost returns to exactly the descriptor size. That
/// is why the paper may quote 16 B/page as an amortized figure and must not
/// quote it as a per-page one.
#[test]
fn certificate_header_premium_matches_serialized_pages() {
    const FIELD_DESC: usize = 48;
    const FRAME_DESC: usize = 32;
    const DEPENDENCY_DESC: usize = 32;
    const ALIGNMENT: usize = 64;
    const V3_BASE: usize = 48;
    const V2_BASE: usize = 32;

    let manifest = std::fs::read_to_string("experiments/results/claim_manifest.csv").unwrap();
    let claimed: usize = manifest
        .lines()
        .find_map(|line| line.strip_prefix("WitCertificateHeaderBytes,"))
        .expect("missing WitCertificateHeaderBytes")
        .parse()
        .unwrap();
    assert_eq!(
        claimed,
        V3_BASE - V2_BASE,
        "manifest descriptor size drifted"
    );

    let align = |value: usize| value.div_ceil(ALIGNMENT) * ALIGNMENT;
    let mut premiums = Vec::new();
    for directory in [
        "docs/generated/artifacts",
        "experiments/results/access_compiler/pages",
    ] {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("acp") {
                continue;
            }
            let page = SerializedPage::parse(std::fs::read(&path).unwrap())
                .expect("committed page must parse");
            let layout = page.layout();
            let desc = layout.fields.len() * FIELD_DESC
                + layout.frames.len() * FRAME_DESC
                + layout.dependencies.len() * DEPENDENCY_DESC;

            // Recovers V3_BASE from real bytes: the metadata field spans exactly
            // the aligned header.
            assert_eq!(
                layout.fields[0].length,
                align(V3_BASE + desc),
                "{}: header disagrees with the version-3 base",
                path.display()
            );
            premiums.push(align(V3_BASE + desc) - align(V2_BASE + desc));
        }
    }

    assert!(
        premiums.len() >= 24,
        "expected the committed page corpus, got {}",
        premiums.len()
    );
    assert!(
        premiums.iter().all(|p| *p == 0 || *p == ALIGNMENT),
        "alignment must quantize the descriptor to 0 or {ALIGNMENT} bytes, saw {premiums:?}"
    );
    assert!(
        !premiums.contains(&claimed),
        "no single page should pay exactly the descriptor size"
    );
    let mean = premiums.iter().sum::<usize>() as f64 / premiums.len() as f64;
    assert!(
        (mean - claimed as f64).abs() < 1e-9,
        "amortized premium {mean} should equal the {claimed}-byte descriptor"
    );
}
