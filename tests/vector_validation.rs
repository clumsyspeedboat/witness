#![cfg(feature = "experiment")]

use witness::access_compiler::{
    ClosureMode, InputColumn, NullPlacement, ReadSession, Recipe, Span, encode, vector,
};

#[test]
fn vector_queries_reject_reversed_and_out_of_bounds_spans() {
    let column = encode(
        &Recipe::For(Box::new(Recipe::BitPack)),
        InputColumn::dense(vec![7; 8]),
    )
    .unwrap();
    let session = || ReadSession::new(&column.page, ClosureMode::Selective);
    for rows in [Span { start: 4, end: 3 }, Span { start: 9, end: 9 }] {
        assert!(vector::sum(&column.decoder, &mut session(), rows).is_err());
        assert!(vector::between(&column.decoder, &mut session(), rows, 0, 10).is_err());
        assert!(vector::between_selection(&column.decoder, &mut session(), rows, 0, 10).is_err());
        assert!(vector::non_null(&column.decoder, &mut session(), rows).is_err());
        assert!(
            vector::search_monotone(
                &column.decoder,
                &mut session(),
                rows,
                0,
                10,
                NullPlacement::NoNulls
            )
            .is_err()
        );
        assert!(vector::search_piecewise(&column.decoder, &mut session(), rows, 0, 10, 4).is_err());
        assert!(
            vector::fused_sum_between(
                &column.decoder,
                &mut session(),
                &column.decoder,
                &mut session(),
                rows,
                (0, 10),
                vector::CodeCompare::Offset
            )
            .is_err()
        );
    }
    assert!(
        vector::search_piecewise(
            &column.decoder,
            &mut session(),
            Span { start: 0, end: 0 },
            10,
            0,
            4
        )
        .is_err()
    );
}

#[test]
fn selected_sums_reject_malformed_bitmaps_without_panicking() {
    let column = encode(
        &Recipe::For(Box::new(Recipe::BitPack)),
        InputColumn::dense(vec![7; 8]),
    )
    .unwrap();
    for selection in [
        vector::Selection {
            rows: Span { start: 0, end: 8 },
            words: vec![],
        },
        vector::Selection {
            rows: Span { start: 0, end: 8 },
            words: vec![0, 1],
        },
        vector::Selection {
            rows: Span { start: 0, end: 8 },
            words: vec![1 << 8],
        },
        vector::Selection {
            rows: Span { start: 4, end: 3 },
            words: vec![],
        },
    ] {
        let mut session = ReadSession::new(&column.page, ClosureMode::Selective);
        assert!(vector::sum_selection(&column.decoder, &mut session, &selection).is_err());
    }
}

#[test]
fn dictionary_queries_below_the_first_entry_return_no_rows() {
    let column = encode(
        &Recipe::Dictionary(Box::new(Recipe::BitPack)),
        InputColumn::dense(vec![30, 10, 20, 10]),
    )
    .unwrap();
    let session = || ReadSession::new(&column.page, ClosureMode::Selective);
    let rows = Span::new(0, 4).unwrap();
    for (low, high) in [(i64::MIN, i64::MIN), (-10, 9)] {
        let selection =
            vector::dictionary_selection(&column.decoder, &mut session(), rows, low, high).unwrap();
        assert_eq!(selection.count(), 0);
        assert_eq!(
            vector::fused_sum_between(
                &column.decoder,
                &mut session(),
                &column.decoder,
                &mut session(),
                rows,
                (low, high),
                vector::CodeCompare::SortedDictionary,
            )
            .unwrap(),
            Some((0, 0))
        );
    }
    for rows in [Span { start: 2, end: 1 }, Span { start: 5, end: 5 }] {
        assert!(
            vector::dictionary_selection(&column.decoder, &mut session(), rows, 10, 20).is_err()
        );
        assert!(vector::dictionary_range(&column.decoder, &mut session(), rows, 10, 20).is_err());
    }
}
