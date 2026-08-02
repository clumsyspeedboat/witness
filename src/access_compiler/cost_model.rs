const FEATURE_COUNT: usize = 10;

#[derive(Clone, Copy, Debug)]
pub struct CostFeatures {
    pub selected_rows: usize,
    pub decoded_rows: usize,
    pub delivered_bytes: usize,
    pub transfer_operations: usize,
    pub frames_decoded: usize,
    pub restart_entries: usize,
    pub rle_runs: usize,
    pub dictionary_entries: usize,
    pub patch_count: usize,
    pub nullable_nodes: usize,
}

impl CostFeatures {
    fn values(self) -> [f64; FEATURE_COUNT] {
        [
            self.selected_rows as f64,
            self.decoded_rows as f64,
            self.delivered_bytes as f64,
            self.transfer_operations as f64,
            self.frames_decoded as f64,
            self.restart_entries as f64,
            self.rle_runs as f64,
            self.dictionary_entries as f64,
            self.patch_count as f64,
            self.nullable_nodes as f64,
        ]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CostObservation {
    pub features: CostFeatures,
    pub runtime_ns: f64,
}

#[derive(Clone, Debug)]
pub struct CostModel {
    means: [f64; FEATURE_COUNT],
    scales: [f64; FEATURE_COUNT],
    weights: Vec<f64>,
}

impl CostModel {
    pub fn fit(observations: &[CostObservation]) -> Result<Self, String> {
        if observations.len() < FEATURE_COUNT + 1
            || observations.iter().any(|observation| {
                !observation.runtime_ns.is_finite() || observation.runtime_ns <= 0.0
            })
        {
            return Err("cost-model observations are insufficient or invalid".into());
        }
        let mut means = [0.0; FEATURE_COUNT];
        for observation in observations {
            for (mean, value) in means.iter_mut().zip(observation.features.values()) {
                *mean += value;
            }
        }
        for mean in &mut means {
            *mean /= observations.len() as f64;
        }
        let mut scales = [0.0; FEATURE_COUNT];
        for observation in observations {
            for ((scale, value), mean) in scales
                .iter_mut()
                .zip(observation.features.values())
                .zip(means)
            {
                *scale += (value - mean).powi(2);
            }
        }
        for scale in &mut scales {
            *scale = (*scale / observations.len() as f64).sqrt().max(1.0);
        }

        let dimension = FEATURE_COUNT + 1;
        let mut normal = vec![vec![0.0; dimension]; dimension];
        let mut target = vec![0.0; dimension];
        for observation in observations {
            let mut row = vec![1.0; dimension];
            for (index, value) in observation.features.values().into_iter().enumerate() {
                row[index + 1] = (value - means[index]) / scales[index];
            }
            let response = observation.runtime_ns.ln();
            for left in 0..dimension {
                target[left] += row[left] * response;
                for right in 0..dimension {
                    normal[left][right] += row[left] * row[right];
                }
            }
        }
        for (index, diagonal) in normal.iter_mut().enumerate().skip(1) {
            diagonal[index] += 1e-6;
        }
        let weights = solve(normal, target)?;
        Ok(Self {
            means,
            scales,
            weights,
        })
    }

    pub fn predict_ns(&self, features: CostFeatures) -> f64 {
        let mut prediction = self.weights[0];
        for (index, value) in features.values().into_iter().enumerate() {
            prediction +=
                self.weights[index + 1] * (value - self.means[index]) / self.scales[index];
        }
        prediction.exp()
    }
}

fn solve(mut matrix: Vec<Vec<f64>>, mut target: Vec<f64>) -> Result<Vec<f64>, String> {
    for pivot in 0..target.len() {
        let best = (pivot..target.len())
            .max_by(|&left, &right| {
                matrix[left][pivot]
                    .abs()
                    .total_cmp(&matrix[right][pivot].abs())
            })
            .unwrap();
        if matrix[best][pivot].abs() < 1e-12 {
            return Err("cost-model normal matrix is singular".into());
        }
        matrix.swap(pivot, best);
        target.swap(pivot, best);
        let divisor = matrix[pivot][pivot];
        for value in &mut matrix[pivot][pivot..] {
            *value /= divisor;
        }
        target[pivot] /= divisor;
        let pivot_values = matrix[pivot].clone();
        for row in 0..target.len() {
            if row == pivot {
                continue;
            }
            let factor = matrix[row][pivot];
            for (value, pivot_value) in matrix[row][pivot..].iter_mut().zip(&pivot_values[pivot..])
            {
                *value -= factor * pivot_value;
            }
            target[row] -= factor * target[pivot];
        }
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitted_model_preserves_a_known_runtime_ordering() {
        let observations = (1..=40)
            .map(|rows| CostObservation {
                features: CostFeatures {
                    selected_rows: rows,
                    decoded_rows: rows,
                    delivered_bytes: rows * 8,
                    transfer_operations: 1,
                    frames_decoded: 0,
                    restart_entries: rows % 3,
                    rle_runs: rows % 2,
                    dictionary_entries: 0,
                    patch_count: 0,
                    nullable_nodes: 0,
                },
                runtime_ns: 100.0 + rows as f64 * 12.0,
            })
            .collect::<Vec<_>>();
        let model = CostModel::fit(&observations).unwrap();
        assert!(
            model.predict_ns(observations[3].features)
                < model.predict_ns(observations[35].features)
        );
    }
}
