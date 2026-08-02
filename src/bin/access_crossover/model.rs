use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};

use witness::access_compiler::{CostFeatures, CostModel, CostObservation};

use super::measure::{CurveCell, PlanKind, QueryKind, TOTAL_CASES, TRAINING_CASES};
use super::storage::StorageTier;

type EvaluationKey = (FeatureMode, &'static str, StorageTier, QueryKind);
type EvaluationGroups = BTreeMap<EvaluationKey, Vec<(bool, f64)>>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FeatureMode {
    PreflightFree,
    ExactClosure,
}

impl FeatureMode {
    fn name(self) -> &'static str {
        match self {
            Self::PreflightFree => "preflight_free_estimate",
            Self::ExactClosure => "exact_closure_features",
        }
    }
}

pub struct ModelSet {
    models: BTreeMap<(FeatureMode, StorageTier, QueryKind), CostModel>,
    guards: BTreeMap<(FeatureMode, StorageTier, QueryKind), f64>,
}

impl ModelSet {
    pub fn predict_ns(
        &self,
        mode: FeatureMode,
        tier: StorageTier,
        query: QueryKind,
        features: CostFeatures,
    ) -> f64 {
        self.models[&(mode, tier, query)].predict_ns(features)
    }

    pub fn choose(
        &self,
        mode: FeatureMode,
        tier: StorageTier,
        query: QueryKind,
        selective: CostFeatures,
        fused: CostFeatures,
    ) -> PlanKind {
        let guard = self.guards[&(mode, tier, query)];
        if self.predict_ns(mode, tier, query, fused) * guard
            < self.predict_ns(mode, tier, query, selective)
        {
            PlanKind::Fused
        } else {
            PlanKind::Selective
        }
    }
}

pub fn fit_and_evaluate(
    cells: &[CurveCell],
    result_dir: &str,
) -> Result<ModelSet, Box<dyn std::error::Error>> {
    let mut models = BTreeMap::new();
    for mode in [FeatureMode::PreflightFree, FeatureMode::ExactClosure] {
        for tier in StorageTier::ALL {
            for query in [QueryKind::Sum, QueryKind::Between] {
                let observations = cells
                    .iter()
                    .filter(|cell| {
                        cell.case < TRAINING_CASES && cell.tier == tier && cell.query == query
                    })
                    .map(|cell| CostObservation {
                        features: features(cell, mode),
                        runtime_ns: cell.median_ns,
                    })
                    .collect::<Vec<_>>();
                models.insert((mode, tier, query), CostModel::fit(&observations)?);
            }
        }
    }
    let mut guards = BTreeMap::new();
    for mode in [FeatureMode::PreflightFree, FeatureMode::ExactClosure] {
        for tier in StorageTier::ALL {
            for query in [QueryKind::Sum, QueryKind::Between] {
                let key = (mode, tier, query);
                guards.insert(key, tune_guard(cells, &models[&key], mode, tier, query));
            }
        }
    }
    let models = ModelSet { models, guards };
    write_crossovers(cells, result_dir)?;
    write_evaluation(cells, &models, result_dir)?;
    Ok(models)
}

fn write_crossovers(
    cells: &[CurveCell],
    result_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut output = BufWriter::new(File::create(format!("{result_dir}/crossovers.csv"))?);
    writeln!(
        output,
        "case,split,recipe,query,tier,first_fused_win_rows,first_fused_win_selectivity,selective_wins,cells"
    )?;
    for case in 0..TOTAL_CASES {
        for query in [QueryKind::Sum, QueryKind::Between] {
            for tier in StorageTier::ALL {
                let mut widths = cells
                    .iter()
                    .filter(|cell| {
                        cell.case == case
                            && cell.query == query
                            && cell.tier == tier
                            && cell.plan == PlanKind::Selective
                    })
                    .collect::<Vec<_>>();
                widths.sort_by_key(|cell| cell.rows.len());
                let first = widths.iter().find(|selective| {
                    counterpart(cells, selective, PlanKind::Fused).median_ns < selective.median_ns
                });
                let selective_wins = widths
                    .iter()
                    .filter(|selective| {
                        selective.median_ns
                            <= counterpart(cells, selective, PlanKind::Fused).median_ns
                    })
                    .count();
                let recipe = &widths[0].recipe;
                let (rows, fraction) = first.map_or((String::new(), String::new()), |cell| {
                    (
                        cell.rows.len().to_string(),
                        format!(
                            "{:.8}",
                            cell.rows.len() as f64 / super::measure::ROWS as f64
                        ),
                    )
                });
                writeln!(
                    output,
                    "{},{},{},{},{},{},{},{},{}",
                    case,
                    if case < TRAINING_CASES {
                        "train"
                    } else {
                        "heldout"
                    },
                    quote(recipe),
                    query.name(),
                    tier.name(),
                    rows,
                    fraction,
                    selective_wins,
                    widths.len(),
                )?;
            }
        }
    }
    output.flush()?;
    Ok(())
}

fn write_evaluation(
    cells: &[CurveCell],
    models: &ModelSet,
    result_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut output = BufWriter::new(File::create(format!("{result_dir}/model_eval.csv"))?);
    writeln!(
        output,
        "feature_mode,case,recipe,query,rows,selectivity,tier,oracle_plan,chosen_plan,correct,selective_ns,fused_ns,predicted_selective_ns,predicted_fused_ns,regret,guard"
    )?;
    let mut groups = EvaluationGroups::new();
    for selective in cells.iter().filter(|cell| {
        cell.case >= TRAINING_CASES && cell.case < TOTAL_CASES && cell.plan == PlanKind::Selective
    }) {
        let fused = counterpart(cells, selective, PlanKind::Fused);
        let oracle = if selective.median_ns <= fused.median_ns {
            PlanKind::Selective
        } else {
            PlanKind::Fused
        };
        for mode in [FeatureMode::PreflightFree, FeatureMode::ExactClosure] {
            let predicted_selective = models.predict_ns(
                mode,
                selective.tier,
                selective.query,
                features(selective, mode),
            );
            let predicted_fused =
                models.predict_ns(mode, fused.tier, fused.query, features(fused, mode));
            let chosen = models.choose(
                mode,
                selective.tier,
                selective.query,
                features(selective, mode),
                features(fused, mode),
            );
            let chosen_ns = match chosen {
                PlanKind::Selective => selective.median_ns,
                PlanKind::Fused => fused.median_ns,
            };
            let oracle_ns = selective.median_ns.min(fused.median_ns);
            let regret = chosen_ns / oracle_ns;
            groups
                .entry((mode, "cost_model", selective.tier, selective.query))
                .or_default()
                .push((chosen == oracle, regret));
            for (policy, plan) in [
                ("always_selective", PlanKind::Selective),
                ("always_fused", PlanKind::Fused),
            ] {
                let runtime = match plan {
                    PlanKind::Selective => selective.median_ns,
                    PlanKind::Fused => fused.median_ns,
                };
                groups
                    .entry((mode, policy, selective.tier, selective.query))
                    .or_default()
                    .push((plan == oracle, runtime / oracle_ns));
            }
            writeln!(
                output,
                "{},{},{},{},{},{:.8},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{:.6},{:.3}",
                mode.name(),
                selective.case,
                quote(&selective.recipe),
                selective.query.name(),
                selective.rows.len(),
                selective.rows.len() as f64 / super::measure::ROWS as f64,
                selective.tier.name(),
                oracle.name(),
                chosen.name(),
                usize::from(chosen == oracle),
                selective.median_ns,
                fused.median_ns,
                predicted_selective,
                predicted_fused,
                regret,
                models.guards[&(mode, selective.tier, selective.query)],
            )?;
        }
    }
    output.flush()?;

    let mut summary = BufWriter::new(File::create(format!("{result_dir}/model_summary.csv"))?);
    writeln!(
        summary,
        "feature_mode,tier,query,heldout_cells,accuracy,median_regret,p95_regret,max_regret,mean_regret,guard"
    )?;
    let mut policy_summary =
        BufWriter::new(File::create(format!("{result_dir}/policy_summary.csv"))?);
    writeln!(
        policy_summary,
        "feature_mode,policy,tier,query,heldout_cells,accuracy,mean_regret,median_regret,p95_regret,max_regret"
    )?;
    for ((mode, policy, tier, query), values) in groups {
        let accuracy =
            values.iter().filter(|(correct, _)| *correct).count() as f64 / values.len() as f64;
        let mut regrets = values.iter().map(|(_, regret)| *regret).collect::<Vec<_>>();
        regrets.sort_by(f64::total_cmp);
        let mean = regrets.iter().sum::<f64>() / regrets.len() as f64;
        if policy == "cost_model" {
            writeln!(
                summary,
                "{},{},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.3}",
                mode.name(),
                tier.name(),
                query.name(),
                regrets.len(),
                accuracy,
                regrets[regrets.len() / 2],
                regrets[regrets.len() * 95 / 100],
                regrets[regrets.len() - 1],
                mean,
                models.guards[&(mode, tier, query)],
            )?;
        }
        writeln!(
            policy_summary,
            "{},{},{},{},{},{:.6},{:.6},{:.6},{:.6},{:.6}",
            mode.name(),
            policy,
            tier.name(),
            query.name(),
            regrets.len(),
            accuracy,
            mean,
            regrets[regrets.len() / 2],
            regrets[regrets.len() * 95 / 100],
            regrets[regrets.len() - 1],
        )?;
    }
    summary.flush()?;
    policy_summary.flush()?;
    Ok(())
}

fn tune_guard(
    cells: &[CurveCell],
    model: &CostModel,
    mode: FeatureMode,
    tier: StorageTier,
    query: QueryKind,
) -> f64 {
    let candidates = [1.0, 1.02, 1.05, 1.1, 1.15, 1.25, 1.5, 2.0, 1e9];
    candidates
        .into_iter()
        .min_by(|left, right| {
            training_regret(cells, model, mode, tier, query, *left)
                .total_cmp(&training_regret(cells, model, mode, tier, query, *right))
                .then_with(|| right.total_cmp(left))
        })
        .unwrap()
}

fn training_regret(
    cells: &[CurveCell],
    model: &CostModel,
    mode: FeatureMode,
    tier: StorageTier,
    query: QueryKind,
    guard: f64,
) -> f64 {
    let mut regret = 0.0;
    let mut count = 0;
    for selective in cells.iter().filter(|cell| {
        cell.case < TRAINING_CASES
            && cell.tier == tier
            && cell.query == query
            && cell.plan == PlanKind::Selective
    }) {
        let fused = counterpart(cells, selective, PlanKind::Fused);
        let chosen = if model.predict_ns(features(fused, mode)) * guard
            < model.predict_ns(features(selective, mode))
        {
            fused.median_ns
        } else {
            selective.median_ns
        };
        regret += chosen / selective.median_ns.min(fused.median_ns);
        count += 1;
    }
    regret / count as f64
}

fn features(cell: &CurveCell, mode: FeatureMode) -> CostFeatures {
    match mode {
        FeatureMode::PreflightFree => cell.preflight_free_features(),
        FeatureMode::ExactClosure => cell.exact_features(),
    }
}

fn counterpart<'a>(cells: &'a [CurveCell], cell: &CurveCell, plan: PlanKind) -> &'a CurveCell {
    cells
        .iter()
        .find(|candidate| {
            candidate.case == cell.case
                && candidate.query == cell.query
                && candidate.rows == cell.rows
                && candidate.tier == cell.tier
                && candidate.plan == plan
        })
        .expect("paired crossover cell is absent")
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
