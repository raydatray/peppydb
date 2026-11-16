//! Seeds that once failed (plan 0028, "Two tiers of seeds").
//!
//! A seed that fails in either tier is fixed, then added here with a note
//! naming the bug. The default tier runs these seeds on every test run, so
//! a fixed bug stays fixed.

/// `(seed, what it found)`.
pub const REGRESSIONS: &[(u64, &str)] = &[];
