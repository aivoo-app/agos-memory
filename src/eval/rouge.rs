//! ROUGE-L summarization quality (issue 0049).
//!
//! ROUGE-L scores a generated summary against a reference by the longest
//! common subsequence (LCS) of their word tokens — order-sensitive, so it
//! rewards summaries that preserve the reference's sentence structure, not
//! just its vocabulary.
//!
//! With `P = LCS / |candidate|`, `R = LCS / |reference|`, the score is the
//! harmonic mean (F1, β = 1):
//!
//! ```text
//! ROUGE-L = 2·P·R / (P + R)
//! ```
//!
//! Tokenization is deterministic and case/punctuation-insensitive: lowercase,
//! split on non-alphanumeric characters, empty tokens dropped.

/// Word tokens: lowercased alphanumeric runs, in order.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Length of the longest common subsequence between two token sequences.
///
/// O(n·m) time, O(min(n, m)) space (row-reduced DP).
pub fn lcs_len(a: &[String], b: &[String]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    // Keep the shorter sequence in the DP row to bound memory.
    let (row, col) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut prev = vec![0usize; row.len() + 1];
    let mut curr = vec![0usize; row.len() + 1];
    for cb in col {
        for (j, cr) in row.iter().enumerate() {
            curr[j + 1] = if cb == cr {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
        curr.iter_mut().for_each(|v| *v = 0);
    }
    prev[row.len()]
}

/// ROUGE-L F1 (β = 1) of `candidate` against `reference`.
///
/// Returns 0.0 when either side tokenizes to nothing (an empty summary must
/// never score as a perfect match).
pub fn rouge_l(reference: &str, candidate: &str) -> f64 {
    let r = tokenize(reference);
    let c = tokenize(candidate);
    if r.is_empty() || c.is_empty() {
        return 0.0;
    }
    let lcs = lcs_len(&r, &c) as f64;
    let precision = lcs / c.len() as f64;
    let recall = lcs / r.len() as f64;
    if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_and_normalizes() {
        assert_eq!(
            tokenize("Rust is a Memory-Safe language!"),
            vec!["rust", "is", "a", "memory", "safe", "language"]
        );
        assert!(tokenize("!!! ...").is_empty());
    }

    #[test]
    fn lcs_identical() {
        let v = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(lcs_len(&v, &v), 3);
    }

    #[test]
    fn lcs_disjoint() {
        let a = vec!["a".to_string(), "b".to_string()];
        let b = vec!["x".to_string(), "y".to_string()];
        assert_eq!(lcs_len(&a, &b), 0);
    }

    #[test]
    fn lcs_subsequence_not_substring() {
        // LCS of "the quick brown fox" / "the lazy brown dog" is 2
        // ("the", "brown" — order-sensitive, not a substring match).
        let a = tokenize("the quick brown fox");
        let b = tokenize("the lazy brown dog");
        assert_eq!(lcs_len(&a, &b), 2);
    }

    #[test]
    fn rouge_identical_is_one() {
        let s = "Rust prevents data races at compile time.";
        assert!((rouge_l(s, s) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rouge_disjoint_is_zero() {
        assert!((rouge_l("alpha beta", "gamma delta") - 0.0).abs() < 1e-9);
    }

    #[test]
    fn rouge_partial_known_value() {
        // reference = "the quick brown fox", candidate = "the brown fox"
        // LCS = 3 → P = 3/3, R = 3/4 → F1 = 2·(3/3)(3/4)/((3/3)+(3/4)) = 6/7.
        let score = rouge_l("the quick brown fox", "the brown fox");
        assert!((score - 6.0 / 7.0).abs() < 1e-9, "got {score}");
    }

    #[test]
    fn rouge_empty_inputs_are_zero() {
        assert_eq!(rouge_l("", "anything"), 0.0);
        assert_eq!(rouge_l("anything", ""), 0.0);
        assert_eq!(rouge_l("", ""), 0.0);
        assert_eq!(rouge_l("!!!", "..."), 0.0);
    }

    #[test]
    fn rouge_is_case_and_punctuation_insensitive() {
        let a = rouge_l("Rust, is fast!", "rust is fast");
        assert!((a - 1.0).abs() < 1e-9);
    }
}
