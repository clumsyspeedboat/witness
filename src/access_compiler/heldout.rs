use std::collections::BTreeSet;

use super::{InputColumn, Recipe};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataProfile {
    Trend,
    TrendNullsFirst,
    TrendNullsLast,
    Runs,
    ManyRuns,
    SmallDomain,
    Sawtooth,
}

#[derive(Clone, Debug)]
pub struct HeldoutCase {
    pub id: usize,
    pub recipe: Recipe,
    pub profile: DataProfile,
    pub nullable: bool,
    pub patched: bool,
}

impl HeldoutCase {
    pub fn name(&self) -> String {
        self.recipe.name()
    }
}

pub fn heldout_cases() -> Vec<HeldoutCase> {
    let bitpack = || Recipe::BitPack;
    let for_bp = || Recipe::For(Box::new(bitpack()));
    let delta_bp = || Recipe::Delta {
        restart_interval: 256,
        deltas: Box::new(bitpack()),
    };
    let unsigned_delta_bp = || Recipe::UnsignedDelta {
        restart_interval: 256,
        deltas: Box::new(bitpack()),
    };
    let for_delta = || Recipe::For(Box::new(delta_bp()));
    let dictionary_bp = || Recipe::Dictionary(Box::new(bitpack()));
    let dictionary_rle = || {
        Recipe::Dictionary(Box::new(Recipe::Rle {
            index_interval: 32,
            values: Box::new(bitpack()),
        }))
    };
    let nullable = |values| Recipe::Nullable {
        rank_interval: 256,
        values: Box::new(values),
    };
    let patch = |values| Recipe::Patch {
        index_interval: 32,
        values: Box::new(values),
    };
    let frame = |values| Recipe::Frame(Box::new(values));

    vec![
        case(0, nullable(for_bp()), DataProfile::Trend, true, false),
        case(1, patch(for_bp()), DataProfile::Trend, false, true),
        case(
            2,
            nullable(dictionary_rle()),
            DataProfile::Runs,
            true,
            false,
        ),
        case(3, frame(for_delta()), DataProfile::Trend, false, false),
        case(
            4,
            patch(dictionary_bp()),
            DataProfile::SmallDomain,
            false,
            true,
        ),
        case(
            5,
            nullable(patch(for_delta())),
            DataProfile::Trend,
            true,
            true,
        ),
        case(
            6,
            frame(nullable(for_bp())),
            DataProfile::Trend,
            true,
            false,
        ),
        case(7, nullable(for_delta()), DataProfile::Trend, true, false),
        case(8, patch(for_delta()), DataProfile::Trend, false, true),
        case(9, frame(dictionary_rle()), DataProfile::Runs, false, false),
        case(
            10,
            nullable(patch(dictionary_rle())),
            DataProfile::Runs,
            true,
            true,
        ),
        case(11, frame(patch(for_bp())), DataProfile::Trend, false, true),
        case(12, patch(dictionary_rle()), DataProfile::Runs, false, true),
        case(
            13,
            nullable(dictionary_bp()),
            DataProfile::SmallDomain,
            true,
            false,
        ),
        case(
            14,
            frame(nullable(patch(for_delta()))),
            DataProfile::Trend,
            true,
            true,
        ),
        case(
            15,
            nullable(unsigned_delta_bp()),
            DataProfile::TrendNullsFirst,
            true,
            false,
        ),
        case(
            16,
            nullable(unsigned_delta_bp()),
            DataProfile::TrendNullsLast,
            true,
            false,
        ),
        case(17, dictionary_rle(), DataProfile::ManyRuns, false, false),
        case(18, unsigned_delta_bp(), DataProfile::Sawtooth, false, false),
    ]
}

pub fn input_for(case: &HeldoutCase, rows: usize) -> InputColumn {
    let mut patch_rows = BTreeSet::new();
    let values = (0..rows)
        .map(|row| {
            let is_null = match case.profile {
                DataProfile::TrendNullsFirst => row < rows / 16,
                DataProfile::TrendNullsLast => row >= rows - rows / 16,
                _ => row % 17 == 0,
            };
            if case.nullable && is_null {
                return None;
            }
            let mut value = match case.profile {
                DataProfile::Trend | DataProfile::TrendNullsFirst | DataProfile::TrendNullsLast => {
                    1_000_000 + 7 * row as i64 + (row % 5) as i64
                }
                DataProfile::Runs => 2_000_000 + [10, 20, 10, 30][(row / 37) % 4],
                DataProfile::ManyRuns => 2_000_000 + (((row / 8) * 37) % 1_024) as i64,
                DataProfile::SmallDomain => 3_000_000 + [0, 1, 7, 2, 1][row % 5],
                DataProfile::Sawtooth => 4_000_000 + 7 * (row % 256) as i64,
            };
            if case.patched && row % 997 == 503 {
                value += 1_000_000_000;
                patch_rows.insert(row);
            }
            Some(value)
        })
        .collect();
    InputColumn { values, patch_rows }
}

/// An *exact source fingerprint* over the derivation and code-generation
/// modules, not a semantic one. It hashes the literal text of the files below,
/// so reformatting, a comment, or widening a constant's visibility invalidates
/// it exactly as a rule change would, and every frozen kernel must then be
/// regenerated. That is deliberately conservative: it can never miss a semantic
/// change, at the cost of firing on edits that carry none. Treat a changed
/// fingerprint as "the frozen kernels no longer correspond to this source",
/// not as "the calculus behaves differently".
///
/// Note this function's own file is absent from the list, so renaming it would
/// still perturb the hash via the call site in `codegen.rs`.
pub fn primitive_rule_fingerprint() -> u64 {
    const RULES: &str = concat!(
        include_str!("decoder.rs"),
        include_str!("invariants.rs"),
        include_str!("layout.rs"),
        include_str!("plan.rs"),
        include_str!("encode.rs"),
        include_str!("format.rs"),
        include_str!("runtime.rs"),
        include_str!("support.rs"),
        include_str!("interpreter.rs"),
        include_str!("compiler.rs"),
        include_str!("codegen.rs"),
        include_str!("static_baseline.rs"),
    );
    RULES.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3)
    })
}

fn case(
    id: usize,
    recipe: Recipe,
    profile: DataProfile,
    nullable: bool,
    patched: bool,
) -> HeldoutCase {
    HeldoutCase {
        id,
        recipe,
        profile,
        nullable,
        patched,
    }
}
