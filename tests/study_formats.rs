#![cfg(feature = "experiment")]

use bytes::Bytes;
use witness::experiment::study_formats::{
    ParquetStudyConfig, decode_orc_i64_file, decode_parquet_i64_file, decode_pco_i64_file,
    decode_raw_i64_file, orc_i64_file, parquet_i64_file, pco_i64_file, raw_i64_file,
};
use witness::experiment::time_window::{
    brute_window_sum, parquet_boundary_sum_counted, parquet_full_sum_counted,
    parquet_indexed_sum_counted, parquet_late_sum_counted, parquet_oracle_sum_counted,
    parquet_pair_file,
};

#[test]
fn all_physical_study_formats_preserve_null_positions() {
    let values: Vec<Option<i64>> = (0..4096)
        .map(|index| (index % 17 != 0).then_some((index as i64 * 31) % 997))
        .collect();

    let raw = raw_i64_file(&values);
    assert_eq!(decode_raw_i64_file(&raw).unwrap(), values);

    let pco = pco_i64_file(&values, 12).unwrap();
    assert_eq!(decode_pco_i64_file(&pco).unwrap(), values);

    for config in [
        ParquetStudyConfig::DictionarySnappy,
        ParquetStudyConfig::DeltaZstd,
    ] {
        let parquet = parquet_i64_file(&values, config).unwrap();
        assert_eq!(decode_parquet_i64_file(&parquet).unwrap(), values);
    }

    let orc = orc_i64_file(&values).unwrap();
    assert_eq!(decode_orc_i64_file(&orc).unwrap(), values);
}

#[test]
fn nonnullable_wrappers_do_not_pay_for_a_validity_bitmap() {
    let values = vec![Some(7); 128];
    assert_eq!(raw_i64_file(&values).len(), 20 + 128 * 8);
    assert_eq!(decode_raw_i64_file(&raw_i64_file(&values)).unwrap(), values);
}

#[test]
fn time_window_arms_match_nullable_brute_force() {
    let timestamps: Vec<Option<i64>> = (0..4096)
        .map(|index| (index % 97 != 0).then_some(1_700_000_000 + index as i64 * 60))
        .collect();
    let values: Vec<Option<i64>> = (0..4096)
        .map(|index| (index % 31 != 0).then_some((index as i64 * 17) % 101))
        .collect();
    let (lower, upper) = (1_700_000_000 + 731 * 60, 1_700_000_000 + 2881 * 60);
    let (expected_sum, ranges) = brute_window_sum(&timestamps, &values, lower, upper).unwrap();
    let expected_rows = ranges.iter().map(|(start, end)| end - start).sum::<usize>();

    let parquet = Bytes::from(parquet_pair_file(&timestamps, &values, 1024).unwrap());
    let boundary = parquet_boundary_sum_counted(parquet.clone(), lower, upper).unwrap();
    assert!(boundary.boundary_order_used);
    assert!(boundary.candidate_pages > 0);
    for result in [
        parquet_full_sum_counted(parquet.clone(), lower, upper).unwrap(),
        parquet_late_sum_counted(parquet.clone(), lower, upper).unwrap(),
        parquet_indexed_sum_counted(parquet.clone(), lower, upper).unwrap(),
        boundary,
        parquet_oracle_sum_counted(parquet, &ranges, timestamps.len()).unwrap(),
    ] {
        assert_eq!(result.sum, expected_sum);
        assert_eq!(result.matched_rows, expected_rows);
        assert!(result.bytes_read > 0);
        assert!(result.unique_bytes > 0);
        assert!(result.unique_bytes <= result.bytes_read);
        assert!(result.read_calls > 0);
    }
}
