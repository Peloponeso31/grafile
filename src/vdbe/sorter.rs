//! External merge sort for unbounded `ORDER BY` and `DISTINCT`.
//!
//! Rows buffer in memory until they pass the database's sort budget; each
//! full buffer is stable-sorted and written as a run to a spill file next to
//! the database, through its VFS. Finishing a sort that never spilled sorts
//! the buffer in place; otherwise a k-way merge streams rows from every run.
//! Ties between runs go to the earlier run, so the output equals a stable
//! sort of the whole input, and `DISTINCT` drops a row equal to the last one
//! kept, as `sort_by` followed by `dedup_by` does.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use super::{Reg, compare_keys};
use crate::error::{Error, Result};
use crate::record::{Value, decode_value, decode_varint, encode_value, encode_varint};
use crate::vfs::{File, OpenMode, Vfs};

/// Bytes written or read per spill-file I/O call.
const BLOCK: usize = 64 << 10;

/// Spill-only tags, above every record tag, for values the record format
/// refuses to store and for the two entity registers.
mod tag {
    pub const REG_NODE: u8 = 0xF0;
    pub const REG_EDGE: u8 = 0xF1;
    pub const LIST: u8 = 0xF2;
    pub const MAP: u8 = 0xF3;
    pub const NODE: u8 = 0xF4;
    pub const EDGE: u8 = 0xF5;
}

/// Distinguishes spill files of concurrent sorts in one process.
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Runs spilled on this thread, so tests can tell a merge from an
    /// in-memory sort.
    pub(crate) static SPILLED_RUNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Where a sorter writes its runs.
pub(super) struct SpillTarget {
    pub vfs: Arc<dyn Vfs>,
    pub database: PathBuf,
}

/// Rows of an unbounded sort or `DISTINCT` in progress.
pub(super) struct Sorter {
    target: SpillTarget,
    budget: usize,
    dedup: bool,
    rows: Vec<Vec<Reg>>,
    buffered: usize,
    runs: Vec<RunFile>,
}

/// A finished sort: still in memory, or merged from spilled runs.
pub(super) enum Sorted {
    Rows(Vec<Vec<Reg>>),
    Merge(Merge),
}

impl Sorter {
    /// A sorter that spills once its rows pass `budget` bytes; `dedup`
    /// drops rows equal to the previous one after sorting.
    pub fn new(target: SpillTarget, budget: usize, dedup: bool) -> Self {
        Self {
            target,
            budget,
            dedup,
            rows: Vec::new(),
            buffered: 0,
            runs: Vec::new(),
        }
    }

    /// Buffer a row, spilling the buffer as a sorted run when it passes the
    /// budget. `prefix` leading columns ride along; the rest are sort keys.
    pub fn push(&mut self, row: &[Reg], prefix: usize, ascending: &[bool]) -> Result<()> {
        self.buffered += row_size(row);
        self.rows.push(row.to_vec());
        if self.buffered > self.budget {
            self.spill(prefix, ascending)?;
        }
        Ok(())
    }

    // Out of line and cold: most sorts never spill, and inlining the spill
    // into the executor's dispatch loop slows every buffered row.
    #[cold]
    #[inline(never)]
    fn spill(&mut self, prefix: usize, ascending: &[bool]) -> Result<()> {
        sort_run(&mut self.rows, prefix, ascending, self.dedup);
        let run = RunFile::write(&self.target, &self.rows)?;
        self.runs.push(run);
        self.rows.clear();
        self.buffered = 0;
        #[cfg(test)]
        SPILLED_RUNS.with(|count| count.set(count.get() + 1));
        Ok(())
    }

    /// Sort what remains in memory and return every row in order.
    ///
    /// # Errors
    ///
    /// Propagates spill-file I/O and decoding errors.
    pub fn finish(mut self, prefix: usize, ascending: &[bool]) -> Result<Sorted> {
        sort_run(&mut self.rows, prefix, ascending, self.dedup);
        if self.runs.is_empty() {
            return Ok(Sorted::Rows(self.rows));
        }
        Merge::new(self, prefix, ascending).map(Sorted::Merge)
    }
}

/// Stable-sort a run by its key columns (one per `ascending` entry after
/// `prefix`) and optionally drop rows whose keys equal the previous row's. `==` is structural and transitive, so deduplicating each run before
/// the merge leaves the same rows as deduplicating the merged stream.
fn sort_run(rows: &mut Vec<Vec<Reg>>, prefix: usize, ascending: &[bool], dedup: bool) {
    use rayon::slice::ParallelSliceMut;
    // Stable and parallel; rayon sorts short slices sequentially itself.
    rows.par_sort_by(|a, b| compare_keys(&a[prefix..], &b[prefix..], ascending));
    if dedup {
        let keys = prefix..prefix + ascending.len();
        rows.dedup_by(|a, b| a[keys.clone()] == b[keys.clone()]);
    }
}

/// Approximate heap and inline bytes held by a buffered row.
fn row_size(row: &[Reg]) -> usize {
    size_of::<Vec<Reg>>()
        + row
            .iter()
            .map(|reg| match reg {
                Reg::Value(value) => size_of::<Reg>() + value_heap(value),
                Reg::Node(_) | Reg::Edge(_) => size_of::<Reg>(),
            })
            .sum::<usize>()
}

fn value_heap(value: &Value) -> usize {
    match value {
        Value::Text(text) => text.len(),
        Value::Bytes(bytes) => bytes.len(),
        Value::List(items) => items
            .iter()
            .map(|item| size_of::<Value>() + value_heap(item))
            .sum(),
        Value::Map(entries) => entries
            .iter()
            .map(|(key, item)| size_of::<(Arc<str>, Value)>() + key.len() + value_heap(item))
            .sum(),
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Node(_)
        | Value::Edge(_)
        | Value::Date(_)
        | Value::LocalTime(_)
        | Value::Time(_)
        | Value::LocalDateTime(_)
        | Value::DateTime(_)
        | Value::Duration(_)
        | Value::Point(_) => 0,
    }
}

fn encode_reg(out: &mut Vec<u8>, reg: &Reg) {
    match reg {
        Reg::Value(value) => encode_spill_value(out, value),
        Reg::Node(id) => {
            out.push(tag::REG_NODE);
            encode_varint(out, *id);
        }
        Reg::Edge(id) => {
            out.push(tag::REG_EDGE);
            encode_varint(out, *id);
        }
    }
}

/// Encode a value, including the maps and entity references that record
/// encoding refuses; storable leaves use the record encoding.
fn encode_spill_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::List(items) => {
            out.push(tag::LIST);
            encode_varint(out, items.len() as u64);
            for item in items.iter() {
                encode_spill_value(out, item);
            }
        }
        Value::Map(entries) => {
            out.push(tag::MAP);
            encode_varint(out, entries.len() as u64);
            for (key, item) in entries.iter() {
                encode_varint(out, key.len() as u64);
                out.extend_from_slice(key.as_bytes());
                encode_spill_value(out, item);
            }
        }
        Value::Node(id) => {
            out.push(tag::NODE);
            encode_varint(out, *id);
        }
        Value::Edge(id) => {
            out.push(tag::EDGE);
            encode_varint(out, *id);
        }
        other => encode_value(out, other),
    }
}

fn decode_reg(input: &[u8], pos: &mut usize) -> Result<Reg> {
    match input.get(*pos) {
        Some(&tag::REG_NODE) => {
            *pos += 1;
            Ok(Reg::Node(decode_varint(input, pos)?))
        }
        Some(&tag::REG_EDGE) => {
            *pos += 1;
            Ok(Reg::Edge(decode_varint(input, pos)?))
        }
        _ => decode_spill_value(input, pos).map(Reg::Value),
    }
}

fn decode_spill_value(input: &[u8], pos: &mut usize) -> Result<Value> {
    let Some(&tag) = input.get(*pos) else {
        return Err(Error::corrupt("truncated sort spill value"));
    };
    match tag {
        tag::LIST => {
            *pos += 1;
            let count = decode_count(input, pos)?;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_spill_value(input, pos)?);
            }
            Ok(Value::List(items.into()))
        }
        tag::MAP => {
            *pos += 1;
            let count = decode_count(input, pos)?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let len = decode_count(input, pos)?;
                let key = input
                    .get(*pos..*pos + len)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .ok_or_else(|| Error::corrupt("invalid sort spill map key"))?;
                *pos += len;
                entries.push((Arc::from(key), decode_spill_value(input, pos)?));
            }
            Ok(Value::Map(entries.into()))
        }
        tag::NODE => {
            *pos += 1;
            Ok(Value::Node(decode_varint(input, pos)?))
        }
        tag::EDGE => {
            *pos += 1;
            Ok(Value::Edge(decode_varint(input, pos)?))
        }
        _ => decode_value(input, pos),
    }
}

/// A count bounded by the remaining input, so corrupt data cannot request a
/// huge allocation.
fn decode_count(input: &[u8], pos: &mut usize) -> Result<usize> {
    usize::try_from(decode_varint(input, pos)?)
        .ok()
        .filter(|count| *count <= input.len().saturating_sub(*pos))
        .ok_or_else(|| Error::corrupt("sort spill count exceeds its row"))
}

/// One spilled run on disk; the file is removed when this is dropped,
/// whether the sort finished, failed or was abandoned.
struct RunFile {
    vfs: Arc<dyn Vfs>,
    path: PathBuf,
    /// `Option` so `Drop` can close the handle before removing the file
    /// (Windows refuses to delete an open file).
    file: Option<Box<dyn File>>,
    len: u64,
}

impl RunFile {
    /// Write `rows` as length-prefixed encoded rows in `BLOCK`-sized writes.
    fn write(target: &SpillTarget, rows: &[Vec<Reg>]) -> Result<Self> {
        let path = spill_path(&target.database);
        let file = target.vfs.open(&path, OpenMode::Create)?;
        let mut run = Self {
            vfs: Arc::clone(&target.vfs),
            path,
            file: Some(file),
            len: 0,
        };
        let file = run.file.as_deref().expect("a run being written is open");
        // A crashed process may have left a longer file under a reused pid.
        file.set_len(0)?;
        let mut block = Vec::with_capacity(BLOCK * 2);
        let mut row_bytes = Vec::new();
        for row in rows {
            row_bytes.clear();
            for reg in row {
                encode_reg(&mut row_bytes, reg);
            }
            encode_varint(&mut block, row.len() as u64);
            encode_varint(&mut block, row_bytes.len() as u64);
            block.extend_from_slice(&row_bytes);
            if block.len() >= BLOCK {
                file.write_at(&block, run.len)?;
                run.len += block.len() as u64;
                block.clear();
            }
        }
        if !block.is_empty() {
            file.write_at(&block, run.len)?;
            run.len += block.len() as u64;
        }
        Ok(run)
    }
}

impl Drop for RunFile {
    fn drop(&mut self) {
        drop(self.file.take());
        // Nothing can report an error from `drop`; a leftover file only
        // wastes space and is truncated if its name is ever reused.
        let _ = self.vfs.remove(&self.path);
    }
}

/// `<database>-sort-<pid>-<n>`, next to the database like its journal.
fn spill_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(format!(
        "-sort-{}-{}",
        std::process::id(),
        NEXT_RUN.fetch_add(1, AtomicOrdering::Relaxed)
    ));
    PathBuf::from(name)
}

/// Sequential reader over one run, buffering a block at a time.
struct RunReader {
    run: RunFile,
    buffer: Vec<u8>,
    pos: usize,
    offset: u64,
}

impl RunReader {
    fn new(run: RunFile) -> Self {
        Self {
            run,
            buffer: Vec::new(),
            pos: 0,
            offset: 0,
        }
    }

    /// Make at least `want` unread bytes available, or every remaining byte
    /// when the file ends first.
    fn fill(&mut self, want: usize) -> Result<()> {
        let available = self.buffer.len() - self.pos;
        let remaining = self.run.len - self.offset;
        if available >= want || remaining == 0 {
            return Ok(());
        }
        self.buffer.drain(..self.pos);
        self.pos = 0;
        let read = (want - available).max(BLOCK) as u64;
        let read = usize::try_from(read.min(remaining)).expect("block fits in memory");
        let start = self.buffer.len();
        self.buffer.resize(start + read, 0);
        let file = self.run.file.as_deref().expect("a run being read is open");
        file.read_at(&mut self.buffer[start..], self.offset)?;
        self.offset += read as u64;
        Ok(())
    }

    fn next_row(&mut self) -> Result<Option<Vec<Reg>>> {
        // Two varints of at most ten bytes each.
        self.fill(20)?;
        if self.pos == self.buffer.len() {
            return Ok(None);
        }
        let columns = decode_varint(&self.buffer, &mut self.pos)?;
        let len = decode_varint(&self.buffer, &mut self.pos)?;
        let len = usize::try_from(len).map_err(|_| Error::corrupt("sort spill row too long"))?;
        self.fill(len)?;
        let end = self.pos + len;
        let bytes = self
            .buffer
            .get(self.pos..end)
            .ok_or_else(|| Error::corrupt("truncated sort spill row"))?;
        let mut row = Vec::with_capacity(columns as usize);
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            row.push(decode_reg(bytes, &mut cursor)?);
        }
        if row.len() as u64 != columns {
            return Err(Error::corrupt("sort spill row has the wrong width"));
        }
        self.pos = end;
        Ok(Some(row))
    }
}

/// A row waiting in the merge heap with the run it came from.
struct Head {
    row: Vec<Reg>,
    source: usize,
    prefix: usize,
    ascending: Arc<[bool]>,
}

impl Head {
    /// Ascending output order: by keys, then by run so ties keep input order.
    fn order(&self, other: &Self) -> Ordering {
        compare_keys(
            &self.row[self.prefix..],
            &other.row[other.prefix..],
            &self.ascending,
        )
        .then(self.source.cmp(&other.source))
    }
}

impl PartialEq for Head {
    fn eq(&self, other: &Self) -> bool {
        self.order(other).is_eq()
    }
}

impl Eq for Head {}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Head {
    /// Reversed: `BinaryHeap` is a max-heap and the merge wants the least.
    fn cmp(&self, other: &Self) -> Ordering {
        other.order(self)
    }
}

/// A k-way merge over spilled runs plus the final in-memory run, which is
/// the newest and so ranks last on ties.
pub(super) struct Merge {
    readers: Vec<RunReader>,
    memory: std::vec::IntoIter<Vec<Reg>>,
    heap: BinaryHeap<Head>,
    prefix: usize,
    ascending: Arc<[bool]>,
    dedup: bool,
    current: Option<Vec<Reg>>,
}

impl Merge {
    fn new(sorter: Sorter, prefix: usize, ascending: &[bool]) -> Result<Self> {
        let mut merge = Self {
            readers: sorter.runs.into_iter().map(RunReader::new).collect(),
            memory: sorter.rows.into_iter(),
            heap: BinaryHeap::new(),
            prefix,
            ascending: ascending.into(),
            dedup: sorter.dedup,
            current: None,
        };
        for source in 0..=merge.readers.len() {
            merge.refill(source)?;
        }
        Ok(merge)
    }

    /// Push the next row of `source` (the in-memory run is the last index).
    fn refill(&mut self, source: usize) -> Result<()> {
        let row = match self.readers.get_mut(source) {
            Some(reader) => reader.next_row()?,
            None => self.memory.next(),
        };
        if let Some(row) = row {
            self.heap.push(Head {
                row,
                source,
                prefix: self.prefix,
                ascending: Arc::clone(&self.ascending),
            });
        }
        Ok(())
    }

    /// Move to the next output row; `false` once every run is exhausted.
    ///
    /// # Errors
    ///
    /// Propagates spill-file I/O and decoding errors.
    pub fn advance(&mut self) -> Result<bool> {
        while let Some(head) = self.heap.pop() {
            self.refill(head.source)?;
            let keys = self.prefix..self.prefix + self.ascending.len();
            let duplicate = self.dedup
                && self
                    .current
                    .as_ref()
                    .is_some_and(|last| last[keys.clone()] == head.row[keys.clone()]);
            if !duplicate {
                self.current = Some(head.row);
                return Ok(true);
            }
        }
        self.current = None;
        Ok(false)
    }

    /// The row the merge is positioned on.
    pub fn current(&self) -> Option<&[Reg]> {
        self.current.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temporal::civil::Date;
    use crate::vfs::mem::{FaultConfig, MemVfs};
    use proptest::prelude::*;

    fn target(vfs: &Arc<MemVfs>) -> SpillTarget {
        SpillTarget {
            vfs: Arc::clone(vfs) as Arc<dyn Vfs>,
            database: PathBuf::from("sorter.db"),
        }
    }

    fn spill_files(vfs: &MemVfs) -> usize {
        let store = vfs.store().lock().expect("store");
        store
            .keys()
            .filter(|path| path.to_string_lossy().contains("-sort-"))
            .count()
    }

    /// Run a sorter over `rows` and collect its output.
    fn sort_all(
        vfs: &Arc<MemVfs>,
        rows: &[Vec<Reg>],
        budget: usize,
        prefix: usize,
        ascending: &[bool],
        dedup: bool,
    ) -> Result<Vec<Vec<Reg>>> {
        let mut sorter = Sorter::new(target(vfs), budget, dedup);
        for row in rows {
            sorter.push(row, prefix, ascending)?;
        }
        match sorter.finish(prefix, ascending)? {
            Sorted::Rows(rows) => Ok(rows),
            Sorted::Merge(mut merge) => {
                let mut out = Vec::new();
                while merge.advance()? {
                    out.push(merge.current().expect("positioned").to_vec());
                }
                Ok(out)
            }
        }
    }

    fn expected(
        rows: &[Vec<Reg>],
        prefix: usize,
        ascending: &[bool],
        dedup: bool,
    ) -> Vec<Vec<Reg>> {
        let mut rows = rows.to_vec();
        sort_run(&mut rows, prefix, ascending, dedup);
        rows
    }

    fn value_strategy() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (-5i64..5).prop_map(Value::Int),
            (-5i64..5).prop_map(|value| Value::Float(value as f64)),
            "[a-c]{0,3}".prop_map(|text| Value::from(text.as_str())),
            (0u64..4).prop_map(Value::Node),
            (0u64..4).prop_map(Value::Edge),
        ];
        leaf.prop_recursive(2, 8, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..3)
                    .prop_map(|items| Value::List(items.into())),
                prop::collection::vec(("[a-b]", inner), 0..3).prop_map(|entries| {
                    Value::Map(
                        entries
                            .into_iter()
                            .map(|(key, value)| (Arc::from(key.as_str()), value))
                            .collect::<Vec<_>>()
                            .into(),
                    )
                }),
            ]
        })
    }

    fn reg_strategy() -> impl Strategy<Value = Reg> {
        prop_oneof![
            value_strategy().prop_map(Reg::Value),
            (0u64..4).prop_map(Reg::Node),
            (0u64..4).prop_map(Reg::Edge),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn spilled_sort_matches_stable_sort(
            rows in prop::collection::vec(prop::collection::vec(reg_strategy(), 4), 0..120),
            budget in 0usize..2048,
            ascending in prop::collection::vec(any::<bool>(), 2),
            dedup in any::<bool>(),
        ) {
            let vfs = Arc::new(MemVfs::new());
            let actual = sort_all(&vfs, &rows, budget, 1, &ascending, dedup).expect("sort");
            prop_assert_eq!(actual, expected(&rows, 1, &ascending, dedup));
            prop_assert_eq!(spill_files(&vfs), 0);
        }
    }

    #[test]
    fn ties_keep_input_order_across_runs() {
        let vfs = Arc::new(MemVfs::new());
        let rows: Vec<Vec<Reg>> = (0..200)
            .map(|index| {
                vec![
                    Reg::Value(Value::Int(index)),
                    Reg::Value(Value::Int(index % 3)),
                ]
            })
            .collect();
        let before = SPILLED_RUNS.with(std::cell::Cell::get);
        let actual = sort_all(&vfs, &rows, 512, 1, &[true], false).expect("sort");
        assert!(SPILLED_RUNS.with(std::cell::Cell::get) - before > 2);
        assert_eq!(actual, expected(&rows, 1, &[true], false));
    }

    #[test]
    fn codec_round_trips_every_register_kind() {
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Int(-7),
            Value::Float(f64::NAN),
            Value::from("text"),
            Value::Bytes(Arc::from(&b"\x00\xff"[..])),
            Value::Node(3),
            Value::Edge(4),
            Value::Date(Date::new(2026, 11, 7).expect("date")),
            Value::List(vec![Value::Node(1), Value::from("x")].into()),
            Value::Map(vec![(Arc::from("k"), Value::Edge(9))].into()),
        ];
        let regs: Vec<Reg> = values
            .into_iter()
            .map(Reg::Value)
            .chain([Reg::Node(11), Reg::Edge(12)])
            .collect();
        let mut bytes = Vec::new();
        for reg in &regs {
            encode_reg(&mut bytes, reg);
        }
        let mut pos = 0;
        let decoded: Vec<Reg> = regs
            .iter()
            .map(|_| decode_reg(&bytes, &mut pos).expect("decode"))
            .collect();
        assert_eq!(pos, bytes.len());
        assert_eq!(decoded, regs);
    }

    #[test]
    fn empty_and_single_run_sorts_stay_in_memory() {
        let vfs = Arc::new(MemVfs::new());
        let before = SPILLED_RUNS.with(std::cell::Cell::get);
        assert!(
            sort_all(&vfs, &[], 0, 0, &[true], false)
                .expect("sort")
                .is_empty()
        );
        let rows = vec![
            vec![Reg::Value(Value::Int(2))],
            vec![Reg::Value(Value::Int(1))],
        ];
        let sorted = sort_all(&vfs, &rows, usize::MAX, 0, &[true], false).expect("sort");
        assert_eq!(sorted, vec![rows[1].clone(), rows[0].clone()]);
        assert_eq!(SPILLED_RUNS.with(std::cell::Cell::get), before);
    }

    #[test]
    fn oversized_rows_cross_block_boundaries() {
        let vfs = Arc::new(MemVfs::new());
        let rows: Vec<Vec<Reg>> = (0..6)
            .map(|index| {
                let text = "x".repeat(BLOCK + index * 1000);
                vec![
                    Reg::Value(Value::from(text.as_str())),
                    Reg::Value(Value::Int(-(index as i64))),
                ]
            })
            .collect();
        let actual = sort_all(&vfs, &rows, 0, 1, &[true], false).expect("sort");
        assert_eq!(actual, expected(&rows, 1, &[true], false));
    }

    #[test]
    fn io_failures_surface_and_remove_spill_files() {
        let rows: Vec<Vec<Reg>> = (0..100)
            .map(|index| vec![Reg::Value(Value::Int(index))])
            .collect();
        for fail_after_ops in [0, 3, 10] {
            let vfs = Arc::new(MemVfs::new());
            vfs.inject_fault(FaultConfig {
                fail_after_ops: Some(fail_after_ops),
                torn_write: false,
            });
            assert!(sort_all(&vfs, &rows, 64, 0, &[false], false).is_err());
            vfs.clear_fault();
            assert_eq!(spill_files(&vfs), 0);
        }
    }

    #[test]
    fn abandoned_merge_removes_spill_files() {
        let vfs = Arc::new(MemVfs::new());
        let mut sorter = Sorter::new(target(&vfs), 64, false);
        for index in 0..100 {
            sorter
                .push(&[Reg::Value(Value::Int(index))], 0, &[true])
                .expect("push");
        }
        let Sorted::Merge(mut merge) = sorter.finish(0, &[true]).expect("finish") else {
            panic!("expected a spilled sort");
        };
        assert!(merge.advance().expect("advance"));
        assert!(spill_files(&vfs) > 0);
        drop(merge);
        assert_eq!(spill_files(&vfs), 0);
    }
}
