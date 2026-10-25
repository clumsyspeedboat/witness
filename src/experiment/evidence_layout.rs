//! Experimental compression placement, using the existing recipe language.
//!
//! Framing a complete composition also frames its bases, dictionaries, and
//! indexes. Framing only its packed streams leaves those fields directly
//! readable. The decoder and its logical facts are otherwise unchanged.

use crate::access_compiler::Recipe;

#[derive(Clone, Copy, Debug)]
pub enum Compression {
    Whole,
    Chunked { bytes: usize },
}

impl Compression {
    pub fn wrap(self, recipe: Recipe) -> Result<Recipe, String> {
        match self {
            Self::Whole => Ok(Recipe::Frame(Box::new(recipe))),
            Self::Chunked { bytes: 0 } => Err("frame chunks must be positive".into()),
            Self::Chunked { bytes } => Ok(Recipe::ChunkedFrame {
                chunk_bytes: bytes,
                values: Box::new(recipe),
            }),
        }
    }
}

/// Compresses packed streams, leaving every other codec's fields outside
/// the frames, including nullable validity/rank and run/restart indexes.
/// Already-framed inputs are rejected rather than silently double-framed.
pub fn compress_payload(recipe: &Recipe, compression: Compression) -> Result<Recipe, String> {
    let child = |recipe| compress_payload(recipe, compression).map(Box::new);
    Ok(match recipe {
        Recipe::BitPack => return compression.wrap(Recipe::BitPack),
        Recipe::For(values) => Recipe::For(child(values)?),
        Recipe::Dictionary(values) => Recipe::Dictionary(child(values)?),
        Recipe::Rle {
            index_interval,
            values,
        } => Recipe::Rle {
            index_interval: *index_interval,
            values: child(values)?,
        },
        Recipe::Delta {
            restart_interval,
            deltas,
        } => Recipe::Delta {
            restart_interval: *restart_interval,
            deltas: child(deltas)?,
        },
        Recipe::UnsignedDelta {
            restart_interval,
            deltas,
        } => Recipe::UnsignedDelta {
            restart_interval: *restart_interval,
            deltas: child(deltas)?,
        },
        Recipe::Patch {
            index_interval,
            values,
        } => Recipe::Patch {
            index_interval: *index_interval,
            values: child(values)?,
        },
        Recipe::Nullable {
            rank_interval,
            values,
        } => Recipe::Nullable {
            rank_interval: *rank_interval,
            values: child(values)?,
        },
        Recipe::Frame(_) | Recipe::ChunkedFrame { .. } => {
            return Err("payload compression needs an unframed recipe".into());
        }
    })
}

/// Keeps the uncompressed candidates as fallbacks. PagePolicy::smallest
/// charges complete serialized bytes, including frame descriptors/padding.
pub fn compression_menu(
    recipes: &[Recipe],
    compression: Compression,
    payload_only: bool,
) -> Result<Vec<Recipe>, String> {
    let mut menu = recipes.to_vec();
    for recipe in recipes {
        menu.push(if payload_only {
            compress_payload(recipe, compression)?
        } else {
            compression.wrap(recipe.clone())?
        });
    }
    Ok(menu)
}
