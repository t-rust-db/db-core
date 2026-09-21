//! Hand-rolled recursive-descent parser for the informal EBNF dialect used
//! by `src/parser/grammar.ebnf`: `::=`, `|`, `[ ]`, `{ }`, `( )`, `"..."`
//! terminals, `(* ... *)` comments, and trailing `(* Vn *)` tags on
//! sqlite-rs-section alternatives. Not strict ISO 14977 (no `?special
//! sequence?`), and treats uppercase bare identifiers (`ALPHA`, `DIGIT`,
//! `IDENT`, `INTEGER`) as primitive terminals never defined by a rule.

use std::collections::BTreeMap;

/// A single grammar section: the file's own SHARED / COLUMN-RS ONLY /
/// SQLITE-RS ONLY split. Rule names repeat across sections with different
/// bodies (both define `sql-stmt`, `expr`, ...), so sections are kept as
/// separate rule maps rather than one flat one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Shared,
    Column,
    Sqlite,
}

pub type Sequence = Vec<Term>;
/// An alternation is a list of alternative sequences (`seq1 | seq2 | ...`).
pub type Alternation = Vec<Sequence>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// A quoted literal keyword/punctuation, e.g. `"SELECT"`.
    Terminal(String),
    /// A bare identifier: either another rule's name, or (if the grammar
    /// has no rule by that name) an implicit tokenizer primitive.
    NonTerminal(String),
    Group(Alternation),
    Optional(Alternation),
    Repetition(Alternation),
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub alternatives: Alternation,
    /// V-block tags per top-level alternative, aligned by index with
    /// `alternatives`. Empty for alternatives with no `(* Vn *)` comment.
    pub v_tags: Vec<Vec<String>>,
}

#[derive(Debug, Default)]
pub struct Grammar {
    pub shared: BTreeMap<String, Rule>,
    pub column: BTreeMap<String, Rule>,
    pub sqlite: BTreeMap<String, Rule>,
}

impl Grammar {
    pub fn section(&self, section: Section) -> &BTreeMap<String, Rule> {
        match section {
            Section::Shared => &self.shared,
            Section::Column => &self.column,
            Section::Sqlite => &self.sqlite,
        }
    }

    /// Looks up `name` in `section` first, then falls back to `shared`
    /// (SHARED rules are referenced by name from both other sections).
    pub fn resolve<'g>(&'g self, section: Section, name: &str) -> Option<&'g Rule> {
        self.section(section)
            .get(name)
            .or_else(|| self.shared.get(name))
    }

    /// Total count of (rule, alternative) pairs in `section`, plus SHARED.
    pub fn alternative_count(&self, section: Section) -> usize {
        self.section(section)
            .values()
            .chain(self.shared.values())
            .map(|r| r.alternatives.len())
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Assign,
    Semi,
    Pipe,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Ident(String),
    Str(String),
    Comment(String),
}

#[derive(Debug)]
pub struct GrammarError(pub String);

impl std::fmt::Display for GrammarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "grammar error: {}", self.0)
    }
}

impl std::error::Error for GrammarError {}

fn err(msg: impl Into<String>) -> GrammarError {
    GrammarError(msg.into())
}

fn tokenize(src: &str) -> Result<Vec<Token>, GrammarError> {
    let mut tokens = Vec::new();
    let mut chars = src.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        match c {
            '(' => {
                chars.next();
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // The prose in these comments occasionally contains a
                    // literal "(*"/"*)"-shaped substring that is not a
                    // real delimiter: `COUNT(*)`/`count(*)` (glued, no
                    // spaces), a `src/row/*)` path glob, or a
                    // backtick-quoted example like `` `(* V2 *)` ``.
                    // Every genuine closing "*)" in this file both (a) is
                    // NOT immediately preceded by "(" (i.e. not a glued
                    // "(*)") and (b) is followed by whitespace or
                    // end-of-input, not more prose/punctuation. Every
                    // false instance found so far fails at least one of
                    // the two checks.
                    let mut content = String::new();
                    let mut prev: Option<char> = None;
                    loop {
                        match chars.next() {
                            Some('*') if chars.peek() == Some(&')') && prev != Some('(') => {
                                let mut probe = chars.clone();
                                probe.next(); // the ')' we're about to consume
                                let followed_by_boundary =
                                    probe.peek().is_none() || probe.peek().is_some_and(|c| c.is_whitespace());
                                if followed_by_boundary {
                                    chars.next();
                                    break;
                                }
                                content.push('*');
                                prev = Some('*');
                            }
                            Some(ch) => {
                                content.push(ch);
                                prev = Some(ch);
                            }
                            None => return Err(err("unterminated (* comment *)")),
                        }
                    }
                    tokens.push(Token::Comment(content));
                } else {
                    tokens.push(Token::LParen);
                }
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            '[' => {
                chars.next();
                tokens.push(Token::LBracket);
            }
            ']' => {
                chars.next();
                tokens.push(Token::RBracket);
            }
            '{' => {
                chars.next();
                tokens.push(Token::LBrace);
            }
            '}' => {
                chars.next();
                tokens.push(Token::RBrace);
            }
            '|' => {
                chars.next();
                tokens.push(Token::Pipe);
            }
            ';' => {
                chars.next();
                tokens.push(Token::Semi);
            }
            ':' => {
                chars.next();
                if chars.peek() != Some(&':') {
                    return Err(err("expected '::=' after ':'"));
                }
                chars.next();
                if chars.peek() != Some(&'=') {
                    return Err(err("expected '::=', found ':: '"));
                }
                chars.next();
                tokens.push(Token::Assign);
            }
            // Most terminals are double-quoted, but `quoted-identifier`
            // (sqlite-rs section) also spells out single-quote- and
            // backtick-delimited terminals for the identifier-quoting
            // styles it itself describes -- all three are terminal
            // strings, delimited by the character that opens them.
            '"' | '\'' | '`' => {
                let delim = c;
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some(ch) if ch == delim => break,
                        Some(ch) => s.push(ch),
                        None => return Err(err("unterminated string literal")),
                    }
                }
                tokens.push(Token::Str(s));
            }
            // `quoted-identifier`'s body also contains informal
            // descriptive prose ("any character, ... escapes a literal
            // ...") inline rather than real EBNF productions; the comma
            // there is filler, not a grammar operator, so it's dropped
            // rather than treated as a hard parse error.
            ',' => {
                chars.next();
            }
            ch if ch.is_alphanumeric() || ch == '_' => {
                let mut s = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_alphanumeric() || next == '_' || next == '-' {
                        s.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Ident(s));
            }
            other => return Err(err(format!("unexpected character '{other}'"))),
        }
    }

    Ok(tokens)
}

/// Extracts `Vn` tags from a comment's text, e.g. `"V2/V6 -- some note"`
/// -> `["V2", "V6"]`. Returns an empty vec for comments that don't start
/// with a `V`-tag token (ordinary explanatory comments).
fn extract_v_tags(comment: &str) -> Vec<String> {
    let Some(first_word) = comment.split_whitespace().next() else {
        return Vec::new();
    };
    let candidates: Vec<&str> = first_word.split('/').collect();
    let is_v_tag = |s: &str| {
        let mut chars = s.chars();
        chars.next() == Some('V') && chars.clone().all(|c| c.is_ascii_digit()) && chars.count() > 0
    };
    if candidates.iter().all(|c| is_v_tag(c)) {
        candidates.into_iter().map(str::to_string).collect()
    } else {
        Vec::new()
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos = self.pos.saturating_add(1);
        }
        tok
    }

    fn skip_comments(&mut self) {
        while matches!(self.peek(), Some(Token::Comment(_))) {
            self.pos = self.pos.saturating_add(1);
        }
    }

    /// The next non-comment token, without consuming anything -- used to
    /// tell an interspersed mid-sequence comment (followed by another
    /// term) apart from a trailing one (followed by `|`/`;`/a closer),
    /// which must be left for the caller to collect as a v-tag instead
    /// of being silently skipped here.
    fn peek_significant(&self) -> Option<&Token> {
        let mut i = self.pos;
        while matches!(self.tokens.get(i), Some(Token::Comment(_))) {
            i = i.saturating_add(1);
        }
        self.tokens.get(i)
    }

    fn expect(&mut self, want: &Token) -> Result<(), GrammarError> {
        match self.bump() {
            Some(tok) if tok == *want => Ok(()),
            Some(tok) => Err(err(format!("expected {want:?}, found {tok:?}"))),
            None => Err(err(format!("expected {want:?}, found end of input"))),
        }
    }

    /// Parses `sequence { "|" sequence }`. When `top_level` is true,
    /// trailing `(* Vn *)` comments right after each sequence are
    /// collected as that alternative's v-tags.
    fn parse_alternation(&mut self, top_level: bool) -> Result<(Alternation, Vec<Vec<String>>), GrammarError> {
        let mut sequences = Vec::new();
        let mut tags = Vec::new();
        loop {
            let seq = self.parse_sequence()?;
            let mut collected = Vec::new();
            while let Some(Token::Comment(text)) = self.peek() {
                let text = text.clone();
                self.bump();
                if top_level {
                    collected.extend(extract_v_tags(&text));
                }
            }
            sequences.push(seq);
            tags.push(collected);
            if matches!(self.peek(), Some(Token::Pipe)) {
                self.bump();
                continue;
            }
            break;
        }
        Ok((sequences, tags))
    }

    fn parse_sequence(&mut self) -> Result<Sequence, GrammarError> {
        let mut terms = Vec::new();
        // Only consume a run of comments here if a real term follows --
        // otherwise it's trailing (a `(* Vn *)` tag, or ordinary prose
        // before the next `|`/`;`/closer), and must be left for
        // `parse_alternation` to collect or the caller to see.
        while let Some(Token::Ident(_) | Token::Str(_) | Token::LParen | Token::LBracket | Token::LBrace) =
            self.peek_significant()
        {
            self.skip_comments();
            match self.peek() {
                Some(Token::Ident(name)) => {
                    let name = name.clone();
                    self.bump();
                    terms.push(Term::NonTerminal(name));
                }
                Some(Token::Str(text)) => {
                    let text = text.clone();
                    self.bump();
                    terms.push(Term::Terminal(text));
                }
                Some(Token::LParen) => {
                    self.bump();
                    let (alts, _) = self.parse_alternation(false)?;
                    self.expect(&Token::RParen)?;
                    terms.push(Term::Group(alts));
                }
                Some(Token::LBracket) => {
                    self.bump();
                    let (alts, _) = self.parse_alternation(false)?;
                    self.expect(&Token::RBracket)?;
                    terms.push(Term::Optional(alts));
                }
                Some(Token::LBrace) => {
                    self.bump();
                    let (alts, _) = self.parse_alternation(false)?;
                    self.expect(&Token::RBrace)?;
                    terms.push(Term::Repetition(alts));
                }
                // Unreachable: `peek_significant` already matched one of
                // the five term-starting variants above.
                _ => break,
            }
        }
        Ok(terms)
    }

    fn parse_rule(&mut self) -> Result<Option<Rule>, GrammarError> {
        self.skip_comments();
        let Some(Token::Ident(name)) = self.peek() else {
            return Ok(None);
        };
        let name = name.clone();
        self.bump();
        self.expect(&Token::Assign)?;
        let (alternatives, v_tags) = self.parse_alternation(true)?;
        self.expect(&Token::Semi)?;
        Ok(Some(Rule {
            name,
            alternatives,
            v_tags,
        }))
    }
}

fn parse_rules(src: &str) -> Result<BTreeMap<String, Rule>, GrammarError> {
    let tokens = tokenize(src)?;
    let mut parser = Parser { tokens, pos: 0 };
    let mut rules = BTreeMap::new();
    loop {
        parser.skip_comments();
        if parser.peek().is_none() {
            break;
        }
        match parser.parse_rule()? {
            Some(rule) => {
                rules.insert(rule.name.clone(), rule);
            }
            None => break,
        }
    }
    Ok(rules)
}

const SHARED_BANNER: &str = "===== 1. SHARED =====";
const COLUMN_BANNER: &str = "===== 2. COLUMN-RS ONLY =====";
const SQLITE_BANNER: &str = "===== 3. SQLITE-RS ONLY";

/// Scans forward from just after a comment's opening `"(*"`, applying the
/// same disambiguation `tokenize`'s comment reader uses (a `"*)"` is the
/// real close only if it's not glued to a preceding `"("` and is
/// followed by whitespace or end-of-input), and returns the byte offset
/// right after the real closing `"*)"`.
fn find_comment_close(src: &str, body_start: usize) -> Result<usize, GrammarError> {
    let body = src
        .get(body_start..)
        .ok_or_else(|| err("comment body offset landed outside the source string"))?;
    let mut prev: Option<char> = None;
    let mut chars = body.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch == '*' {
            if let Some(&(_, ')')) = chars.peek() {
                if prev != Some('(') {
                    let close_end = i.saturating_add(2);
                    let after = body.get(close_end..).and_then(|s| s.chars().next());
                    if after.is_none() || after.is_some_and(char::is_whitespace) {
                        return Ok(body_start.saturating_add(close_end));
                    }
                }
            }
        }
        prev = Some(ch);
    }
    Err(err("no real comment terminator found"))
}

/// Finds `banner`'s byte offset, then the end of the `(* ... *)` comment
/// block it sits inside (its section's rules start right after).
fn section_body_start(src: &str, banner: &str) -> Result<usize, GrammarError> {
    let banner_at = banner_offset(src, banner)?;
    let comment_open = enclosing_comment_start(src, banner_at)?;
    find_comment_close(src, comment_open.saturating_add("(*".len()))
}

/// The byte offset of `banner`'s text within `src`.
fn banner_offset(src: &str, banner: &str) -> Result<usize, GrammarError> {
    src.find(banner)
        .ok_or_else(|| err(format!("section banner {banner:?} not found in grammar file")))
}

/// The byte offset where the `(* ... *)` comment block *containing*
/// `banner_at` begins -- i.e. the last `"(*"` at or before it. A section's
/// preceding sibling must end here, not at `banner_at` itself, or the
/// sibling's text is left with a dangling unterminated `(* `.
fn enclosing_comment_start(src: &str, banner_at: usize) -> Result<usize, GrammarError> {
    src.get(..banner_at)
        .ok_or_else(|| err("banner offset landed outside the source string"))?
        .rfind("(*")
        .ok_or_else(|| err("no comment opener found before section banner"))
}

/// Parses the full three-section `grammar.ebnf` source into a [`Grammar`].
pub fn parse_source(src: &str) -> Result<Grammar, GrammarError> {
    let shared_at = banner_offset(src, SHARED_BANNER)?;
    let column_at = banner_offset(src, COLUMN_BANNER)?;
    let sqlite_at = banner_offset(src, SQLITE_BANNER)?;

    if !(shared_at < column_at && column_at < sqlite_at) {
        return Err(err("section banners appear out of expected order"));
    }

    let shared_body_start = section_body_start(src, SHARED_BANNER)?;
    let column_body_start = section_body_start(src, COLUMN_BANNER)?;
    let sqlite_body_start = section_body_start(src, SQLITE_BANNER)?;

    // Each section's text stops right before the *next* section banner's
    // own enclosing comment opens -- not at the banner's marker text,
    // which sits mid-comment and would otherwise leave the slice with a
    // dangling, unterminated `(* `.
    let column_comment_start = enclosing_comment_start(src, column_at)?;
    let sqlite_comment_start = enclosing_comment_start(src, sqlite_at)?;

    let shared_text = src
        .get(shared_body_start..column_comment_start)
        .ok_or_else(|| err("SHARED section slice out of bounds"))?;
    let column_text = src
        .get(column_body_start..sqlite_comment_start)
        .ok_or_else(|| err("COLUMN-RS ONLY section slice out of bounds"))?;
    let sqlite_text = src
        .get(sqlite_body_start..)
        .ok_or_else(|| err("SQLITE-RS ONLY section slice out of bounds"))?;

    Ok(Grammar {
        shared: parse_rules(shared_text)?,
        column: parse_rules(column_text)?,
        sqlite: parse_rules(sqlite_text)?,
    })
}
