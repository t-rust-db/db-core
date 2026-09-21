//! Seeded, bounded-depth, weighted-alternative walk over a [`Grammar`],
//! producing terminal-token strings for a chosen entry rule.

use std::collections::HashSet;

use crate::ebnf::{Alternation, Grammar, Rule, Section, Sequence, Term};

/// Deterministic xorshift64 PRNG -- no external `rand` dependency needed
/// for a seeded, reproducible walk.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // xorshift64 is undefined at seed 0.
        Rng(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Returns a value in `0..n`, or `0` if `n == 0`.
    pub fn gen_range(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let n_u64 = u64::try_from(n).unwrap_or(u64::MAX);
        usize::try_from(self.next_u64().checked_rem(n_u64).unwrap_or(0)).unwrap_or(0)
    }
}

/// Which V-block-tagged alternatives are in scope for a walk.
#[derive(Debug, Clone, Default)]
pub enum VBlockScope {
    /// Every alternative is in scope, tagged or not (`--include-future`).
    #[default]
    All,
    /// Untagged alternatives are always in scope; a tagged alternative is
    /// in scope only if at least one of its tags is in `landed`.
    Landed(HashSet<String>),
}

impl VBlockScope {
    fn allows(&self, tags: &[String]) -> bool {
        match self {
            VBlockScope::All => true,
            VBlockScope::Landed(landed) => {
                tags.is_empty() || tags.iter().any(|t| landed.contains(t))
            }
        }
    }
}

/// Terminal/rule substitution hook: lets a runner swap the grammar's
/// bare lexical productions (`table-name`, `column-name`, `identifier`,
/// `STRING`, `NUMBER`, `BLOB`, `type-name`, ...) for values drawn from a
/// real catalog, so generated statements name tables that exist and
/// carry typed literals instead of the grammar's informal prose
/// (db-core#545). Consulted for every rule reference before the grammar
/// itself; `None` falls through to the ordinary expansion.
pub trait Dialect {
    fn substitute(&mut self, rule: &str, rng: &mut Rng) -> Option<String>;
}

pub struct WalkerConfig {
    pub max_depth: usize,
    pub scope: VBlockScope,
    pub dialect: Option<Box<dyn Dialect>>,
}

impl std::fmt::Debug for WalkerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalkerConfig")
            .field("max_depth", &self.max_depth)
            .field("scope", &self.scope)
            .field("dialect", &self.dialect.as_ref().map(|_| "<dialect>"))
            .finish()
    }
}

impl Default for WalkerConfig {
    fn default() -> Self {
        WalkerConfig {
            max_depth: 16,
            scope: VBlockScope::default(),
            dialect: None,
        }
    }
}

#[derive(Debug)]
pub struct WalkError(pub String);

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "walk error: {}", self.0)
    }
}

impl std::error::Error for WalkError {}

/// One (rule, alternative-index) pair exercised during a walk, for
/// coverage accounting.
pub type CoverageKey = (String, usize);

pub struct Walker<'g> {
    grammar: &'g Grammar,
    section: Section,
    config: WalkerConfig,
    rng: Rng,
    visited: HashSet<CoverageKey>,
    /// Rules the dialect hook answered for, so their grammar alternatives
    /// were never walked; excluded from the coverage denominator.
    substituted: HashSet<String>,
    /// Entry rules `generate` has been called with; the denominator only
    /// counts alternatives reachable from these.
    entries: HashSet<String>,
    /// Minimum number of expansion steps needed to reach an all-terminal
    /// derivation for each rule name, computed once up front by fixpoint
    /// relaxation (`None` for a rule with no terminal-reachable
    /// alternative within scope at all, or for a name that isn't a rule
    /// -- an implicit primitive, always height 0). Used once the depth
    /// budget is exhausted, to pick the alternative that provably
    /// terminates soonest rather than merely guessing by shape.
    min_heights: std::collections::HashMap<String, usize>,
}

impl<'g> Walker<'g> {
    pub fn new(grammar: &'g Grammar, section: Section, seed: u64, config: WalkerConfig) -> Self {
        let min_heights = compute_min_heights(grammar, section, &config.scope);
        Walker {
            grammar,
            section,
            config,
            rng: Rng::new(seed),
            visited: HashSet::new(),
            substituted: HashSet::new(),
            entries: HashSet::new(),
            min_heights,
        }
    }

    /// Generates a whitespace-joined token string from `entry_rule`.
    pub fn generate(&mut self, entry_rule: &str) -> Result<String, WalkError> {
        let mut out = Vec::new();
        self.entries.insert(entry_rule.to_string());
        self.expand_rule(entry_rule, 0, &mut out)?;
        Ok(out.join(" "))
    }

    /// The precomputed minimum expansion-step height for `name`, or
    /// `None` if it has no terminal-reachable alternative in scope.
    pub fn min_height(&self, name: &str) -> Option<usize> {
        self.min_heights.get(name).copied()
    }

    /// `alternatives exercised / alternatives in scope`, where "in scope"
    /// means: allowed by the V-block scope, belonging to a rule reachable
    /// from an entry rule `generate` was called with, and not answered by
    /// the dialect hook (a substituted rule's grammar alternatives are
    /// by design never walked, so they are neither hits nor misses).
    pub fn coverage(&self) -> (usize, usize) {
        let denominator: usize = self
            .countable_rules()
            .map(|rule| self.in_scope_indices(rule).len())
            .sum();
        (self.visited.len(), denominator)
    }

    /// Every countable `(rule, alternative-index, alternative text)` this
    /// walker has never chosen, sorted by rule name -- the epic's
    /// "unexercised rules listed" line of the coverage report.
    pub fn unexercised(&self) -> Vec<(String, usize, String)> {
        let mut out: Vec<(String, usize, String)> = self
            .countable_rules()
            .flat_map(|rule| {
                self.in_scope_indices(rule)
                    .into_iter()
                    .filter(|i| !self.visited.contains(&(rule.name.clone(), *i)))
                    .map(|i| {
                        let text = rule
                            .alternatives
                            .get(i)
                            .map(|seq| render_sequence(seq))
                            .unwrap_or_default();
                        (rule.name.clone(), i, text)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        out.sort();
        out
    }

    /// Rules reachable from the entry rules, minus dialect-substituted
    /// ones. Falls back to every rule of the section when `generate` has
    /// not been called yet.
    fn countable_rules(&self) -> impl Iterator<Item = &'g Rule> + '_ {
        let reachable = self.reachable_rules();
        self.grammar
            .section(self.section)
            .values()
            .chain(self.grammar.shared.values())
            .filter(move |rule| {
                reachable
                    .as_ref()
                    .is_none_or(|set| set.contains(&rule.name))
                    && !self.substituted.contains(&rule.name)
            })
    }

    /// Rule names reachable from the entry rules through in-scope
    /// alternatives, not descending into substituted rules. `None` when
    /// no entry rule is known yet.
    fn reachable_rules(&self) -> Option<HashSet<String>> {
        if self.entries.is_empty() {
            return None;
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = self.entries.iter().cloned().collect();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) || self.substituted.contains(&name) {
                continue;
            }
            let Some(rule) = self.grammar.resolve(self.section, &name) else {
                continue;
            };
            for i in self.in_scope_indices(rule) {
                if let Some(seq) = rule.alternatives.get(i) {
                    collect_references(seq, &mut stack);
                }
            }
        }
        Some(seen)
    }

    fn in_scope_indices(&self, rule: &Rule) -> Vec<usize> {
        rule.v_tags
            .iter()
            .enumerate()
            .filter(|(_, tags)| self.config.scope.allows(tags))
            .map(|(i, _)| i)
            .collect()
    }

    fn expand_rule(
        &mut self,
        name: &str,
        depth: usize,
        out: &mut Vec<String>,
    ) -> Result<(), WalkError> {
        // `max_depth` only biases alternative choice toward termination; a
        // grammar with a rule that has no zero-complexity (terminal-only)
        // base case at all would otherwise recurse forever. This hard cap
        // turns that into a typed error instead of a stack overflow --
        // the totality property this crate exists to test starts with
        // itself.
        if depth > self.config.max_depth.saturating_mul(4).saturating_add(64) {
            return Err(WalkError(format!(
                "depth limit exceeded expanding '{name}': grammar may have no terminal base case reachable from here"
            )));
        }
        if let Some(dialect) = self.config.dialect.as_mut() {
            if let Some(text) = dialect.substitute(name, &mut self.rng) {
                self.substituted.insert(name.to_string());
                out.push(text);
                return Ok(());
            }
        }
        let Some(rule) = self.grammar.resolve(self.section, name) else {
            // Not a defined rule: treat as an implicit tokenizer primitive
            // (ALPHA, DIGIT, IDENT, INTEGER, ...).
            out.push(primitive_terminal(name, &mut self.rng));
            return Ok(());
        };

        let in_scope = self.in_scope_indices(rule);
        if in_scope.is_empty() {
            return Err(WalkError(format!(
                "rule '{name}' has no in-scope alternatives under the current V-block scope"
            )));
        }

        // Once the depth budget is exhausted, pick the alternative with
        // the smallest precomputed min-height rather than guessing by
        // shape -- guarantees termination within `min_heights[name]` more
        // steps whenever the rule has any terminal-reachable derivation
        // in scope at all.
        let chosen_idx = if depth >= self.config.max_depth {
            in_scope
                .iter()
                .copied()
                .min_by_key(|&i| {
                    rule.alternatives
                        .get(i)
                        .and_then(|seq| {
                            sequence_min_height(seq, &self.min_heights, self.grammar, self.section)
                        })
                        .unwrap_or(usize::MAX)
                })
                .unwrap_or(0)
        } else {
            let pick = self.rng.gen_range(in_scope.len());
            in_scope.get(pick).copied().unwrap_or(0)
        };

        self.visited.insert((name.to_string(), chosen_idx));

        let Some(sequence) = rule.alternatives.get(chosen_idx) else {
            return Err(WalkError(format!(
                "internal: alternative {chosen_idx} missing for rule '{name}'"
            )));
        };
        let sequence = sequence.clone();
        for term in &sequence {
            self.expand_term(term, depth.saturating_add(1), out)?;
        }
        Ok(())
    }

    fn expand_term(
        &mut self,
        term: &Term,
        depth: usize,
        out: &mut Vec<String>,
    ) -> Result<(), WalkError> {
        match term {
            Term::Terminal(text) => {
                out.push(text.clone());
                Ok(())
            }
            Term::NonTerminal(name) => self.expand_rule(name, depth, out),
            Term::Group(alts) => self.expand_alternation(alts, depth, out),
            Term::Optional(alts) => {
                // 50/50 include vs. skip while inside the depth budget,
                // but always skip past it -- skipping is always a valid
                // (and here, the terminal-safe) choice for an optional.
                if depth >= self.config.max_depth || self.rng.gen_range(2) == 0 {
                    return Ok(());
                }
                self.expand_alternation(alts, depth, out)
            }
            Term::Repetition(alts) => {
                if depth >= self.config.max_depth {
                    return Ok(());
                }
                // Geometric-ish repeat count, capped to keep output bounded
                // -- a low continue chance (1 in 3) matters more than it
                // looks: each repeated element can itself contain further
                // repetitions/subqueries, so output size compounds fast.
                let max_reps: usize = 2;
                let mut reps: usize = 0;
                while reps < max_reps && self.rng.gen_range(3) == 0 {
                    self.expand_alternation(alts, depth.saturating_add(1), out)?;
                    reps = reps.saturating_add(1);
                }
                Ok(())
            }
        }
    }

    fn expand_alternation(
        &mut self,
        alts: &Alternation,
        depth: usize,
        out: &mut Vec<String>,
    ) -> Result<(), WalkError> {
        if alts.is_empty() {
            return Ok(());
        }
        let idx = self.rng.gen_range(alts.len());
        let Some(sequence) = alts.get(idx) else {
            return Err(WalkError(
                "internal: alternation index out of range".to_string(),
            ));
        };
        let sequence = sequence.clone();
        for term in &sequence {
            self.expand_term(term, depth, out)?;
        }
        Ok(())
    }
}

/// Pushes every rule name referenced anywhere in `seq`.
fn collect_references(seq: &[Term], out: &mut Vec<String>) {
    for term in seq {
        match term {
            Term::Terminal(_) => {}
            Term::NonTerminal(name) => out.push(name.clone()),
            Term::Group(alts) | Term::Optional(alts) | Term::Repetition(alts) => {
                for s in alts {
                    collect_references(s, out);
                }
            }
        }
    }
}

/// One alternative as EBNF-ish text, for the unexercised report.
fn render_sequence(seq: &[Term]) -> String {
    seq.iter()
        .map(|t| match t {
            Term::Terminal(text) => format!("\"{text}\""),
            Term::NonTerminal(name) => name.clone(),
            Term::Group(alts) => format!("( {} )", render_alternation(alts)),
            Term::Optional(alts) => format!("[ {} ]", render_alternation(alts)),
            Term::Repetition(alts) => format!("{{ {} }}", render_alternation(alts)),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_alternation(alts: &Alternation) -> String {
    alts.iter()
        .map(|seq| render_sequence(seq))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Steps needed to reach an all-terminal derivation of `seq`, given each
/// referenced rule's already-known minimum height. `Optional` and
/// `Repetition` always contribute 0 (both can always resolve to nothing);
/// only `Group` and a direct `NonTerminal` reference require descending.
/// Returns `None` if any required reference has no known height yet.
fn sequence_min_height(
    seq: &[Term],
    heights: &std::collections::HashMap<String, usize>,
    grammar: &Grammar,
    section: Section,
) -> Option<usize> {
    let mut total = 0usize;
    for term in seq {
        let h = match term {
            Term::Terminal(_) => 0,
            Term::NonTerminal(name) => {
                if grammar.resolve(section, name).is_some() {
                    *heights.get(name)?
                } else {
                    0 // an implicit tokenizer primitive: always terminal.
                }
            }
            Term::Group(alts) => alternation_min_height(alts, heights, grammar, section)?,
            Term::Optional(_) | Term::Repetition(_) => 0,
        };
        total = total.saturating_add(h);
    }
    Some(total.saturating_add(1))
}

fn alternation_min_height(
    alts: &Alternation,
    heights: &std::collections::HashMap<String, usize>,
    grammar: &Grammar,
    section: Section,
) -> Option<usize> {
    alts.iter()
        .filter_map(|seq| sequence_min_height(seq, heights, grammar, section))
        .min()
}

/// Fixpoint relaxation over every in-scope rule of `section` (+ SHARED):
/// repeatedly tries to compute each still-unknown rule's minimum height
/// from what's already known, until a full pass makes no more progress.
/// Terminates because heights only ever decrease into a finite range.
fn compute_min_heights(
    grammar: &Grammar,
    section: Section,
    scope: &VBlockScope,
) -> std::collections::HashMap<String, usize> {
    let rules: Vec<&Rule> = grammar
        .section(section)
        .values()
        .chain(grammar.shared.values())
        .collect();
    let mut heights: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    loop {
        let mut changed = false;
        for rule in &rules {
            let in_scope_alts: Vec<&Sequence> = rule
                .v_tags
                .iter()
                .enumerate()
                .filter(|(_, tags)| scope.allows(tags))
                .filter_map(|(i, _)| rule.alternatives.get(i))
                .collect();
            let Some(best) = in_scope_alts
                .iter()
                .filter_map(|seq| sequence_min_height(seq, &heights, grammar, section))
                .min()
            else {
                continue;
            };
            let entry = heights.entry(rule.name.clone()).or_insert(usize::MAX);
            if best < *entry {
                *entry = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    heights
}

fn primitive_terminal(name: &str, rng: &mut Rng) -> String {
    match name {
        "ALPHA" => {
            let letters = "abcdefghijklmnopqrstuvwxyz";
            let idx = rng.gen_range(letters.len());
            letters.chars().nth(idx).unwrap_or('x').to_string()
        }
        "DIGIT" => rng.gen_range(10).to_string(),
        "INTEGER" => rng.gen_range(1000).to_string(),
        "IDENT" => {
            let letters = "abcdefghijklmnopqrstuvwxyz";
            let len = 1usize.saturating_add(rng.gen_range(6));
            (0..len)
                .map(|_| {
                    let idx = rng.gen_range(letters.len());
                    letters.chars().nth(idx).unwrap_or('x')
                })
                .collect()
        }
        // `STRING`/`quoted-identifier` (sqlite-rs section) spell their
        // body content out as informal prose -- `{ any character except
        // "'" }` -- rather than a real production. "any"/"character"
        // stand in for a single arbitrary char; "except" is a connective
        // that precedes the excluded literal(s), not content of its own.
        "any" | "character" => {
            let letters = "abcdefghijklmnopqrstuvwxyz";
            let idx = rng.gen_range(letters.len());
            letters.chars().nth(idx).unwrap_or('x').to_string()
        }
        "except" => String::new(),
        other => other.to_string(),
    }
}
