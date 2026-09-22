//! Statement-level delta debugging (ddmin) over a repro script: the
//! smallest subsequence of statements for which `still_fails` holds.
//! Works on the engine history the worker already keeps, so it needs no
//! AST printer; shrinking inside a statement is a follow-up.

/// Classic ddmin. `still_fails` is called on candidate subsequences and
/// must be deterministic; `max_probes` bounds the number of calls.
/// Returns the reduced script and how many probes were spent.
pub fn ddmin<F>(script: &[String], mut still_fails: F, max_probes: usize) -> (Vec<String>, usize)
where
    F: FnMut(&[String]) -> bool,
{
    let mut current: Vec<String> = script.to_vec();
    let mut probes = 0usize;
    let mut n = 2usize;
    while current.len() >= 2 && probes < max_probes {
        let chunk = current.len().div_ceil(n);
        let mut reduced = false;
        let mut start = 0usize;
        while start < current.len() {
            let end = start.saturating_add(chunk).min(current.len());
            // Complement of this chunk.
            let candidate: Vec<String> = current
                .iter()
                .enumerate()
                .filter(|(i, _)| *i < start || *i >= end)
                .map(|(_, s)| s.clone())
                .collect();
            probes = probes.saturating_add(1);
            if !candidate.is_empty() && still_fails(&candidate) {
                current = candidate;
                n = n.saturating_sub(1).max(2);
                reduced = true;
                break;
            }
            if probes >= max_probes {
                break;
            }
            start = end;
        }
        if !reduced {
            if n >= current.len() {
                break;
            }
            n = n.saturating_mul(2).min(current.len());
        }
    }
    (current, probes)
}
