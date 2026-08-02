pub const YELLOW_PARQUET: &str = "eval/data/yellow_tripdata_2024-01.parquet";

#[derive(Clone, Debug)]
pub struct Column {
    pub group: String,
    pub source: String,
    pub name: String,
    pub mode: String,
    pub values: Vec<Option<i64>>,
    /// Original doubles for columns where no decimal scale round-trips exactly
    /// (the measurement protocol). Such columns run the float codec path; `values` is left
    /// empty and the column never enters integer comparisons.
    pub float_values: Option<Vec<Option<f64>>>,
    pub exact_i64: bool,
}

impl Column {
    pub fn len(&self) -> usize {
        self.float_values
            .as_ref()
            .map_or(self.values.len(), Vec::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
