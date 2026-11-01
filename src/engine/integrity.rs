//! Integrity checking and derived-structure rebuild.
//!
//! Node and edge records are the primary data. Incidence, label and type
//! fragments, property indexes and fulltext postings are derived from them,
//! so the records serve as the oracle: the checker recomputes what every
//! derived structure must contain and compares. It also verifies B+tree
//! structure, page ownership (every page belongs to exactly one tree or the
//! freelist) and the freelist itself.
//!
//! Each derived set is checked in two cheap halves: every bit or entry a
//! record implies must be present, and the stored total must equal the
//! expected total. Together they prove set equality without materialising
//! the expected sets; only when the totals disagree does a targeted scan
//! name the extra entries.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
use indicatif::ProgressBar;
use serde::Serialize;

use super::{Database, edge_index_prefix, node_index_prefix};
use crate::btree::{Btree, CheckStats, TreeKind};
use crate::catalog::{EdgeIndexDef, FulltextIndexDef, IndexKind, NodeIndexDef};
use crate::error::{Error, ErrorCode, Result};
use crate::fulltext::{document_terms, encode_positions, posting_key, posting_node};
use crate::matrix::{
    ANY_TYPE, Direction, FAMILY_EDGE_TYPE, FAMILY_IN, FAMILY_LABEL, FAMILY_OUT, Fragment,
    FragmentKey, FragmentStore,
};
use crate::pager::PageNo;
use crate::record::{EdgeRecord, NodeRecord, Value};

/// Findings stored per kind; further ones are only counted, so a badly
/// damaged file still yields a readable report.
const MAX_FINDINGS_PER_KIND: usize = 100;

/// Which structure an integrity finding concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Header counters disagree with the stored data.
    Header,
    /// A B+tree, overflow chain or catalog entry is malformed.
    Structure,
    /// The freelist is malformed, or a page is leaked or claimed twice.
    Pages,
    /// A node or edge record is undecodable or references missing data.
    Record,
    /// Out/in incidence planes disagree with edge records.
    Incidence,
    /// Label bitmaps disagree with node records.
    Label,
    /// Edge-type bitmaps disagree with edge records.
    EdgeType,
    /// Property index entries disagree with records.
    Index,
    /// Uniqueness or existence constraints are violated by stored data.
    Constraint,
    /// Fulltext postings disagree with node records.
    Fulltext,
}

impl FindingKind {
    /// Stable short name, used in reports.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::Structure => "structure",
            Self::Pages => "pages",
            Self::Record => "record",
            Self::Incidence => "incidence",
            Self::Label => "label",
            Self::EdgeType => "edge_type",
            Self::Index => "index",
            Self::Constraint => "constraint",
            Self::Fulltext => "fulltext",
        }
    }

    fn of_family(family: u8) -> Self {
        match family {
            FAMILY_LABEL => Self::Label,
            FAMILY_EDGE_TYPE => Self::EdgeType,
            _ => Self::Incidence,
        }
    }
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One integrity violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    kind: FindingKind,
    message: String,
}

impl Finding {
    /// The structure the finding concerns.
    pub fn kind(&self) -> FindingKind {
        self.kind
    }

    /// Human-readable description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Sizes observed while checking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IntegrityStats {
    /// Pages in the file, including page 1.
    pub pages: u64,
    /// Pages on the freelist.
    pub free_pages: u64,
    /// Node records.
    pub nodes: u64,
    /// Edge records.
    pub edges: u64,
    /// Stored bitmap fragments.
    pub fragments: u64,
    /// Node and edge property index entries.
    pub index_entries: u64,
    /// Fulltext postings.
    pub postings: u64,
}

/// Result of [`Database::check_integrity`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IntegrityReport {
    findings: Vec<Finding>,
    /// Findings past the per-kind cap, counted but not stored.
    suppressed: BTreeMap<FindingKind, u64>,
    stats: IntegrityStats,
    /// Stored findings per kind, so the cap check is constant time.
    #[serde(skip)]
    stored: BTreeMap<FindingKind, usize>,
}

impl IntegrityReport {
    /// Whether no violation was found.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// Stored findings, at most 100 per kind, in discovery order.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Number of findings past the per-kind cap.
    pub fn suppressed(&self) -> u64 {
        self.suppressed.values().sum()
    }

    /// Sizes observed while checking.
    pub fn stats(&self) -> &IntegrityStats {
        &self.stats
    }

    /// The report as a JSON object with `findings`, `suppressed` (per kind)
    /// and `stats`.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("integrity reports contain only serializable data")
    }

    fn push(&mut self, kind: FindingKind, message: impl Into<String>) {
        let stored = self.stored.entry(kind).or_default();
        if *stored < MAX_FINDINGS_PER_KIND {
            *stored += 1;
            self.findings.push(Finding {
                kind,
                message: message.into(),
            });
        } else {
            *self.suppressed.entry(kind).or_default() += 1;
        }
    }
}

impl fmt::Display for IntegrityReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for finding in &self.findings {
            writeln!(f, "{}: {}", finding.kind, finding.message)?;
        }
        for (kind, count) in &self.suppressed {
            writeln!(f, "{kind}: {count} more findings not shown")?;
        }
        let stats = &self.stats;
        let total = self.findings.len() as u64 + self.suppressed();
        write!(
            f,
            "{} finding{}: {} pages ({} free), {} nodes, {} edges, {} fragments, \
             {} index entries, {} postings",
            total,
            if total == 1 { "" } else { "s" },
            stats.pages,
            stats.free_pages,
            stats.nodes,
            stats.edges,
            stats.fragments,
            stats.index_entries,
            stats.postings
        )
    }
}

/// Result of [`Database::rebuild_derived`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RebuildSummary {
    rebuilt: Vec<String>,
    abandoned: Vec<String>,
}

impl RebuildSummary {
    /// Derived structures regenerated from records.
    pub fn rebuilt(&self) -> &[String] {
        &self.rebuilt
    }

    /// Structures whose old pages were too damaged to free safely. They
    /// were replaced, and their pages are reported as leaked afterwards.
    pub fn abandoned(&self) -> &[String] {
        &self.abandoned
    }
}

impl Database {
    /// Verify every derived structure against the records, plus B+tree
    /// structure, page ownership and the freelist. Read-only.
    ///
    /// # Returns
    ///
    /// A report that is clean when no violation was found
    ///
    /// # Errors
    ///
    /// Only I/O and lock errors abort the check; corruption becomes a
    /// finding in the report.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::{path::Path, sync::Arc};
    /// use grafile::engine::Database;
    /// use grafile::vfs::mem::MemVfs;
    ///
    /// let mut db = Database::create(Arc::new(MemVfs::new()), Path::new("doc.db"), 4096)?;
    /// db.write(|db| {
    ///     let person = db.ensure_label("Person")?;
    ///     db.create_node(&[person], vec![])
    /// })?;
    /// assert!(db.check_integrity()?.is_clean());
    /// # Ok::<(), grafile::Error>(())
    /// ```
    pub fn check_integrity(&mut self) -> Result<IntegrityReport> {
        self.check_integrity_with_progress(&ProgressBar::hidden())
    }

    /// [`Self::check_integrity`] reporting record progress to `progress`.
    ///
    /// # Arguments
    ///
    /// * `progress` - Bar whose length is set to the record count
    ///
    /// # Errors
    ///
    /// Same as [`Self::check_integrity`].
    pub fn check_integrity_with_progress(
        &mut self,
        progress: &ProgressBar,
    ) -> Result<IntegrityReport> {
        let reading = !self.pager.in_write_txn();
        if reading {
            self.begin_read()?;
        } else {
            // Mid-transaction, unflushed fragments live only in the cache.
            self.fragments.flush(&mut self.pager)?;
        }
        let report = Checker::new(self, progress).run();
        if reading {
            self.end_read()?;
        }
        report
    }

    /// Regenerate fragments, property indexes and fulltext indexes from the
    /// records inside one write transaction. Records are never modified.
    ///
    /// # Arguments
    ///
    /// * `progress` - Bar whose length is set to the record count
    ///
    /// # Returns
    ///
    /// Which structures were rebuilt and which old trees were abandoned
    ///
    /// # Errors
    ///
    /// Returns `Corrupt` when the catalog, node or edge table, or the
    /// freelist is damaged (primary data cannot be recovered), `Constraint`
    /// when the records violate a uniqueness constraint, and propagates
    /// I/O errors. The transaction rolls back on any error.
    pub fn rebuild_derived(&mut self, progress: &ProgressBar) -> Result<RebuildSummary> {
        self.write(|db| rebuild(db, progress))
    }
}

/// Map a `Corrupt` error to `None`: the damage is reported by another pass,
/// and a lookup made while locating extras must not abort the check.
fn corrupt_as_none<T>(result: Result<Option<T>>) -> Result<Option<T>> {
    match result {
        Err(err) if err.code() == ErrorCode::Corrupt => Ok(None),
        other => other,
    }
}

fn table_id(key: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| Error::corrupt("table key is not 8 bytes"))?;
    Ok(u64::from_be_bytes(bytes))
}

/// The entity id stored in the last 8 bytes of an index key.
fn index_entry_id(key: &[u8]) -> Result<u64> {
    let split = key
        .len()
        .checked_sub(8)
        .ok_or_else(|| Error::corrupt("index key shorter than its entity id"))?;
    table_id(&key[split..])
}

fn node_index_name(def: &NodeIndexDef) -> String {
    def.name.clone().unwrap_or_else(|| format!("#{}", def.id))
}

fn edge_index_name(def: &EdgeIndexDef) -> String {
    def.name.clone().unwrap_or_else(|| format!("#{}", def.id))
}

/// Why a property list is invalid, if it is.
fn props_problem(props: &[(u32, Value)], key_count: u32) -> Option<&'static str> {
    if props.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Some("unsorted or duplicate property keys");
    }
    if props.iter().any(|(key, _)| *key == 0 || *key > key_count) {
        return Some("an unknown property key");
    }
    if props.iter().any(|(_, value)| value.is_null()) {
        return Some("a stored null property");
    }
    None
}

/// Decoded fragments for the id band being verified. Records arrive in id
/// order, so each fragment of a band is fetched and decoded once.
#[derive(Default)]
struct FragmentCache {
    band: u64,
    fragments: HashMap<FragmentKey, Option<Fragment>>,
}

impl FragmentCache {
    fn bit_set(
        &mut self,
        db: &mut Database,
        band: u64,
        key: FragmentKey,
        bit: u16,
    ) -> Result<bool> {
        if band != self.band {
            self.fragments.clear();
            self.band = band;
        }
        // `entry` hands out the map slot itself (like `dict.setdefault` with
        // a lazily computed default), so each key is hashed once.
        let fragment = match self.fragments.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let tree = Btree::from_root(db.fragments.root(), TreeKind::Index);
                // Undecodable fragments are reported by the fragment scan.
                let fragment = tree
                    .get(&mut db.pager, &key.encode())?
                    .and_then(|bytes| Fragment::decode(&bytes).ok());
                entry.insert(fragment)
            }
        };
        Ok(fragment.as_ref().is_some_and(|fragment| fragment.test(bit)))
    }
}

/// Expected and missing counts for one derived set.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    expected: u64,
    missing: u64,
}

impl Tally {
    /// Entries that are stored but implied by no record.
    fn extras(self, stored: u64) -> u64 {
        (stored + self.missing).saturating_sub(self.expected)
    }
}

/// Index trees are addressed by node or edge definitions.
enum IndexTarget<'a> {
    Node(&'a NodeIndexDef),
    Edge(&'a EdgeIndexDef),
}

/// One check run. `'a` ties the checker to the exclusive borrow of the
/// database it reads through, which ends when `run` returns the report.
struct Checker<'a> {
    db: &'a mut Database,
    progress: &'a ProgressBar,
    report: IntegrityReport,
    /// Statistics of every tree whose structure verified, by root page.
    trees: HashMap<PageNo, CheckStats>,
    /// Ascending ids of decodable node records, for endpoint checks.
    node_ids: Vec<u64>,
    /// Bits per `(family, family id)` in the fragment store.
    stored_bits: HashMap<(u8, u32), u64>,
    bits: HashMap<(u8, u32), Tally>,
    /// Per index or fulltext object id.
    entries: HashMap<u32, Tally>,
}

impl<'a> Checker<'a> {
    fn new(db: &'a mut Database, progress: &'a ProgressBar) -> Self {
        Self {
            db,
            progress,
            report: IntegrityReport::default(),
            trees: HashMap::new(),
            node_ids: Vec::new(),
            stored_bits: HashMap::new(),
            bits: HashMap::new(),
            entries: HashMap::new(),
        }
    }

    fn run(mut self) -> Result<IntegrityReport> {
        let result = self.check_structure();
        self.absorb(FindingKind::Structure, result)?;
        let (nodes, edges) = (self.db.nodes.root(), self.db.edges.root());
        let records = [nodes, edges]
            .iter()
            .filter_map(|root| self.trees.get(root))
            .map(|stats| stats.keys)
            .sum();
        self.progress.set_length(records);
        if self.trees.contains_key(&self.db.fragments.root()) {
            let result = self.scan_fragments();
            self.absorb(FindingKind::Structure, result)?;
        }
        if self.trees.contains_key(&nodes) {
            self.progress.set_message("checking nodes");
            let result = self.check_nodes();
            self.absorb(FindingKind::Record, result)?;
        }
        if self.trees.contains_key(&edges) {
            self.progress.set_message("checking edges");
            let result = self.check_edges();
            self.absorb(FindingKind::Record, result)?;
        }
        self.check_header();
        self.progress.set_message("checking derived totals");
        if self.trees.contains_key(&self.db.fragments.root()) {
            let result = self.compare_fragment_totals();
            self.absorb(FindingKind::Structure, result)?;
        }
        let result = self.compare_index_totals();
        self.absorb(FindingKind::Index, result)?;
        self.progress.finish_and_clear();
        Ok(self.report)
    }

    /// Record a `Corrupt` error as a finding; other errors abort the check.
    fn absorb(&mut self, kind: FindingKind, result: Result<()>) -> Result<()> {
        match result {
            Err(err) if err.code() == ErrorCode::Corrupt => {
                self.report.push(kind, err.message());
                Ok(())
            }
            other => other,
        }
    }

    fn tree_list(&self) -> Vec<(String, Btree)> {
        let schema = self.db.schema();
        let mut trees = vec![
            (
                "catalog".to_owned(),
                Btree::from_root(self.db.catalog.root(), TreeKind::Table),
            ),
            ("node table".to_owned(), self.db.nodes),
            ("edge table".to_owned(), self.db.edges),
            (
                "fragment store".to_owned(),
                Btree::from_root(self.db.fragments.root(), TreeKind::Index),
            ),
        ];
        trees.extend(schema.node_indexes.iter().map(|def| {
            (
                format!("node index `{}`", node_index_name(def)),
                Btree::from_root(def.root, TreeKind::Index),
            )
        }));
        trees.extend(schema.edge_indexes.iter().map(|def| {
            (
                format!("edge index `{}`", edge_index_name(def)),
                Btree::from_root(def.root, TreeKind::Index),
            )
        }));
        trees.extend(schema.fulltext_indexes.iter().map(|def| {
            (
                format!("fulltext index `{}`", def.name),
                Btree::from_root(def.root, TreeKind::Index),
            )
        }));
        trees
    }

    /// B+tree structure, page ownership and the freelist.
    fn check_structure(&mut self) -> Result<()> {
        self.progress.set_message("checking page structure");
        let mut owned: HashSet<PageNo> = HashSet::from([1]);
        let mut complete = true;
        for (name, tree) in self.tree_list() {
            match tree.integrity_check_into(&mut self.db.pager, &mut owned) {
                Ok(stats) => {
                    self.trees.insert(tree.root(), stats);
                }
                Err(err) if err.code() == ErrorCode::Corrupt => {
                    complete = false;
                    self.report
                        .push(FindingKind::Structure, format!("{name}: {}", err.message()));
                }
                Err(err) => return Err(err),
            }
        }
        let page_count = self.db.pager.header().page_count;
        self.report.stats.pages = page_count;
        let free = match self.db.pager.freelist_pages() {
            Ok(free) => free,
            Err(err) if err.code() == ErrorCode::Corrupt => {
                self.report.push(FindingKind::Pages, err.message());
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        self.report.stats.free_pages = free.len() as u64;
        for page in free {
            if !owned.insert(page) {
                self.report.push(
                    FindingKind::Pages,
                    format!("page {page} is on the freelist but owned by a tree"),
                );
            }
        }
        // A tree that failed its check was only partly walked, so its
        // remaining pages would show up as false leaks.
        if complete {
            for page in 2..=page_count as PageNo {
                if !owned.contains(&page) {
                    self.report.push(
                        FindingKind::Pages,
                        format!("page {page} is neither free nor owned by a tree"),
                    );
                }
            }
        }
        Ok(())
    }

    /// Validate every stored fragment and total its bits per family.
    fn scan_fragments(&mut self) -> Result<()> {
        let tree = Btree::from_root(self.db.fragments.root(), TreeKind::Index);
        let (label_count, type_count) = {
            let schema = self.db.schema();
            (schema.labels.len() as u32, schema.edge_types.len() as u32)
        };
        let mut cursor = tree.cursor();
        let mut ok = cursor.first(&mut self.db.pager)?;
        while ok {
            let key = FragmentKey::decode(cursor.key());
            let value = cursor.value(&mut self.db.pager)?;
            ok = cursor.next(&mut self.db.pager)?;
            self.report.stats.fragments += 1;
            let key = match key {
                Ok(key) => key,
                Err(err) => {
                    self.report.push(FindingKind::Structure, err.message());
                    continue;
                }
            };
            let problem = match key.family {
                FAMILY_OUT | FAMILY_IN => {
                    (key.family_id > type_count).then_some("an unknown edge type")
                }
                FAMILY_LABEL => (key.family_id == 0 || key.family_id > label_count)
                    .then_some("an unknown label"),
                FAMILY_EDGE_TYPE => (key.family_id == 0 || key.family_id > type_count)
                    .then_some("an unknown edge type"),
                _ => Some("an unknown family"),
            }
            .or_else(|| {
                (matches!(key.family, FAMILY_LABEL | FAMILY_EDGE_TYPE) && key.band1 != 0)
                    .then_some("a nonzero second band")
            });
            if let Some(problem) = problem {
                self.report.push(
                    FindingKind::Structure,
                    format!("fragment {key:?} has {problem}"),
                );
                continue;
            }
            match Fragment::decode(&value) {
                Ok(fragment) if fragment.is_empty() => self.report.push(
                    FindingKind::Structure,
                    format!("fragment {key:?} is stored empty"),
                ),
                Ok(fragment) => {
                    *self
                        .stored_bits
                        .entry((key.family, key.family_id))
                        .or_default() += u64::from(fragment.cardinality());
                }
                Err(err) => self.report.push(
                    FindingKind::Structure,
                    format!("fragment {key:?}: {}", err.message()),
                ),
            }
        }
        Ok(())
    }

    fn check_header(&mut self) {
        let header = self.db.pager.header().clone();
        let schema = self.db.schema();
        let counters = [
            ("label", header.next_label_id, schema.labels.len()),
            ("edge type", header.next_type_id, schema.edge_types.len()),
            (
                "property key",
                header.next_key_id,
                schema.property_keys.len(),
            ),
        ];
        let mut problems: Vec<String> = counters
            .iter()
            .filter(|(_, next, count)| *next as usize != count + 1)
            .map(|(name, next, count)| {
                format!("next {name} id is {next} but {count} are registered")
            })
            .collect();
        if let Some(max) = self.node_ids.last()
            && *max >= header.next_node_id
        {
            problems.push(format!(
                "next node id {} does not exceed stored node {max}",
                header.next_node_id
            ));
        }
        if let Some(max) = self.max_edge_id()
            && max >= header.next_edge_id
        {
            problems.push(format!(
                "next edge id {} does not exceed stored edge {max}",
                header.next_edge_id
            ));
        }
        for problem in problems {
            self.report.push(FindingKind::Header, problem);
        }
    }

    fn max_edge_id(&self) -> Option<u64> {
        self.trees
            .get(&self.db.edges.root())
            .and_then(|stats| stats.root_max.as_deref())
            .and_then(|key| table_id(key).ok())
    }

    /// Test an implied bit, tallying it for the totals comparison.
    fn bit_present(
        &mut self,
        cache: &mut FragmentCache,
        band: u64,
        key: FragmentKey,
        bit: u16,
    ) -> Result<bool> {
        let present = cache.bit_set(self.db, band, key, bit)?;
        let tally = self.bits.entry((key.family, key.family_id)).or_default();
        tally.expected += 1;
        if !present {
            tally.missing += 1;
        }
        Ok(present)
    }

    fn entry_expected(&mut self, object_id: u32, present: bool) {
        let tally = self.entries.entry(object_id).or_default();
        tally.expected += 1;
        if !present {
            tally.missing += 1;
        }
    }

    fn check_nodes(&mut self) -> Result<()> {
        let schema = self.db.schema().clone();
        let mut cache = FragmentCache::default();
        let mut cursor = self.db.nodes.cursor();
        let mut ok = cursor.first(&mut self.db.pager)?;
        while ok {
            let id = table_id(cursor.key())?;
            let bytes = cursor.value(&mut self.db.pager)?;
            ok = cursor.next(&mut self.db.pager)?;
            self.progress.inc(1);
            self.report.stats.nodes += 1;
            let record = match NodeRecord::decode(&bytes) {
                Ok(record) => record,
                Err(err) => {
                    self.report
                        .push(FindingKind::Record, format!("node {id}: {}", err.message()));
                    continue;
                }
            };
            self.node_ids.push(id);
            let label_count = schema.labels.len() as u32;
            if record.labels.windows(2).any(|pair| pair[0] >= pair[1]) {
                self.report.push(
                    FindingKind::Record,
                    format!("node {id} has unsorted or duplicate labels"),
                );
            }
            if let Some(label) = record
                .labels
                .iter()
                .find(|label| **label == 0 || **label > label_count)
            {
                self.report.push(
                    FindingKind::Record,
                    format!("node {id} carries unknown label {label}"),
                );
            }
            if let Some(problem) = props_problem(&record.props, schema.property_keys.len() as u32) {
                self.report
                    .push(FindingKind::Record, format!("node {id} has {problem}"));
            }
            for label in &record.labels {
                let key = FragmentKey::label(*label, id);
                if !self.bit_present(&mut cache, id >> 16, key, FragmentKey::label_bit(id))? {
                    self.report.push(
                        FindingKind::Label,
                        format!(
                            "node {id} is missing from the `{}` label bitmap",
                            schema.label_name(*label).unwrap_or("?")
                        ),
                    );
                }
            }
            self.check_node_entries(&schema, id, &record)?;
        }
        Ok(())
    }

    /// Property index entries, constraints and fulltext postings of a node.
    fn check_node_entries(
        &mut self,
        schema: &crate::catalog::Schema,
        id: u64,
        record: &NodeRecord,
    ) -> Result<()> {
        let applies = |label_id: u32| label_id == 0 || record.labels.contains(&label_id);
        for def in schema
            .node_indexes
            .iter()
            .filter(|def| applies(def.label_id))
        {
            if def.kind == IndexKind::NodeKey
                && let Some(key_id) = def.key_ids.iter().find(|key| record.get(**key).is_none())
            {
                self.report.push(
                    FindingKind::Constraint,
                    format!(
                        "node {id} lacks property `{}` required by NODE KEY `{}`",
                        schema.property_key_name(*key_id).unwrap_or("?"),
                        node_index_name(def)
                    ),
                );
            }
            let Some(mut key) = node_index_prefix(record, def) else {
                continue;
            };
            key.extend_from_slice(&id.to_be_bytes());
            let tree = Btree::from_root(def.root, TreeKind::Index);
            let present = tree.contains(&mut self.db.pager, &key)?;
            self.entry_expected(def.id, present);
            if !present {
                self.report.push(
                    FindingKind::Index,
                    format!("node {id} is missing from index `{}`", node_index_name(def)),
                );
            }
        }
        for def in schema
            .node_constraints
            .iter()
            .filter(|def| applies(def.label_id) && record.get(def.key_id).is_none())
        {
            self.report.push(
                FindingKind::Constraint,
                format!(
                    "node {id} lacks property `{}` required by `{}`",
                    schema.property_key_name(def.key_id).unwrap_or("?"),
                    def.name
                ),
            );
        }
        for def in schema
            .fulltext_indexes
            .iter()
            .filter(|def| record.labels.contains(&def.label_id))
        {
            self.check_postings(def, id, record)?;
        }
        Ok(())
    }

    fn check_postings(
        &mut self,
        def: &FulltextIndexDef,
        id: u64,
        record: &NodeRecord,
    ) -> Result<()> {
        let tree = Btree::from_root(def.root, TreeKind::Index);
        let terms = document_terms(
            def.key_ids
                .iter()
                .filter_map(|key_id| record.get(*key_id).and_then(Value::as_text)),
        );
        for (term, positions) in &terms {
            let stored = tree.get(&mut self.db.pager, &posting_key(term, id))?;
            self.entry_expected(def.id, stored.is_some());
            match stored {
                None => self.report.push(
                    FindingKind::Fulltext,
                    format!(
                        "node {id} is missing term `{term}` from fulltext index `{}`",
                        def.name
                    ),
                ),
                Some(bytes) if bytes != encode_positions(positions) => self.report.push(
                    FindingKind::Fulltext,
                    format!(
                        "node {id} has stale positions for `{term}` in fulltext index `{}`",
                        def.name
                    ),
                ),
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn check_edges(&mut self) -> Result<()> {
        let schema = self.db.schema().clone();
        let type_count = schema.edge_types.len() as u32;
        let mut incidence = FragmentCache::default();
        let mut types = FragmentCache::default();
        let mut cursor = self.db.edges.cursor();
        let mut ok = cursor.first(&mut self.db.pager)?;
        while ok {
            let id = table_id(cursor.key())?;
            let bytes = cursor.value(&mut self.db.pager)?;
            ok = cursor.next(&mut self.db.pager)?;
            self.progress.inc(1);
            self.report.stats.edges += 1;
            let record = match EdgeRecord::decode(&bytes) {
                Ok(record) => record,
                Err(err) => {
                    self.report
                        .push(FindingKind::Record, format!("edge {id}: {}", err.message()));
                    continue;
                }
            };
            if record.edge_type == 0 || record.edge_type > type_count {
                self.report.push(
                    FindingKind::Record,
                    format!("edge {id} has unknown type {}", record.edge_type),
                );
            }
            for (end, node) in [("starts", record.src), ("ends", record.dst)] {
                if self.node_ids.binary_search(&node).is_err() {
                    self.report.push(
                        FindingKind::Record,
                        format!("edge {id} {end} at missing node {node}"),
                    );
                }
            }
            if let Some(problem) = props_problem(&record.props, schema.property_keys.len() as u32) {
                self.report
                    .push(FindingKind::Record, format!("edge {id} has {problem}"));
            }
            for (direction, node) in [(Direction::Out, record.src), (Direction::In, record.dst)] {
                for type_id in [record.edge_type, ANY_TYPE] {
                    let key = FragmentKey::incidence(direction, type_id, node, id);
                    let bit = FragmentKey::incidence_bit(node, id);
                    if !self.bit_present(&mut incidence, id >> 8, key, bit)? {
                        self.report.push(
                            FindingKind::Incidence,
                            format!(
                                "edge {id} is missing its {direction:?} bit at node {node} \
                                 in the {} plane",
                                if type_id == ANY_TYPE {
                                    "any-type"
                                } else {
                                    "typed"
                                }
                            ),
                        );
                    }
                }
            }
            let key = FragmentKey::edge_type(record.edge_type, id);
            if !self.bit_present(&mut types, id >> 16, key, FragmentKey::edge_type_bit(id))? {
                self.report.push(
                    FindingKind::EdgeType,
                    format!("edge {id} is missing from its edge-type bitmap"),
                );
            }
            self.check_edge_entries(&schema, id, &record)?;
        }
        Ok(())
    }

    fn check_edge_entries(
        &mut self,
        schema: &crate::catalog::Schema,
        id: u64,
        record: &EdgeRecord,
    ) -> Result<()> {
        let applies = |type_id: u32| type_id == 0 || type_id == record.edge_type;
        for def in schema
            .edge_indexes
            .iter()
            .filter(|def| applies(def.type_id))
        {
            let Some(mut key) = edge_index_prefix(record, def) else {
                continue;
            };
            key.extend_from_slice(&id.to_be_bytes());
            let tree = Btree::from_root(def.root, TreeKind::Index);
            let present = tree.contains(&mut self.db.pager, &key)?;
            self.entry_expected(def.id, present);
            if !present {
                self.report.push(
                    FindingKind::Index,
                    format!("edge {id} is missing from index `{}`", edge_index_name(def)),
                );
            }
        }
        for def in schema
            .edge_constraints
            .iter()
            .filter(|def| applies(def.type_id) && record.get(def.key_id).is_none())
        {
            self.report.push(
                FindingKind::Constraint,
                format!(
                    "edge {id} lacks property `{}` required by `{}`",
                    schema.property_key_name(def.key_id).unwrap_or("?"),
                    def.name
                ),
            );
        }
        Ok(())
    }

    /// Compare stored bit totals with the record-implied totals and name the
    /// extra bits of any family that holds more than its records imply.
    fn compare_fragment_totals(&mut self) -> Result<()> {
        let mut families: Vec<(u8, u32)> = self
            .stored_bits
            .keys()
            .chain(self.bits.keys())
            .copied()
            .collect();
        families.sort_unstable();
        families.dedup();
        for family in families {
            let stored = self.stored_bits.get(&family).copied().unwrap_or(0);
            let extras = self
                .bits
                .get(&family)
                .copied()
                .unwrap_or_default()
                .extras(stored);
            if extras > 0 {
                self.locate_extra_bits(family, extras)?;
            }
        }
        Ok(())
    }

    fn locate_extra_bits(&mut self, (family, family_id): (u8, u32), extras: u64) -> Result<()> {
        let kind = FindingKind::of_family(family);
        let tree = Btree::from_root(self.db.fragments.root(), TreeKind::Index);
        let prefix = &FragmentKey::prefix(family, family_id, 0)[..5];
        let mut found = 0u64;
        let mut cursor = tree.cursor();
        let mut ok = cursor.seek_ge(&mut self.db.pager, prefix)?;
        while ok && found < extras && cursor.key().starts_with(prefix) {
            let key = FragmentKey::decode(cursor.key());
            let fragment = Fragment::decode(&cursor.value(&mut self.db.pager)?);
            ok = cursor.next(&mut self.db.pager)?;
            // Undecodable entries were reported by the fragment scan.
            let (Ok(key), Ok(fragment)) = (key, fragment) else {
                continue;
            };
            for bit in fragment.iter_bits() {
                if let Some(problem) = self.stray_bit(&key, bit)? {
                    found += 1;
                    self.report.push(kind, problem);
                }
            }
        }
        if found < extras {
            self.report.push(
                kind,
                format!(
                    "family {family} id {family_id} stores {} more bits than its records imply",
                    extras - found
                ),
            );
        }
        Ok(())
    }

    /// Why a stored bit is implied by no record, if it is not.
    fn stray_bit(&mut self, key: &FragmentKey, bit: u16) -> Result<Option<String>> {
        let bit = u64::from(bit);
        match key.family {
            FAMILY_LABEL => {
                let node = (u64::from(key.band0) << 16) | bit;
                let record = corrupt_as_none(self.db.get_node(node))?;
                Ok(
                    (!record.is_some_and(|record| record.labels.contains(&key.family_id))).then(
                        || {
                            format!(
                                "label {} bitmap lists node {node}, which lacks the label",
                                key.family_id
                            )
                        },
                    ),
                )
            }
            FAMILY_EDGE_TYPE => {
                let edge = (u64::from(key.band0) << 16) | bit;
                let record = corrupt_as_none(self.db.get_edge(edge))?;
                Ok(
                    (!record.is_some_and(|record| record.edge_type == key.family_id)).then(|| {
                        format!(
                            "edge-type {} bitmap lists edge {edge}, which has another type \
                                 or does not exist",
                            key.family_id
                        )
                    }),
                )
            }
            _ => {
                let node = (u64::from(key.band0) << 8) | (bit >> 8);
                let edge = (u64::from(key.band1) << 8) | (bit & 255);
                let record = corrupt_as_none(self.db.get_edge(edge))?;
                let matches = record.is_some_and(|record| {
                    let end = if key.family == FAMILY_OUT {
                        record.src
                    } else {
                        record.dst
                    };
                    end == node && (key.family_id == ANY_TYPE || record.edge_type == key.family_id)
                });
                let direction = if key.family == FAMILY_OUT {
                    "out"
                } else {
                    "in"
                };
                Ok((!matches).then(|| {
                    format!(
                        "{direction} plane {} links node {node} to edge {edge}, \
                         which does not have that endpoint and type",
                        key.family_id
                    )
                }))
            }
        }
    }

    /// Compare stored entry totals with record-implied totals, name stale
    /// entries, and verify uniqueness of unique indexes.
    fn compare_index_totals(&mut self) -> Result<()> {
        let schema = self.db.schema().clone();
        for def in &schema.node_indexes {
            self.compare_index(IndexTarget::Node(def), def.id, def.root, def.unique())?;
        }
        for def in &schema.edge_indexes {
            self.compare_index(IndexTarget::Edge(def), def.id, def.root, def.unique())?;
        }
        for def in &schema.fulltext_indexes {
            let Some(stored) = self.trees.get(&def.root).map(|stats| stats.keys) else {
                continue;
            };
            self.report.stats.postings += stored;
            let extras = self.tally(def.id).extras(stored);
            if extras > 0 {
                self.locate_stale_postings(def, extras)?;
            }
        }
        Ok(())
    }

    fn tally(&self, object_id: u32) -> Tally {
        self.entries.get(&object_id).copied().unwrap_or_default()
    }

    fn compare_index(
        &mut self,
        target: IndexTarget<'_>,
        id: u32,
        root: PageNo,
        unique: bool,
    ) -> Result<()> {
        let Some(stored) = self.trees.get(&root).map(|stats| stats.keys) else {
            return Ok(());
        };
        self.report.stats.index_entries += stored;
        let extras = self.tally(id).extras(stored);
        if extras == 0 && !unique {
            return Ok(());
        }
        let (name, noun) = match &target {
            IndexTarget::Node(def) => (node_index_name(def), "node"),
            IndexTarget::Edge(def) => (edge_index_name(def), "edge"),
        };
        let tree = Btree::from_root(root, TreeKind::Index);
        let mut found = 0u64;
        let mut previous: Option<(Vec<u8>, u64)> = None;
        let mut cursor = tree.cursor();
        let mut ok = cursor.first(&mut self.db.pager)?;
        while ok {
            let key = cursor.key().to_vec();
            ok = cursor.next(&mut self.db.pager)?;
            let entity = index_entry_id(&key)?;
            let value = &key[..key.len() - 8];
            if unique
                && let Some((previous_value, previous_entity)) = &previous
                && previous_value.as_slice() == value
            {
                self.report.push(
                    FindingKind::Constraint,
                    format!(
                        "unique index `{name}` holds one value for {noun}s \
                         {previous_entity} and {entity}"
                    ),
                );
            }
            if found < extras && self.expected_index_key(&target, entity)? != Some(key.clone()) {
                found += 1;
                self.report.push(
                    FindingKind::Index,
                    format!("index `{name}` holds a stale entry for {noun} {entity}"),
                );
            }
            previous = Some((value.to_vec(), entity));
        }
        Ok(())
    }

    /// The index key a record implies for `entity`, or `None` when the
    /// entity is gone or not indexed.
    fn expected_index_key(
        &mut self,
        target: &IndexTarget<'_>,
        entity: u64,
    ) -> Result<Option<Vec<u8>>> {
        let prefix = match target {
            IndexTarget::Node(def) => corrupt_as_none(self.db.get_node(entity))?
                .filter(|record| def.label_id == 0 || record.labels.contains(&def.label_id))
                .and_then(|record| node_index_prefix(&record, def)),
            IndexTarget::Edge(def) => corrupt_as_none(self.db.get_edge(entity))?
                .filter(|record| def.type_id == 0 || record.edge_type == def.type_id)
                .and_then(|record| edge_index_prefix(&record, def)),
        };
        Ok(prefix.map(|mut key| {
            key.extend_from_slice(&entity.to_be_bytes());
            key
        }))
    }

    fn locate_stale_postings(&mut self, def: &FulltextIndexDef, extras: u64) -> Result<()> {
        let tree = Btree::from_root(def.root, TreeKind::Index);
        let mut found = 0u64;
        let mut cursor = tree.cursor();
        let mut ok = cursor.first(&mut self.db.pager)?;
        while ok && found < extras {
            let key = cursor.key().to_vec();
            ok = cursor.next(&mut self.db.pager)?;
            let node = posting_node(&key)?;
            // Posting keys are `term 0x00 node-id`.
            let term = &key[..key.len() - 9];
            let implied = corrupt_as_none(self.db.get_node(node))?
                .filter(|record| record.labels.contains(&def.label_id))
                .is_some_and(|record| {
                    document_terms(
                        def.key_ids
                            .iter()
                            .filter_map(|key_id| record.get(*key_id).and_then(Value::as_text)),
                    )
                    .iter()
                    .any(|(stored, _)| stored.as_bytes() == term)
                });
            if !implied {
                found += 1;
                self.report.push(
                    FindingKind::Fulltext,
                    format!(
                        "fulltext index `{}` holds a stale posting `{}` for node {node}",
                        def.name,
                        String::from_utf8_lossy(term)
                    ),
                );
            }
        }
        Ok(())
    }
}

/// Free a derived tree's pages when its structure verified against
/// `owned`; otherwise leave the pages alone rather than risk freeing a page
/// another tree still uses.
fn release_tree(
    db: &mut Database,
    owned: &mut HashSet<PageNo>,
    name: String,
    tree: Btree,
    summary: &mut RebuildSummary,
) -> Result<()> {
    match tree.integrity_check_into(&mut db.pager, owned) {
        Ok(_) => tree.destroy(&mut db.pager)?,
        Err(err) if err.code() == ErrorCode::Corrupt => {
            summary.abandoned.push(format!("{name}: {}", err.message()));
        }
        Err(err) => return Err(err),
    }
    summary.rebuilt.push(name);
    Ok(())
}

/// Verify a primary tree, returning its record count.
fn verify_primary(
    db: &mut Database,
    owned: &mut HashSet<PageNo>,
    name: &str,
    tree: Btree,
) -> Result<u64> {
    let stats = tree
        .integrity_check_into(&mut db.pager, owned)
        .map_err(|err| {
            Error::corrupt(format!(
                "the {name} is damaged ({}); primary data cannot be rebuilt",
                err.message()
            ))
        })?;
    Ok(stats.keys)
}

fn rebuild(db: &mut Database, progress: &ProgressBar) -> Result<RebuildSummary> {
    progress.set_message("verifying primary records");
    let mut owned: HashSet<PageNo> = HashSet::from([1]);
    let catalog = Btree::from_root(db.catalog.root(), TreeKind::Table);
    let (nodes, edges) = (db.nodes, db.edges);
    verify_primary(db, &mut owned, "catalog", catalog)?;
    let records = verify_primary(db, &mut owned, "node table", nodes)?
        + verify_primary(db, &mut owned, "edge table", edges)?;
    // Reusing pages from a damaged freelist could overwrite live records.
    let free = db.pager.freelist_pages().map_err(|err| {
        Error::corrupt(format!(
            "the freelist is damaged ({}); rebuilding would reuse live pages",
            err.message()
        ))
    })?;
    if let Some(page) = free.iter().find(|page| owned.contains(page)) {
        return Err(Error::corrupt(format!(
            "free page {page} is still owned by a primary tree; rebuilding would reuse it"
        )));
    }
    owned.extend(free);

    let mut summary = RebuildSummary::default();
    let fragments = Btree::from_root(db.fragments.root(), TreeKind::Index);
    release_tree(
        db,
        &mut owned,
        "fragment store".to_owned(),
        fragments,
        &mut summary,
    )?;
    db.fragments = FragmentStore::create(&mut db.pager)?;
    progress.set_length(records);
    progress.set_message("rebuilding bitmaps");
    let mut cursor = db.nodes.cursor();
    let mut ok = cursor.first(&mut db.pager)?;
    while ok {
        let id = table_id(cursor.key())?;
        let record = NodeRecord::decode(&cursor.value(&mut db.pager)?)?;
        for label in &record.labels {
            let key = FragmentKey::label(*label, id);
            db.fragments
                .set_bit(&mut db.pager, &key, FragmentKey::label_bit(id))?;
        }
        progress.inc(1);
        ok = cursor.next(&mut db.pager)?;
    }
    let mut cursor = db.edges.cursor();
    let mut ok = cursor.first(&mut db.pager)?;
    while ok {
        let id = table_id(cursor.key())?;
        let record = EdgeRecord::decode(&cursor.value(&mut db.pager)?)?;
        db.set_incidence(&record, id, true)?;
        progress.inc(1);
        ok = cursor.next(&mut db.pager)?;
    }

    progress.set_message("rebuilding indexes");
    let schema = db.schema().clone();
    for def in &schema.node_indexes {
        let old = Btree::from_root(def.root, TreeKind::Index);
        let name = format!("node index `{}`", node_index_name(def));
        release_tree(db, &mut owned, name, old, &mut summary)?;
        let root = Btree::create(&mut db.pager, TreeKind::Index)?.root();
        db.replace_node_index_root(def.id, root)?;
        db.backfill_node_index(def.id)?;
    }
    for def in &schema.edge_indexes {
        let old = Btree::from_root(def.root, TreeKind::Index);
        let name = format!("edge index `{}`", edge_index_name(def));
        release_tree(db, &mut owned, name, old, &mut summary)?;
        let root = Btree::create(&mut db.pager, TreeKind::Index)?.root();
        db.replace_edge_index_root(def.id, root)?;
        db.backfill_edge_index(def.id)?;
    }
    for def in &schema.fulltext_indexes {
        let old = Btree::from_root(def.root, TreeKind::Index);
        let name = format!("fulltext index `{}`", def.name);
        release_tree(db, &mut owned, name, old, &mut summary)?;
        let root = Btree::create(&mut db.pager, TreeKind::Index)?.root();
        db.catalog.set_fulltext_root(&mut db.pager, def.id, root)?;
        db.backfill_fulltext_index(def.id)?;
    }
    progress.finish_and_clear();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::vfs::mem::MemVfs;

    /// Ids of the objects [`populated`] creates.
    struct Fixture {
        alice: u64,
        bob: u64,
        knows: u64,
        person: u32,
        knows_type: u32,
        name: u32,
        name_index: u32,
        fulltext: u32,
    }

    /// A small graph with a unique index, an edge index, a NOT NULL
    /// constraint and a fulltext index.
    fn populated() -> (Database, Fixture) {
        let mut db =
            Database::create(Arc::new(MemVfs::new()), Path::new("check.db"), 4096).expect("create");
        let fixture = db
            .write(|db| {
                let person = db.ensure_label("Person")?;
                let knows_type = db.ensure_edge_type("KNOWS")?;
                let name = db.ensure_property_key("name")?;
                let since = db.ensure_property_key("since")?;
                let alice = db.create_node(&[person], vec![(name, Value::from("alice smith"))])?;
                let bob = db.create_node(&[person], vec![(name, Value::from("bob"))])?;
                let knows =
                    db.create_edge(knows_type, alice, bob, vec![(since, Value::Int(2020))])?;
                let name_index = db.create_node_index("name", Some("Person"), true)?;
                db.create_edge_index("since", Some("KNOWS"), false)?;
                db.catalog
                    .create_node_constraint(&mut db.pager, "person_name", person, name)?;
                let fulltext =
                    db.catalog
                        .create_fulltext_index(&mut db.pager, "names", person, &[name])?;
                db.backfill_fulltext_index(fulltext)?;
                Ok(Fixture {
                    alice,
                    bob,
                    knows,
                    person,
                    knows_type,
                    name,
                    name_index,
                    fulltext,
                })
            })
            .expect("populate");
        (db, fixture)
    }

    fn kinds(report: &IntegrityReport) -> Vec<FindingKind> {
        let mut kinds: Vec<FindingKind> = report.findings().iter().map(Finding::kind).collect();
        kinds.dedup();
        kinds
    }

    /// Assert the report flags exactly `expected`, then that a rebuild
    /// repairs the damage.
    fn assert_repairs(db: &mut Database, expected: &[FindingKind]) {
        let report = db.check_integrity().expect("check");
        assert_eq!(kinds(&report), expected, "{report}");
        db.rebuild_derived(&ProgressBar::hidden()).expect("rebuild");
        let report = db.check_integrity().expect("recheck");
        assert!(report.is_clean(), "{report}");
    }

    fn node_index_tree(db: &Database, id: u32) -> Btree {
        let def = db
            .schema()
            .node_indexes
            .iter()
            .find(|def| def.id == id)
            .expect("index exists");
        Btree::from_root(def.root, TreeKind::Index)
    }

    #[test]
    fn a_consistent_database_is_clean() {
        let (mut db, _) = populated();
        let report = db.check_integrity().expect("check");
        assert!(report.is_clean(), "{report}");
        let stats = report.stats();
        assert_eq!((stats.nodes, stats.edges), (2, 1));
        // Unique name index: 2 entries, edge index: 1.
        assert_eq!(stats.index_entries, 3);
        // "alice", "smith", "bob".
        assert_eq!(stats.postings, 3);
        assert!(stats.fragments > 0 && stats.pages > 1);
        assert!(report.to_json().contains("\"findings\":[]"));
    }

    #[test]
    fn missing_and_stray_label_bits_are_found() {
        let (mut db, fx) = populated();
        db.write(|db| {
            let key = FragmentKey::label(fx.person, fx.alice);
            db.fragments
                .clear_bit(&mut db.pager, &key, FragmentKey::label_bit(fx.alice))?;
            let stray = FragmentKey::label(fx.person, 999);
            db.fragments
                .set_bit(&mut db.pager, &stray, FragmentKey::label_bit(999))
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        let messages: Vec<&str> = report.findings().iter().map(Finding::message).collect();
        assert!(
            messages[0].contains("node 1 is missing from the `Person`"),
            "{report}"
        );
        assert!(messages[1].contains("lists node 999"), "{report}");
        assert_repairs(&mut db, &[FindingKind::Label]);
    }

    #[test]
    fn foreign_incidence_bits_are_found() {
        let (mut db, fx) = populated();
        // The 0.2.1 band bug in reverse: bob's row claims alice's edge.
        db.write(|db| {
            for type_id in [fx.knows_type, ANY_TYPE] {
                let key = FragmentKey::incidence(Direction::Out, type_id, fx.bob, fx.knows);
                db.fragments.set_bit(
                    &mut db.pager,
                    &key,
                    FragmentKey::incidence_bit(fx.bob, fx.knows),
                )?;
            }
            Ok(())
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        assert_eq!(report.findings().len(), 2, "{report}");
        assert!(
            report.findings()[0]
                .message()
                .contains("links node 2 to edge 1")
        );
        assert_repairs(&mut db, &[FindingKind::Incidence]);
    }

    #[test]
    fn missing_edge_type_bits_are_found() {
        let (mut db, fx) = populated();
        db.write(|db| {
            let key = FragmentKey::edge_type(fx.knows_type, fx.knows);
            db.fragments
                .clear_bit(&mut db.pager, &key, FragmentKey::edge_type_bit(fx.knows))
        })
        .expect("damage");
        assert_repairs(&mut db, &[FindingKind::EdgeType]);
    }

    #[test]
    fn missing_and_stale_index_entries_are_found() {
        let (mut db, fx) = populated();
        db.write(|db| {
            let mut tree = node_index_tree(db, fx.name_index);
            let mut alice = Vec::new();
            crate::record::encode_index_key(&mut alice, &Value::from("alice smith"));
            alice.extend_from_slice(&fx.alice.to_be_bytes());
            assert!(tree.delete(&mut db.pager, &alice)?);
            let mut stale = Vec::new();
            crate::record::encode_index_key(&mut stale, &Value::from("carol"));
            stale.extend_from_slice(&fx.bob.to_be_bytes());
            tree.insert(&mut db.pager, &stale, &[])?;
            db.replace_node_index_root(fx.name_index, tree.root())
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        assert_eq!(report.findings().len(), 2, "{report}");
        assert!(
            report.findings()[1]
                .message()
                .contains("stale entry for node 2")
        );
        assert_repairs(&mut db, &[FindingKind::Index]);
    }

    #[test]
    fn duplicate_values_in_a_unique_index_are_found() {
        let (mut db, fx) = populated();
        db.write(|db| {
            let mut tree = node_index_tree(db, fx.name_index);
            let mut duplicate = Vec::new();
            crate::record::encode_index_key(&mut duplicate, &Value::from("bob"));
            duplicate.extend_from_slice(&fx.alice.to_be_bytes());
            tree.insert(&mut db.pager, &duplicate, &[])?;
            db.replace_node_index_root(fx.name_index, tree.root())
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        assert!(
            report
                .findings()
                .iter()
                .any(|finding| finding.kind() == FindingKind::Constraint
                    && finding.message().contains("nodes 1 and 2")),
            "{report}"
        );
        assert_repairs(&mut db, &[FindingKind::Index, FindingKind::Constraint]);
    }

    #[test]
    fn existence_violations_and_dangling_edges_are_found() {
        let (mut db, fx) = populated();
        // Rewrite records behind the engine's back: alice loses her name and
        // bob's record disappears while the KNOWS edge still points at him.
        db.write(|db| {
            let mut alice = db.get_node(fx.alice)?.expect("alice");
            alice.remove(fx.name);
            let key = fx.alice.to_be_bytes();
            db.nodes.delete(&mut db.pager, &key)?;
            db.nodes.insert(&mut db.pager, &key, &alice.encode())?;
            db.nodes.delete(&mut db.pager, &fx.bob.to_be_bytes())?;
            Ok(())
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        let found = kinds(&report);
        assert!(found.contains(&FindingKind::Constraint), "{report}");
        assert!(
            report
                .findings()
                .iter()
                .any(|finding| finding.message() == "edge 1 ends at missing node 2"),
            "{report}"
        );
        // Derived structures are repaired; the primary damage remains.
        db.rebuild_derived(&ProgressBar::hidden()).expect("rebuild");
        let report = db.check_integrity().expect("recheck");
        assert_eq!(
            kinds(&report),
            vec![FindingKind::Constraint, FindingKind::Record],
            "{report}"
        );
    }

    #[test]
    fn missing_and_stale_postings_are_found() {
        let (mut db, fx) = populated();
        db.write(|db| {
            let def = db
                .schema()
                .fulltext_indexes
                .iter()
                .find(|def| def.id == fx.fulltext)
                .cloned()
                .expect("fulltext");
            let mut tree = Btree::from_root(def.root, TreeKind::Index);
            assert!(tree.delete(&mut db.pager, &posting_key("smith", fx.alice))?);
            tree.insert(&mut db.pager, &posting_key("ghost", fx.bob), &[1, 0])?;
            db.catalog
                .set_fulltext_root(&mut db.pager, def.id, tree.root())
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        assert_eq!(report.findings().len(), 2, "{report}");
        assert!(
            report.findings()[1].message().contains("`ghost`"),
            "{report}"
        );
        assert_repairs(&mut db, &[FindingKind::Fulltext]);
    }

    #[test]
    fn leaked_and_doubly_owned_pages_are_found() {
        let (mut db, _) = populated();
        let leaked = db
            .write(|db| {
                let page = db.pager.alloc_page()?;
                // An initialised but unreferenced page.
                crate::btree::Btree::create(&mut db.pager, TreeKind::Index)?;
                Ok(page)
            })
            .expect("leak");
        let report = db.check_integrity().expect("check");
        assert_eq!(kinds(&report), vec![FindingKind::Pages], "{report}");
        assert!(
            report.findings()[0]
                .message()
                .contains(&format!("page {leaked} "))
        );
        // Freeing a live page puts it on the freelist while a tree owns it.
        let root = db.nodes.root();
        db.write(|db| db.pager.free_page(root)).expect("double");
        let report = db.check_integrity().expect("check");
        assert!(
            report
                .findings()
                .iter()
                .any(|finding| finding.message().contains("on the freelist but owned")),
            "{report}"
        );
        let err = db
            .rebuild_derived(&ProgressBar::hidden())
            .expect_err("rebuild refuses to reuse a live page");
        assert_eq!(err.code(), ErrorCode::Corrupt);
    }

    #[test]
    fn stale_header_counters_are_found() {
        let (mut db, _) = populated();
        db.write(|db| {
            db.pager.update_header(|header| {
                header.next_node_id = 2;
                header.next_label_id = 7;
            });
            Ok(())
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        assert_eq!(kinds(&report), vec![FindingKind::Header], "{report}");
        assert_eq!(report.findings().len(), 2, "{report}");
    }

    #[test]
    fn findings_are_capped_per_kind() {
        let mut db =
            Database::create(Arc::new(MemVfs::new()), Path::new("cap.db"), 4096).expect("create");
        db.write(|db| {
            let label = db.ensure_label("L")?;
            for _ in 0..150 {
                let id = db.create_node(&[label], vec![])?;
                db.fragments.clear_bit(
                    &mut db.pager,
                    &FragmentKey::label(label, id),
                    FragmentKey::label_bit(id),
                )?;
            }
            Ok(())
        })
        .expect("damage");
        let report = db.check_integrity().expect("check");
        assert_eq!(report.findings().len(), MAX_FINDINGS_PER_KIND);
        assert_eq!(report.suppressed(), 50);
        assert!(
            report
                .to_string()
                .contains("label: 50 more findings not shown")
        );
    }

    #[test]
    fn checking_inside_a_write_transaction_sees_cached_fragments() {
        let (mut db, fx) = populated();
        db.begin_write().expect("begin");
        db.create_node(&[fx.person], vec![(fx.name, Value::from("carol"))])
            .expect("node");
        let report = db.check_integrity().expect("check");
        assert!(report.is_clean(), "{report}");
        db.rollback().expect("rollback");
        assert!(db.check_integrity().expect("check").is_clean());
    }
}
