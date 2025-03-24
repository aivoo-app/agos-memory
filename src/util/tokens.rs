//! Token accounting (D9).
//!
//! Recall packs memory into a fixed token budget per answer. Exact tokenization
//! requires the model tokenizer; v0.1.0 ships a conservative heuristic that is
//! measured against real token counts by the eval harness. The 1.15 safety
//! factor exists because the heuristic must overestimate, never underestimate:
//! an underestimate would inject too much context and silently break the
//! budget guarantee.

/// Estimates token counts for prompt packing.
pub trait TokenCounter: Send + Sync {
    /// Estimated token count for a piece of text.
    fn count(&self, text: &str) -> u64;
}

/// Heuristic counter: `ceil(chars / 4) * 1.15` with a floor of 1 token.
///
/// Calibrated for English; the eval harness records drift against real
/// tokenizer counts and the factor is meant to be tuned there (R7).
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicCounter {
    /// Safety multiplier; must be >= 1.0.
    pub factor: f64,
}

impl HeuristicCounter {
    /// Default counter with the 1.15 safety factor.
    pub fn new() -> Self {
        Self { factor: 1.15 }
    }
}

impl TokenCounter for HeuristicCounter {
    fn count(&self, text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        let base = (text.chars().count() as f64 / 4.0).ceil();
        ((base * self.factor).ceil() as u64).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_zero() {
        assert_eq!(HeuristicCounter::new().count(""), 0);
    }

    #[test]
    fn never_underestimates_one_token() {
        // With the 1.15 safety factor, even a single char rounds up to 2.
        assert_eq!(HeuristicCounter::new().count("a"), 2);
        assert_eq!(HeuristicCounter::new().count("word"), 2);
    }

    #[test]
    fn scales_with_length_and_overestimates() {
        // 800 chars -> base 200 -> *1.15 = 230
        let text = "x".repeat(800);
        assert_eq!(HeuristicCounter::new().count(&text), 230);
    }

    #[test]
    fn multibyte_chars_counted_per_char() {
        // Bengali text: chars, not bytes.
        let text = "এটি একটি পরীক্ষা"; // 17 chars incl. spaces
        let n = HeuristicCounter::new().count(text);
        assert!((4..=8).contains(&n), "got {n}");
    }
}
