use witness::access_compiler::{
    AccessInvariant, AccessSet, Answer, ClosureMode, EncodedColumn, FactScope, FieldId,
    FieldLocation, FrameCodec, FrameId, FrameLayout, InputColumn, LayoutIr, Property, Query,
    Recipe, SerializedPage, Span, ValueInvariant, derive_invariants, encode, execute_full_decode,
    execute_interpreted,
};

const ROWS: usize = 65_536;
const CHUNK: usize = 4_096;

fn timestamps() -> Vec<i64> {
    // Non-decreasing with irregular steps, so delta codes are not constant
    // and each chunk compresses to a different size.
    (0..ROWS as i64)
        .scan(1_700_000_000_i64, |value, row| {
            *value += 60 + (row * 7919 % 13);
            Some(*value)
        })
        .collect()
}

fn delta() -> Recipe {
    Recipe::UnsignedDelta {
        restart_interval: 256,
        deltas: Box::new(Recipe::BitPack),
    }
}

fn chunked(values: Recipe) -> Recipe {
    Recipe::ChunkedFrame {
        chunk_bytes: CHUNK,
        values: Box::new(values),
    }
}

fn whole(values: Recipe) -> Recipe {
    Recipe::Frame(Box::new(values))
}

fn column(recipe: &Recipe) -> EncodedColumn {
    encode(recipe, InputColumn::dense(timestamps())).unwrap()
}

#[test]
fn chunked_frames_return_exact_answers() {
    let column = column(&chunked(delta()));
    assert!(column.page.layout().frames.len() > 1);
    assert!(
        column.page.layout().frames[1..]
            .iter()
            .all(|frame| frame.continues)
    );
    for row in [0, 1, 255, 256, CHUNK, ROWS / 2 + 7, ROWS - 1] {
        let get =
            execute_interpreted(&column, &Query::Get { row }, ClosureMode::Selective).unwrap();
        assert_eq!(get.answer, Answer::Value(Some(column.truth[row].unwrap())));
    }
    let rows = Span::new(ROWS / 3, ROWS / 3 + 1_000).unwrap();
    let expected: i128 = column.truth[rows.start..rows.end]
        .iter()
        .flatten()
        .map(|&value| i128::from(value))
        .sum();
    let sum = execute_interpreted(&column, &Query::Sum { rows }, ClosureMode::Selective).unwrap();
    assert_eq!(sum.answer, Answer::Sum(expected));
    let full = execute_full_decode(&column, &Query::Sum { rows }, ClosureMode::Selective).unwrap();
    assert_eq!(full.answer, sum.answer);
}

#[test]
fn chunked_frames_deliver_only_the_chunks_a_read_spans() {
    let framed = column(&whole(delta()));
    let chunked = column(&chunked(delta()));
    assert_eq!(framed.truth, chunked.truth);
    let query = Query::Get { row: ROWS / 2 };
    let whole_read = execute_interpreted(&framed, &query, ClosureMode::Selective).unwrap();
    let chunk_read = execute_interpreted(&chunked, &query, ClosureMode::Selective).unwrap();
    assert_eq!(whole_read.answer, chunk_read.answer);
    assert_eq!(whole_read.metrics.frames_decoded, 1);
    assert!(chunk_read.metrics.frames_decoded >= 1);
    assert!(chunk_read.metrics.frames_decoded < chunked.page.layout().frames.len());
    assert!(chunk_read.metrics.delivered_bytes < whole_read.metrics.delivered_bytes);

    // The declared closure agrees: a short span names a strict subset of the
    // compressed chunks, while the unchunked frame is delivered whole.
    let field = data_field(&chunked);
    let mut seed = AccessSet::default();
    seed.insert(field, Span::new(100, 200).unwrap());
    let closure = chunked.page.layout().closure(&seed).unwrap();
    let all_chunks: usize = chunked
        .page
        .layout()
        .frames
        .iter()
        .map(|frame| frame.compressed_length)
        .sum();
    assert!(closure.delivered_bytes < all_chunks);
}

#[test]
fn a_corrupt_chunk_fails_only_reads_that_span_it() {
    let column = column(&chunked(delta()));
    let layout = column.page.layout();
    let last = layout.frames.last().unwrap().clone();
    let mut bytes = column.page.bytes().to_vec();
    bytes[last.offset..last.offset + last.compressed_length].fill(0);
    let corrupt = EncodedColumn {
        recipe: column.recipe.clone(),
        decoder: column.decoder.clone(),
        page: SerializedPage::parse(bytes).unwrap(),
        truth: column.truth.clone(),
    };
    let early =
        execute_interpreted(&corrupt, &Query::Get { row: 3 }, ClosureMode::Selective).unwrap();
    assert_eq!(early.answer, Answer::Value(Some(column.truth[3].unwrap())));
    let late = execute_interpreted(
        &corrupt,
        &Query::Get { row: ROWS - 1 },
        ClosureMode::Selective,
    );
    assert!(late.is_err());
}

#[test]
fn chunking_keeps_value_facts_and_adds_a_chunk_fact() {
    let single = Recipe::UnsignedDelta {
        restart_interval: ROWS,
        deltas: Box::new(Recipe::BitPack),
    };
    for (recipe, chunks) in [(whole(single.clone()), false), (chunked(single), true)] {
        let column = column(&recipe);
        let facts = derive_invariants(
            &column.decoder,
            column.page.layout(),
            column.page.invariants(),
        )
        .unwrap();
        let root = column.decoder.root();
        assert!(facts.contains(
            root,
            FactScope::AllRows,
            Property::Value(ValueInvariant::NonDecreasing)
        ));
        assert!(facts.iter().any(|fact| matches!(
            fact.property,
            Property::Access(AccessInvariant::EnclosingFrame { .. })
        )));
        assert_eq!(
            facts.iter().any(|fact| matches!(
                fact.property,
                Property::Access(AccessInvariant::FrameChunks {
                    decoded_bytes: CHUNK
                })
            )),
            chunks,
            "{}",
            recipe.name()
        );
    }
}

#[test]
fn malformed_chunk_groups_are_rejected() {
    let frame = |id, decoded_length, continues| FrameLayout {
        id: FrameId(id),
        codec: FrameCodec::Zstd,
        offset: 64 * (id + 1),
        compressed_length: 16,
        decoded_length,
        alignment: 64,
        continues,
    };
    let layout = |frames: Vec<FrameLayout>, frame: usize| LayoutIr {
        metadata: FieldId(0),
        fields: vec![
            witness::access_compiler::FieldLayout {
                id: FieldId(0),
                name: "metadata".into(),
                length: 64,
                alignment: 64,
                read_granularity: 64,
                location: FieldLocation::Direct { offset: 0 },
            },
            witness::access_compiler::FieldLayout {
                id: FieldId(1),
                name: "data".into(),
                length: 150,
                alignment: 1,
                read_granularity: 1,
                location: FieldLocation::Framed {
                    frame: FrameId(frame),
                    decoded_offset: 0,
                },
            },
        ],
        frames,
        dependencies: Vec::new(),
        file_length: 64 * 8,
    };
    let valid = layout(
        vec![frame(0, 64, false), frame(1, 64, true), frame(2, 22, true)],
        0,
    );
    assert!(valid.validate().is_ok());
    // A middle chunk shorter than the head breaks chunk arithmetic.
    let uneven = layout(
        vec![frame(0, 64, false), frame(1, 30, true), frame(2, 56, true)],
        0,
    );
    assert!(uneven.validate().is_err());
    // The first frame cannot continue anything.
    let headless = layout(vec![frame(0, 64, true), frame(1, 86, true)], 0);
    assert!(headless.validate().is_err());
    // A field must name the head of its group, not a continuation.
    let inner = layout(
        vec![frame(0, 64, false), frame(1, 64, true), frame(2, 64, true)],
        1,
    );
    assert!(inner.validate().is_err());
    // A field longer than its whole group is rejected.
    let short = layout(vec![frame(0, 64, false), frame(1, 64, true)], 0);
    assert!(short.validate().is_err());
}

fn data_field(column: &EncodedColumn) -> FieldId {
    column
        .page
        .layout()
        .fields
        .iter()
        .find(|field| {
            matches!(field.location, FieldLocation::Framed { .. }) && field.length > 1_000
        })
        .unwrap()
        .id
}
