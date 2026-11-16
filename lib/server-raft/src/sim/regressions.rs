//! Seeds that once failed (plan 0028, "Two tiers of seeds").
//!
//! A seed that fails in either tier is fixed, then added here with a note
//! naming the bug. The default tier runs these seeds on every test run, so
//! a fixed bug stays fixed.

/// `(seed, what it found)`.
pub const REGRESSIONS: &[(u64, &str)] = &[(
    976_989_917_238,
    "plan 0029: an installed snapshot became durable before the queued truncation of the \
     entries it replaced; after a crash openraft refused to start (inverted log order)",
)];
