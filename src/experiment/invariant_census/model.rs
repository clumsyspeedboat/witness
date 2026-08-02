#[derive(Clone, Debug)]
pub struct ColumnRow {
    pub id: usize,
    pub group: String,
    pub source: String,
    pub name: String,
    pub rows: usize,
    pub nulls: usize,
    pub unique_non_null: usize,
    pub global_monotone_non_null: bool,
    pub null_placement: &'static str,
    pub distinct_non_null: bool,
    pub max_rank_displacement: usize,
    pub monotone_segment_rows: usize,
    pub monotone_segments: usize,
    pub smallest_candidate_bytes: usize,
    pub smallest_access_ready_bytes: usize,
    pub structural_monotone_bytes: Option<usize>,
    pub structural_monotone_premium: Option<f64>,
    pub structural_monotone_access_ready_bytes: Option<usize>,
    pub checked_monotone_access_ready_bytes: Option<usize>,
    pub structural_piecewise_access_ready_bytes: Option<usize>,
    pub order_mapping_access_ready_bytes: Option<usize>,
    pub order_mapping_access_ready_premium: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct PageRow {
    pub column: usize,
    pub page_size: usize,
    pub page: usize,
    pub rows: usize,
    pub nulls: usize,
    pub monotone_non_null: bool,
    pub distinct_non_null: bool,
    pub max_rank_displacement: usize,
    pub unique_non_null: usize,
}

#[derive(Clone, Debug)]
pub struct CandidateRow {
    pub column: usize,
    pub recipe: String,
    pub bytes: usize,
    pub structural_monotone: bool,
    pub structural_piecewise_monotone: bool,
    pub checked_monotone: bool,
    pub order_preserving_mapping: bool,
    pub framed: bool,
    pub restart_bound: Option<usize>,
    pub access_ready: bool,
    pub semantic_facts: usize,
    pub mapping_facts: usize,
    pub access_facts: usize,
}

#[derive(Clone, Debug)]
pub struct SourceRow {
    pub group: String,
    pub source: String,
    pub columns: usize,
    pub rows: usize,
    pub global_monotone_columns: usize,
    pub page_1024_total: usize,
    pub page_1024_monotone: usize,
    pub page_16384_total: usize,
    pub page_16384_monotone: usize,
    pub structural_monotone_columns: usize,
}
