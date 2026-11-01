//! Fulltext search: analyzer, posting encoding, query language and scoring.
//!
//! A fulltext index is an inverted index stored in an index B+tree. Every
//! (term, node) pair is one entry whose key is the term bytes, a `0x00`
//! separator and the big-endian node id, and whose value lists the term's
//! positions in the node's indexed text. Terms are alphanumeric, so the
//! separator never occurs inside a term and all postings of a term share the
//! prefix `term 0x00`, ordered by node id.
//!
//! The analyzer is deliberately simple and not Lucene-compatible: text is
//! split at every non-alphanumeric character and lowercased. Queries
//! support terms, `"quoted phrases"` and the `AND`/`OR` operators (`AND`
//! binds tighter; adjacent clauses are joined with `OR`).

use hashbrown::{HashMap, HashSet};

use crate::error::{Error, Result};
use crate::record::{decode_varint, encode_varint};

/// Tokens longer than this many bytes are not indexed (they could not fit a
/// B+tree key on small pages). Lucene's standard analyzer uses the same cap.
pub const MAX_TERM_BYTES: usize = 255;

/// Separator between the term and the node id in a posting key.
const TERM_END: u8 = 0x00;

/// Positions of one term inside one node's indexed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    /// Node id.
    pub node_id: u64,
    /// Ascending token positions.
    pub positions: Vec<u32>,
}

/// Split `text` into lowercase alphanumeric terms.
fn analyze(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty() && token.len() <= MAX_TERM_BYTES)
        .map(str::to_lowercase)
}

/// Group the terms of a document's property texts with their positions.
///
/// Positions continue across properties with a one-position gap, so a
/// phrase never matches across two property values.
///
/// # Arguments
///
/// * `texts` - The document's indexed property values, in key order
///
/// # Returns
///
/// `(term, ascending positions)` pairs, sorted by term
///
/// # Examples
///
/// ```
/// use grafile::fulltext::document_terms;
///
/// let terms = document_terms(["Red fox", "red"]);
/// assert_eq!(terms, vec![("fox".to_owned(), vec![1]), ("red".to_owned(), vec![0, 3])]);
/// ```
pub fn document_terms<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<(String, Vec<u32>)> {
    let mut tokens: Vec<(String, u32)> = Vec::new();
    let mut position = 0u32;
    for text in texts {
        for term in analyze(text) {
            tokens.push((term, position));
            position += 1;
        }
        position += 1;
    }
    // Stable sort: positions of a term stay ascending.
    tokens.sort_by(|a, b| a.0.cmp(&b.0));
    let mut grouped: Vec<(String, Vec<u32>)> = Vec::new();
    for (term, position) in tokens {
        match grouped.last_mut() {
            Some((last, positions)) if *last == term => positions.push(position),
            _ => grouped.push((term, vec![position])),
        }
    }
    grouped
}

/// Key prefix shared by every posting of `term`.
pub fn term_prefix(term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(term.len() + 9);
    key.extend_from_slice(term.as_bytes());
    key.push(TERM_END);
    key
}

/// B+tree key of the posting of `term` in `node_id`.
pub fn posting_key(term: &str, node_id: u64) -> Vec<u8> {
    let mut key = term_prefix(term);
    key.extend_from_slice(&node_id.to_be_bytes());
    key
}

/// Node id stored at the end of a posting key.
///
/// # Errors
///
/// Returns a corruption error when the key is shorter than a node id.
pub fn posting_node(key: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = key
        .len()
        .checked_sub(8)
        .and_then(|start| key.get(start..))
        .and_then(|tail| tail.try_into().ok())
        .ok_or_else(|| Error::corrupt("fulltext posting key too short"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// Encode ascending positions as a count followed by delta varints.
pub fn encode_positions(positions: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(positions.len() + 1);
    encode_varint(&mut out, positions.len() as u64);
    let mut previous = 0u32;
    for position in positions {
        encode_varint(&mut out, u64::from(position - previous));
        previous = *position;
    }
    out
}

/// Decode positions written by [`encode_positions`].
///
/// # Errors
///
/// Returns a corruption error on truncated or out-of-range data.
pub fn decode_positions(bytes: &[u8]) -> Result<Vec<u32>> {
    let mut pos = 0usize;
    let count = usize::try_from(decode_varint(bytes, &mut pos)?)
        .map_err(|_| Error::corrupt("fulltext position count out of range"))?;
    let mut positions = Vec::with_capacity(count.min(bytes.len()));
    let mut current = 0u32;
    for _ in 0..count {
        let delta = u32::try_from(decode_varint(bytes, &mut pos)?)
            .map_err(|_| Error::corrupt("fulltext position out of range"))?;
        current = current
            .checked_add(delta)
            .ok_or_else(|| Error::corrupt("fulltext position overflow"))?;
        positions.push(current);
    }
    Ok(positions)
}

/// A parsed fulltext query.
#[derive(Debug, Clone, PartialEq)]
pub enum FulltextQuery {
    /// A single analyzed term.
    Term(String),
    /// Consecutive analyzed terms.
    Phrase(Vec<String>),
    /// Every clause must match; scores add up.
    And(Vec<FulltextQuery>),
    /// Any clause may match; scores add up.
    Or(Vec<FulltextQuery>),
}

/// Lexical unit of the query language.
#[derive(Debug, PartialEq)]
enum QueryToken {
    Clause(FulltextQuery),
    And,
    Or,
}

impl FulltextQuery {
    /// Parse a query string.
    ///
    /// Bare words go through the analyzer; a word that splits into several
    /// terms (`foo-bar`) is a phrase. `AND` and `OR` are operators only in
    /// upper case.
    ///
    /// # Errors
    ///
    /// Returns a syntax error for an empty query, an unterminated quote or a
    /// misplaced operator.
    ///
    /// # Examples
    ///
    /// ```
    /// use grafile::fulltext::FulltextQuery;
    ///
    /// let query = FulltextQuery::parse("graphs AND \"hot take\"").expect("query");
    /// assert_eq!(
    ///     query,
    ///     FulltextQuery::And(vec![
    ///         FulltextQuery::Term("graphs".to_owned()),
    ///         FulltextQuery::Phrase(vec!["hot".to_owned(), "take".to_owned()]),
    ///     ])
    /// );
    /// ```
    pub fn parse(text: &str) -> Result<Self> {
        let tokens = tokenize_query(text)?;
        let mut alternatives: Vec<Self> = Vec::new();
        let mut conjunction: Vec<Self> = Vec::new();
        let mut pending_and = false;
        let mut after_operator = true;
        for token in tokens {
            match token {
                QueryToken::Clause(clause) => {
                    if !pending_and && !conjunction.is_empty() {
                        alternatives.push(Self::collapse(std::mem::take(&mut conjunction), true));
                    }
                    conjunction.push(clause);
                    pending_and = false;
                    after_operator = false;
                }
                QueryToken::And | QueryToken::Or if after_operator => {
                    return Err(misplaced_operator());
                }
                QueryToken::And => {
                    pending_and = true;
                    after_operator = true;
                }
                QueryToken::Or => {
                    alternatives.push(Self::collapse(std::mem::take(&mut conjunction), true));
                    after_operator = true;
                }
            }
        }
        if after_operator {
            return Err(if conjunction.is_empty() && alternatives.is_empty() {
                Error::syntax("fulltext query has no searchable terms")
            } else {
                misplaced_operator()
            });
        }
        alternatives.push(Self::collapse(conjunction, true));
        Ok(Self::collapse(alternatives, false))
    }

    /// A single clause stays as is; several become `And` or `Or`.
    fn collapse(mut clauses: Vec<Self>, and: bool) -> Self {
        if clauses.len() == 1 {
            return clauses.pop().expect("one clause");
        }
        if and {
            Self::And(clauses)
        } else {
            Self::Or(clauses)
        }
    }

    /// Every distinct term the query reads postings for.
    pub fn terms(&self) -> HashSet<&str> {
        let mut out = HashSet::new();
        self.collect_terms(&mut out);
        out
    }

    fn collect_terms<'a>(&'a self, out: &mut HashSet<&'a str>) {
        match self {
            Self::Term(term) => {
                out.insert(term.as_str());
            }
            Self::Phrase(terms) => out.extend(terms.iter().map(String::as_str)),
            Self::And(clauses) | Self::Or(clauses) => {
                clauses.iter().for_each(|clause| clause.collect_terms(out));
            }
        }
    }

    /// Score every matching node.
    ///
    /// # Arguments
    ///
    /// * `postings` - Postings of every term in [`FulltextQuery::terms`],
    ///   each list ordered by node id (missing terms have no postings)
    /// * `doc_count` - Number of documents the index covers, for idf
    ///
    /// # Returns
    ///
    /// `(node id, score)` pairs ordered by descending score, then node id
    pub fn search(
        &self,
        postings: &HashMap<String, Vec<Posting>>,
        doc_count: u64,
    ) -> Vec<(u64, f64)> {
        let scorer = Scorer {
            postings,
            doc_count,
        };
        let mut hits: Vec<(u64, f64)> = scorer.eval(self).into_iter().collect();
        hits.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        hits
    }
}

fn misplaced_operator() -> Error {
    Error::syntax("fulltext query operator AND/OR needs a clause on both sides")
}

/// Split a query string into clauses and operators.
fn tokenize_query(text: &str) -> Result<Vec<QueryToken>> {
    let mut tokens = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let (word, quoted, remainder) = if let Some(after_quote) = rest.strip_prefix('"') {
            let end = after_quote
                .find('"')
                .ok_or_else(|| Error::syntax("unterminated phrase in fulltext query"))?;
            (&after_quote[..end], true, &after_quote[end + 1..])
        } else {
            let end = rest
                .find(|ch: char| ch.is_whitespace() || ch == '"')
                .unwrap_or(rest.len());
            (&rest[..end], false, &rest[end..])
        };
        rest = remainder.trim_start();
        if !quoted && word == "AND" {
            tokens.push(QueryToken::And);
            continue;
        }
        if !quoted && word == "OR" {
            tokens.push(QueryToken::Or);
            continue;
        }
        let mut terms: Vec<String> = analyze(word).collect();
        match terms.len() {
            0 => {}
            1 => tokens.push(QueryToken::Clause(FulltextQuery::Term(
                terms.pop().expect("one term"),
            ))),
            _ => tokens.push(QueryToken::Clause(FulltextQuery::Phrase(terms))),
        }
    }
    Ok(tokens)
}

/// Query evaluation over prefetched postings.
struct Scorer<'a> {
    postings: &'a HashMap<String, Vec<Posting>>,
    doc_count: u64,
}

impl Scorer<'_> {
    fn list(&self, term: &str) -> &[Posting] {
        self.postings.get(term).map_or(&[], Vec::as_slice)
    }

    /// BM25-style inverse document frequency; always positive.
    fn idf(&self, document_frequency: usize) -> f64 {
        let df = document_frequency as f64;
        let n = (self.doc_count as f64).max(df);
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    fn eval(&self, query: &FulltextQuery) -> HashMap<u64, f64> {
        match query {
            FulltextQuery::Term(term) => {
                let list = self.list(term);
                let idf = self.idf(list.len());
                list.iter()
                    .map(|posting| {
                        (
                            posting.node_id,
                            idf * (posting.positions.len() as f64).sqrt(),
                        )
                    })
                    .collect()
            }
            FulltextQuery::Phrase(terms) => self.phrase(terms),
            FulltextQuery::And(clauses) => {
                let mut results: Vec<HashMap<u64, f64>> =
                    clauses.iter().map(|clause| self.eval(clause)).collect();
                results.sort_unstable_by_key(HashMap::len);
                let mut iter = results.into_iter();
                let mut acc = iter.next().unwrap_or_default();
                for other in iter {
                    acc.retain(|node, score| match other.get(node) {
                        Some(extra) => {
                            *score += extra;
                            true
                        }
                        None => false,
                    });
                }
                acc
            }
            FulltextQuery::Or(clauses) => {
                let mut acc: HashMap<u64, f64> = HashMap::new();
                for clause in clauses {
                    for (node, score) in self.eval(clause) {
                        *acc.entry(node).or_insert(0.0) += score;
                    }
                }
                acc
            }
        }
    }

    /// Nodes where the terms occur at consecutive positions.
    fn phrase(&self, terms: &[String]) -> HashMap<u64, f64> {
        let lists: Vec<&[Posting]> = terms.iter().map(|term| self.list(term)).collect();
        let idf_sum: f64 = lists.iter().map(|list| self.idf(list.len())).sum();
        let mut out = HashMap::new();
        let Some((first, rest)) = lists.split_first() else {
            return out;
        };
        for posting in first.iter() {
            // Lists are ordered by node id, so membership is a binary search.
            let others: Option<Vec<&Posting>> = rest
                .iter()
                .map(|list| {
                    list.binary_search_by_key(&posting.node_id, |other| other.node_id)
                        .ok()
                        .map(|found| &list[found])
                })
                .collect();
            let Some(others) = others else {
                continue;
            };
            let frequency = posting
                .positions
                .iter()
                .filter(|start| {
                    others.iter().enumerate().all(|(offset, other)| {
                        start
                            .checked_add(offset as u32 + 1)
                            .is_some_and(|wanted| other.positions.binary_search(&wanted).is_ok())
                    })
                })
                .count();
            if frequency > 0 {
                out.insert(posting.node_id, idf_sum * (frequency as f64).sqrt());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(text: &str) -> FulltextQuery {
        FulltextQuery::Term(text.to_owned())
    }

    fn postings_of(docs: &[(u64, &str)]) -> HashMap<String, Vec<Posting>> {
        let mut out: HashMap<String, Vec<Posting>> = HashMap::new();
        for (node_id, text) in docs {
            for (term, positions) in document_terms([*text]) {
                out.entry(term).or_default().push(Posting {
                    node_id: *node_id,
                    positions,
                });
            }
        }
        out
    }

    fn run(query: &str, docs: &[(u64, &str)]) -> Vec<u64> {
        let query = FulltextQuery::parse(query).expect("query");
        query
            .search(&postings_of(docs), docs.len() as u64)
            .into_iter()
            .map(|(node, _)| node)
            .collect()
    }

    #[test]
    fn analyzer_lowercases_and_splits_on_punctuation() {
        let terms = document_terms(["Hot take on AI #42", "Über-cool"]);
        let names: Vec<&str> = terms.iter().map(|(term, _)| term.as_str()).collect();
        assert_eq!(names, vec!["42", "ai", "cool", "hot", "on", "take", "über"]);
        let uber = terms.iter().find(|(term, _)| term == "über").expect("term");
        assert_eq!(uber.1, vec![6], "the property gap shifts later positions");
    }

    #[test]
    fn overlong_tokens_are_skipped() {
        let long = "a".repeat(MAX_TERM_BYTES + 1);
        let text = format!("short {long}");
        let terms = document_terms([text.as_str()]);
        assert_eq!(terms, vec![("short".to_owned(), vec![0])]);
    }

    #[test]
    fn positions_round_trip() {
        let positions = vec![0, 3, 4, 900, 100_000];
        let decoded = decode_positions(&encode_positions(&positions)).expect("decode");
        assert_eq!(decoded, positions);
        assert!(decode_positions(&[3, 1]).is_err(), "truncated list");
    }

    #[test]
    fn posting_keys_group_by_term_and_order_by_node() {
        let a = posting_key("cat", 2);
        let b = posting_key("cat", 256);
        let c = posting_key("cats", 1);
        assert!(a < b && b < c);
        assert!(a.starts_with(&term_prefix("cat")));
        assert!(!c.starts_with(&term_prefix("cat")));
        assert_eq!(posting_node(&b).expect("node"), 256);
        assert!(posting_node(&[1, 2]).is_err());
    }

    #[test]
    fn parses_operators_with_and_binding_tighter() {
        let query = FulltextQuery::parse("a b AND c OR d").expect("parse");
        assert_eq!(
            query,
            FulltextQuery::Or(vec![
                term("a"),
                FulltextQuery::And(vec![term("b"), term("c")]),
                term("d"),
            ])
        );
        assert_eq!(
            FulltextQuery::parse("foo-bar").expect("parse"),
            FulltextQuery::Phrase(vec!["foo".to_owned(), "bar".to_owned()])
        );
        assert_eq!(FulltextQuery::parse("and or").expect("lowercase"), {
            FulltextQuery::Or(vec![term("and"), term("or")])
        });
    }

    #[test]
    fn rejects_malformed_queries() {
        for bad in ["", "  # ", "AND a", "a OR", "a AND OR b", "\"open phrase"] {
            assert!(FulltextQuery::parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn term_and_boolean_matching() {
        let docs = [
            (1, "graphs are fun"),
            (2, "coffee and graphs"),
            (3, "coffee only"),
        ];
        let mut both = run("graphs AND coffee", &docs);
        both.sort_unstable();
        assert_eq!(both, vec![2]);
        let mut either = run("graphs OR only", &docs);
        either.sort_unstable();
        assert_eq!(either, vec![1, 2, 3]);
        assert!(run("missing", &docs).is_empty());
        assert!(run("missing AND graphs", &docs).is_empty());
    }

    #[test]
    fn phrases_need_consecutive_positions() {
        let docs = [
            (1, "hot take on ai"),
            (2, "take hot coffee"),
            (3, "hot hot take"),
        ];
        let mut hits = run("\"hot take\"", &docs);
        hits.sort_unstable();
        assert_eq!(hits, vec![1, 3]);
        assert!(run("\"take on coffee\"", &docs).is_empty());
    }

    #[test]
    fn phrases_do_not_span_properties() {
        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        for (term, positions) in document_terms(["red", "fox"]) {
            postings.entry(term).or_default().push(Posting {
                node_id: 7,
                positions,
            });
        }
        let query = FulltextQuery::parse("\"red fox\"").expect("parse");
        assert!(query.search(&postings, 1).is_empty());
    }

    #[test]
    fn rarer_and_repeated_terms_score_higher() {
        let docs = [
            (1, "common rare"),
            (2, "common"),
            (3, "common common common"),
        ];
        let query = FulltextQuery::parse("common OR rare").expect("parse");
        let hits = query.search(&postings_of(&docs), 3);
        assert_eq!(hits[0].0, 1, "the rare term outweighs repetition");
        assert_eq!(hits[1].0, 3);
        assert_eq!(hits[2].0, 2);
        assert!(hits.iter().all(|(_, score)| *score > 0.0));
    }

    #[test]
    fn collects_distinct_terms() {
        let query = FulltextQuery::parse("a \"a b\" AND c").expect("parse");
        let mut terms: Vec<&str> = query.terms().into_iter().collect();
        terms.sort_unstable();
        assert_eq!(terms, vec!["a", "b", "c"]);
    }
}
