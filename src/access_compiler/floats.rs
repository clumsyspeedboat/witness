//! Floating-point columns as integer keys that sort like the floats.
//!
//! A double's bits, read as a sign-magnitude integer, sort correctly once
//! negative values have all their bits flipped and non-negative values their
//! sign bit set; shifting the result into `i64` keeps the order. The mapping
//! is a bijection on non-NaN doubles once `-0.0` is folded into `0.0`, so
//! every fact the integer codecs prove about keys---a bound, order, a constant
//! page---is the same fact about the floats, and a range of floats is a range
//! of keys. NaN has no place in the order and is stored as null.

/// The key of `value`, or `None` for NaN.
pub fn key(value: f64) -> Option<i64> {
    if value.is_nan() {
        return None;
    }
    // -0.0 and 0.0 compare equal, so they share a key.
    let bits = if value == 0.0 { 0 } else { value.to_bits() };
    let ordered = if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    };
    Some((ordered ^ (1 << 63)) as i64)
}

/// The double whose key is `key`.
pub fn from_key(key: i64) -> f64 {
    let ordered = (key as u64) ^ (1 << 63);
    let bits = if ordered >> 63 == 1 {
        ordered & !(1 << 63)
    } else {
        !ordered
    };
    f64::from_bits(bits)
}

/// The keys of the doubles in `low..=high`, or `None` when the range holds
/// no double (or a bound is NaN).
pub fn key_range(low: f64, high: f64) -> Option<(i64, i64)> {
    let (first, last) = (key(low)?, key(high)?);
    (first <= last).then_some((first, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sort_like_their_doubles_and_round_trip() {
        let values = vec![
            f64::NEG_INFINITY,
            -f64::MAX,
            -1.5,
            -f64::MIN_POSITIVE,
            -5e-324,
            0.0,
            5e-324,
            f64::MIN_POSITIVE,
            0.1,
            1.0,
            1.0 + f64::EPSILON,
            f64::MAX,
            f64::INFINITY,
        ];
        let keys = values
            .iter()
            .map(|&value| key(value).unwrap())
            .collect::<Vec<_>>();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        for (&value, &key) in values.iter().zip(&keys) {
            assert_eq!(from_key(key).to_bits(), value.to_bits());
        }
        assert_eq!(key(-0.0), key(0.0));
        assert_eq!(key(f64::NAN), None);
        assert_eq!(key_range(1.0, 0.5), None);
        assert_eq!(
            key_range(-0.0, 0.0),
            Some((key(0.0).unwrap(), key(0.0).unwrap()))
        );
    }

    #[test]
    fn random_doubles_compare_as_their_keys_do() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            f64::from_bits(state)
        };
        for _ in 0..100_000 {
            let (left, right) = (next(), next());
            if left.is_nan() || right.is_nan() {
                continue;
            }
            let (left_key, right_key) = (key(left).unwrap(), key(right).unwrap());
            assert_eq!(left.partial_cmp(&right), Some(left_key.cmp(&right_key)));
            assert_eq!(from_key(left_key), left);
        }
    }
}
