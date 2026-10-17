//! VDBE: the bytecode virtual machine that executes compiled Cypher.
//!
//! A [`Program`] is a flat instruction list with jumps, a constant pool, a
//! register file and cursor slots — the same shape as SQLite's VDBE. Pattern
//! matching compiles to nested cursor loops; `Sort`, `Aggregate`, `Distinct`
//! and `Skip`/`Limit` collect rows into row cursors and post-process them;
//! writes compile to single mutation opcodes.
//!
//! Values in registers are [`Reg`]: plain [`Value`]s plus node/edge
//! references bound by cursors.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::cypher::UnaryOp;
use crate::engine::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::matrix::Direction;
use crate::planner::plan::AggFn;
use crate::record::{Value, value_equals};
use crate::temporal::between;
use crate::temporal::civil::Date;
use crate::temporal::construct::{self, Field, TruncateUnit};
use crate::temporal::datetime::{DateTime, LocalDateTime, LocalTime, Time};
use crate::temporal::duration::Duration;
use crate::temporal::point::Point;

/// A register value: a plain value or an entity reference.
#[derive(Debug, Clone, PartialEq)]
pub enum Reg {
    /// Plain value.
    Value(Value),
    /// Bound node id.
    Node(u64),
    /// Bound edge id.
    Edge(u64),
}

impl Reg {
    /// Convert to a plain value for comparisons and output (entities become
    /// entity values).
    pub fn to_value(&self) -> Value {
        match self {
            Self::Value(value) => value.clone(),
            Self::Node(id) => Value::Node(*id),
            Self::Edge(id) => Value::Edge(*id),
        }
    }
}

impl Ord for Reg {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Value(a), Self::Value(b)) => a.cmp(b),
            (Self::Node(a), Self::Node(b)) => a.cmp(b),
            (Self::Edge(a), Self::Edge(b)) => a.cmp(b),
            _ => self.to_value().cmp(&other.to_value()),
        }
    }
}

impl PartialOrd for Reg {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for Reg {}

/// Aggregation specification carried by the aggregate opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggSpec {
    /// Aggregation function.
    pub func: AggFn,
    /// How many argument values each row feeds this aggregation.
    pub args: u8,
    /// Whether the argument is `*` (only meaningful for `count`).
    pub star: bool,
    /// Whether only distinct argument values are aggregated.
    pub distinct: bool,
}

/// A VDBE instruction.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Open a node scan (all nodes, or a label bitmap).
    OpenNodeScan {
        /// Cursor slot.
        cursor: u32,
        /// Constant index of the label name to filter by.
        label: Option<u32>,
    },
    /// Open an edge scan (all edges, or a type bitmap).
    OpenEdgeScan {
        /// Cursor slot.
        cursor: u32,
        /// Edge type filter.
        edge_type: Option<u32>,
    },
    /// Open an index scan for an exact value.
    OpenIndexScan {
        /// Cursor slot.
        cursor: u32,
        /// Node index id.
        index: u32,
        /// Register holding the lookup value.
        value: u32,
    },
    /// Open an expansion from a bound node.
    OpenExpand {
        /// Cursor slot.
        cursor: u32,
        /// Source node register.
        src: u32,
        /// Direction code (0 out, 1 in, 2 undirected).
        direction: u8,
        /// Constant indices of restricting edge type names.
        types: Vec<u32>,
        /// Whether the pattern had no type filter (empty `types` means any).
        any_type: bool,
    },
    /// Position a cursor on its first row; jump when empty.
    Rewind {
        /// Cursor slot.
        cursor: u32,
        /// Jump target when empty.
        target: u32,
    },
    /// Advance a cursor; jump when exhausted.
    Next {
        /// Cursor slot.
        cursor: u32,
        /// Jump target when exhausted.
        target: u32,
    },
    /// Bind the cursor's current id to a register.
    Bind {
        /// Cursor slot.
        cursor: u32,
        /// Destination register.
        dest: u32,
    },
    /// Bind the current expansion's edge and neighbor.
    BindExpand {
        /// Cursor slot.
        cursor: u32,
        /// Edge register.
        edge: u32,
        /// Neighbor register.
        node: u32,
    },
    /// Read a property of an entity register.
    Column {
        /// Entity register.
        target: u32,
        /// Property key id, or [`NO_KEY`].
        key: u32,
        /// Destination register.
        dest: u32,
    },
    /// Load a constant.
    Constant {
        /// Destination register.
        dest: u32,
        /// Constant pool index.
        value: u32,
    },
    /// Copy a register.
    Copy {
        /// Destination register.
        dest: u32,
        /// Source register.
        src: u32,
    },
    /// Load null.
    Null {
        /// Destination register.
        dest: u32,
    },
    /// Unary operation.
    Unary {
        /// Operator code.
        op: u8,
        /// Source register.
        src: u32,
        /// Destination register.
        dest: u32,
    },
    /// Binary operation.
    Binary {
        /// Operator code.
        op: u8,
        /// Left register.
        left: u32,
        /// Right register.
        right: u32,
        /// Destination register.
        dest: u32,
    },
    /// `expr[index]` subscript.
    Subscript {
        /// Base register.
        base: u32,
        /// Index register.
        index: u32,
        /// Destination register.
        dest: u32,
    },
    /// Build a list from registers.
    BuildList {
        /// Destination register.
        dest: u32,
        /// Element registers.
        items: Vec<u32>,
    },
    /// `expr IS NULL`.
    IsNull {
        /// Source register.
        src: u32,
        /// Destination register.
        dest: u32,
    },
    /// Whether a node carries a label.
    HasLabel {
        /// Node register.
        node: u32,
        /// Constant index of the label name.
        label: u32,
        /// Destination register.
        dest: u32,
    },
    /// Scalar function call.
    Function {
        /// Function code.
        func: u8,
        /// Argument registers.
        args: Vec<u32>,
        /// Destination register.
        dest: u32,
    },
    /// Build a map from `(key, register)` pairs.
    BuildMap {
        /// Destination register.
        dest: u32,
        /// Key names and value registers.
        pairs: Vec<(String, u32)>,
    },
    /// Jump when the register is false or null.
    IfNot {
        /// Tested register.
        reg: u32,
        /// Jump target.
        target: u32,
    },
    /// Unconditional jump.
    Goto {
        /// Jump target.
        target: u32,
    },
    /// Emit a result row from a register window.
    ResultRow {
        /// First register.
        start: u32,
        /// Number of registers.
        count: u32,
    },
    /// Stop execution.
    Halt,
    /// Open a row-collection cursor.
    CollectOpen {
        /// Cursor slot.
        cursor: u32,
    },
    /// Snapshot a register window as a row.
    CollectRow {
        /// Cursor slot.
        cursor: u32,
        /// First register.
        start: u32,
        /// Number of registers.
        count: u32,
    },
    /// Hash-aggregate collected rows.
    HashAggregate {
        /// Cursor slot.
        cursor: u32,
        /// Number of leading row columns to ignore.
        prefix: u32,
        /// Number of grouping columns.
        groups: u32,
        /// Aggregations (one input column each, after the groups).
        aggs: Vec<AggSpec>,
    },
    /// Sort collected rows by their trailing key columns.
    SortRows {
        /// Cursor slot.
        cursor: u32,
        /// Number of leading row columns to keep in place.
        prefix: u32,
        /// Number of key columns at the end of each row.
        keys: u32,
        /// Per-key sort direction.
        ascending: Vec<bool>,
    },
    /// Sort by trailing key columns and drop duplicate key groups.
    DedupRows {
        /// Cursor slot.
        cursor: u32,
        /// Number of leading row columns.
        prefix: u32,
        /// Number of key columns at the end of each row.
        keys: u32,
    },
    /// Apply `SKIP`/`LIMIT` to collected rows.
    TruncateRows {
        /// Cursor slot.
        cursor: u32,
        /// Register holding the skip count (null = 0).
        skip: u32,
        /// Register holding the limit count (null = unlimited).
        limit: u32,
    },
    /// Position a row cursor on its first row; jump when empty.
    CtxRewind {
        /// Cursor slot.
        cursor: u32,
        /// Jump target when empty.
        target: u32,
    },
    /// Advance a row cursor; jump when exhausted.
    CtxNext {
        /// Cursor slot.
        cursor: u32,
        /// Jump target when exhausted.
        target: u32,
    },
    /// Test whether a row cursor holds any rows.
    CtxHasRows {
        /// Cursor slot.
        cursor: u32,
        /// Destination register.
        dest: u32,
    },
    /// Collect the first register of every row into a list.
    CtxToList {
        /// Cursor slot.
        cursor: u32,
        /// Destination register.
        dest: u32,
    },
    /// Load the current row into a register window.
    CtxData {
        /// Cursor slot.
        cursor: u32,
        /// First register.
        start: u32,
        /// Number of registers.
        count: u32,
    },
    /// Read a property by name from a node, relationship or map.
    Property {
        /// Entity or map register.
        target: u32,
        /// Constant index of the property key name.
        name: u32,
        /// Destination register.
        dest: u32,
    },
    /// Slice a list: `base[lower..upper]`.
    Slice {
        /// List register.
        base: u32,
        /// Lower bound register (ignored when `open_lower`).
        lower: u32,
        /// Upper bound register (ignored when `open_upper`).
        upper: u32,
        /// Whether the lower bound was omitted.
        open_lower: bool,
        /// Whether the upper bound was omitted.
        open_upper: bool,
        /// Destination register.
        dest: u32,
    },
    /// Build a path value: an alternating node/relationship list, splicing
    /// variable-length relationship lists into individual edges.
    BuildPath {
        /// Destination register.
        dest: u32,
        /// Elements in path order.
        elements: Vec<BuildPathPart>,
    },
    /// Append a value to a list register (rebuilding the list).
    ListPush {
        /// List register, updated in place.
        list: u32,
        /// Value register.
        value: u32,
    },
    /// Open a variable-length expansion from a node.
    VarExpandOpen {
        /// Cursor slot.
        cursor: u32,
        /// Source node register.
        src: u32,
        /// Direction code: 0 out, 1 in, 2 both.
        direction: u8,
        /// Constant indices of restricting edge type names.
        types: Vec<u32>,
        /// Whether the pattern had no type filter.
        any_type: bool,
        /// Literal property filters applied at every hop, by key name.
        props: Vec<(String, Value)>,
        /// Minimum hops.
        min: u32,
        /// Maximum hops; `None` is unbounded.
        max: Option<u32>,
    },
    /// Open a built-in procedure, filling a row cursor with its output.
    ProcedureCallOpen {
        /// Cursor slot.
        cursor: u32,
        /// Constant index of the procedure name.
        name: u32,
    },
    /// Open a shortest-path search from a node to a target node.
    ShortestOpen {
        /// Cursor slot.
        cursor: u32,
        /// Source node register.
        src: u32,
        /// Target node register; `None` targets every reachable node.
        dst: Option<u32>,
        /// Direction code: 0 out, 1 in, 2 both.
        direction: u8,
        /// Constant indices of restricting edge type names.
        types: Vec<u32>,
        /// Whether the pattern had no type filter.
        any_type: bool,
        /// Literal property filters applied at every hop, by key name.
        props: Vec<(String, Value)>,
        /// Minimum hops.
        min: u32,
        /// Maximum hops; `None` is unbounded.
        max: Option<u32>,
        /// Whether every shortest path is produced.
        all: bool,
    },
    /// Advance a variable-length expansion; jump when exhausted.
    VarExpandNext {
        /// Cursor slot.
        cursor: u32,
        /// Destination register for the edge list.
        edges: u32,
        /// Destination register for the node list.
        nodes: u32,
        /// Destination register for the end node.
        node: u32,
        /// Whether the trail runs against the pattern direction, so the
        /// bound lists read from the pattern's start.
        reverse: bool,
        /// Jump target when exhausted.
        target: u32,
    },
    /// Start unwinding a list.
    UnwindInit {
        /// Cursor slot.
        cursor: u32,
        /// Register holding the list.
        list: u32,
    },
    /// Advance the unwind cursor, binding the element; jump when exhausted.
    UnwindNext {
        /// Cursor slot.
        cursor: u32,
        /// Destination register.
        dest: u32,
        /// Jump target when exhausted.
        target: u32,
    },
    /// Create a node from label and property registers.
    CreateNode {
        /// Register holding a list of label names.
        labels: u32,
        /// Register holding a property map.
        props: u32,
        /// Destination register.
        dest: u32,
    },
    /// Create an edge from a type register and bound endpoints.
    CreateEdge {
        /// Register holding the edge type name.
        edge_type: u32,
        /// Source node register.
        src: u32,
        /// Destination node register.
        dst: u32,
        /// Register holding a property map.
        props: u32,
        /// Destination register.
        dest: u32,
    },
    /// Reject null property values, which MERGE does not allow.
    CheckMergeProps {
        /// Register holding a property map.
        props: u32,
    },
    /// Set a property on a node or edge.
    SetProperty {
        /// Entity register.
        target: u32,
        /// Register holding the property key name.
        key: u32,
        /// Register holding the value.
        value: u32,
    },
    /// Add or remove labels on a node.
    SetLabels {
        /// Node register.
        target: u32,
        /// Register holding a list of label names.
        labels: u32,
        /// Whether to add (true) or remove (false).
        add: bool,
    },
    /// Replace all properties of a node.
    ReplaceProperties {
        /// Node register.
        target: u32,
        /// Register holding a property map.
        props: u32,
    },
    /// Merge a property map into a node or relationship.
    MergeProperties {
        /// Node or edge register.
        target: u32,
        /// Register holding a property map.
        props: u32,
    },
    /// Delete a node or edge register.
    DeleteAny {
        /// Entity register.
        target: u32,
        /// Whether to delete incident edges first.
        detach: bool,
    },
}

/// Sentinel property key id meaning "no such property".
pub const NO_KEY: u32 = u32::MAX;

/// One element of an [`Op::BuildPath`] sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum BuildPathPart {
    /// A single value from a register.
    Value(u32),
    /// A variable-length segment: its relationship and node registers are
    /// interleaved into the path (nodes first, then relationships).
    Segment {
        /// Register holding the relationship list.
        edges: u32,
        /// Register holding the node list.
        nodes: u32,
    },
}

/// Function codes for [`Op::Function`].
pub mod func {
    /// `id(x)`
    pub const ID: u8 = 0;
    /// `labels(n)`
    pub const LABELS: u8 = 1;
    /// `type(r)`
    pub const TYPE: u8 = 2;
    /// `startNode(r)`
    pub const START_NODE: u8 = 3;
    /// `endNode(r)`
    pub const END_NODE: u8 = 4;
    /// `coalesce(...)`
    pub const COALESCE: u8 = 5;
    /// `toString(x)`
    pub const TO_STRING: u8 = 6;
    /// `toInteger(x)`
    pub const TO_INTEGER: u8 = 7;
    /// `toFloat(x)`
    pub const TO_FLOAT: u8 = 8;
    /// `size(x)`
    pub const SIZE: u8 = 9;
    /// `properties(x)`
    pub const PROPERTIES: u8 = 10;
    /// `keys(x)`
    pub const KEYS: u8 = 11;
    /// `nodes(path)`
    pub const NODES: u8 = 12;
    /// `relationships(path)`
    pub const RELATIONSHIPS: u8 = 13;
    /// `length(path)`
    pub const LENGTH: u8 = 14;
    /// `toBoolean(x)`
    pub const TO_BOOLEAN: u8 = 15;
    /// `range(start, end [, step])`
    pub const RANGE: u8 = 16;
    /// `head(list)`
    pub const HEAD: u8 = 17;
    /// `last(list)`
    pub const LAST: u8 = 18;
    /// `tail(list)`
    pub const TAIL: u8 = 19;
    /// `reverse(list|string)`
    pub const REVERSE: u8 = 20;
    /// `split(original, delimiter)`
    pub const SPLIT: u8 = 21;
    /// `substring(original, start [, length])`
    pub const SUBSTRING: u8 = 22;
    /// `rand()`
    pub const RAND: u8 = 23;
    /// `toUpper(string)`
    pub const TO_UPPER: u8 = 24;
    /// `toLower(string)`
    pub const TO_LOWER: u8 = 25;
    /// `trim(string)`
    pub const TRIM: u8 = 26;
    /// `ltrim(string)`
    pub const LTRIM: u8 = 27;
    /// `rtrim(string)`
    pub const RTRIM: u8 = 28;
    /// `left(string, length)`
    pub const LEFT: u8 = 29;
    /// `right(string, length)`
    pub const RIGHT: u8 = 30;
    /// `replace(original, search, replacement)`
    pub const REPLACE: u8 = 31;
    /// `abs(x)`
    pub const ABS: u8 = 32;
    /// `ceil(x)`
    pub const CEIL: u8 = 33;
    /// `floor(x)`
    pub const FLOOR: u8 = 34;
    /// `round(x)`
    pub const ROUND: u8 = 35;
    /// `sign(x)`
    pub const SIGN: u8 = 36;
    /// `sqrt(x)`
    pub const SQRT: u8 = 37;
    /// `exp(x)`
    pub const EXP: u8 = 38;
    /// `log(x)`
    pub const LOG: u8 = 39;
    /// `log10(x)`
    pub const LOG10: u8 = 40;
    /// `sin(x)`
    pub const SIN: u8 = 41;
    /// `cos(x)`
    pub const COS: u8 = 42;
    /// `tan(x)`
    pub const TAN: u8 = 43;
    /// `cot(x)`
    pub const COT: u8 = 44;
    /// `asin(x)`
    pub const ASIN: u8 = 45;
    /// `acos(x)`
    pub const ACOS: u8 = 46;
    /// `atan(x)`
    pub const ATAN: u8 = 47;
    /// `atan2(y, x)`
    pub const ATAN2: u8 = 48;
    /// `degrees(x)`
    pub const DEGREES: u8 = 49;
    /// `radians(x)`
    pub const RADIANS: u8 = 50;
    /// `haversin(x)`
    pub const HAVERSIN: u8 = 51;
    /// `pi()`
    pub const PI: u8 = 52;
    /// `e()`
    pub const E: u8 = 53;
    /// `date(...)`
    pub const DATE: u8 = 54;
    /// `localtime(...)`
    pub const LOCALTIME: u8 = 55;
    /// `time(...)`
    pub const TIME: u8 = 56;
    /// `localdatetime(...)`
    pub const LOCAL_DATETIME: u8 = 57;
    /// `datetime(...)`
    pub const DATETIME: u8 = 58;
    /// `duration(...)`
    pub const DURATION: u8 = 59;
    /// `point({...})`
    pub const POINT: u8 = 60;
    /// `distance(a, b)`
    pub const DISTANCE: u8 = 61;
    /// `date.truncate(unit, value [, map])`
    pub const DATE_TRUNCATE: u8 = 62;
    /// `localtime.truncate(unit, value [, map])`
    pub const LOCALTIME_TRUNCATE: u8 = 63;
    /// `time.truncate(unit, value [, map])`
    pub const TIME_TRUNCATE: u8 = 64;
    /// `localdatetime.truncate(unit, value [, map])`
    pub const LOCAL_DATETIME_TRUNCATE: u8 = 65;
    /// `datetime.truncate(unit, value [, map])`
    pub const DATETIME_TRUNCATE: u8 = 66;
    /// `datetime.fromepoch(seconds, nanos)`
    pub const DATETIME_FROM_EPOCH: u8 = 67;
    /// `datetime.fromepochmillis(millis)`
    pub const DATETIME_FROM_EPOCH_MILLIS: u8 = 68;
    /// `duration.between(from, to)`
    pub const DURATION_BETWEEN: u8 = 69;
    /// `duration.inMonths(from, to)`
    pub const DURATION_IN_MONTHS: u8 = 70;
    /// `duration.inDays(from, to)`
    pub const DURATION_IN_DAYS: u8 = 71;
    /// `duration.inSeconds(from, to)`
    pub const DURATION_IN_SECONDS: u8 = 72;
}

/// A compiled VDBE program.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Instructions.
    pub ops: Vec<Op>,
    /// Constant pool.
    pub constants: Vec<Value>,
    /// Number of cursor slots.
    pub cursor_count: usize,
    /// Number of registers.
    pub register_count: usize,
    /// Output column names.
    pub columns: Vec<String>,
    /// Whether the program mutates the graph.
    pub mutating: bool,
}

/// Execute a program and return its result rows.
pub fn execute(db: &mut Database, program: &Program) -> Result<Vec<Vec<Value>>> {
    let mut executor = Executor {
        db,
        program,
        regs: vec![Reg::Value(Value::Null); program.register_count],
        cursors: (0..program.cursor_count).map(|_| Cursor::Empty).collect(),
        results: Vec::new(),
        key_ids: vec![None; program.constants.len()],
        label_ids: vec![None; program.constants.len()],
        statement_time: system_local_datetime(),
        pc: 0,
        steps: 0,
    };
    executor.run()?;
    Ok(executor.results)
}

/// Maximum instructions executed per query, guarding against runaway loops.
const MAX_STEPS: u64 = 10_000_000_000;

enum Cursor {
    Empty,
    Ids {
        ids: Vec<u64>,
        pos: usize,
        is_edge: bool,
    },
    Expand {
        items: Vec<(u64, u64)>,
        pos: usize,
    },
    Trails {
        items: Vec<crate::engine::Trail>,
        pos: usize,
    },
    Rows {
        rows: Vec<Vec<Reg>>,
        pos: usize,
    },
    Unwind {
        items: Vec<Value>,
        pos: usize,
    },
}

struct Executor<'a> {
    db: &'a mut Database,
    program: &'a Program,
    regs: Vec<Reg>,
    cursors: Vec<Cursor>,
    results: Vec<Vec<Value>>,
    key_ids: Vec<Option<u32>>,
    label_ids: Vec<Option<u32>>,
    /// The statement's start time, shared by the clock functions so two
    /// `datetime()` calls in one query observe the same instant.
    statement_time: LocalDateTime,
    pc: usize,
    steps: u64,
}

impl<'a> Executor<'a> {
    fn run(&mut self) -> Result<()> {
        while self.pc < self.program.ops.len() {
            self.steps += 1;
            if self.steps > MAX_STEPS {
                return Err(Error::new(
                    ErrorCode::Range,
                    "query exceeded the execution step limit",
                ));
            }
            let op = self.program.ops[self.pc].clone();
            self.pc += 1;
            self.step(&op)?;
        }
        Ok(())
    }

    fn step(&mut self, op: &Op) -> Result<()> {
        match op {
            Op::OpenNodeScan { cursor, label } => {
                let ids = match label {
                    Some(label) => match self.resolve_label(*label)? {
                        Some(label) => self.db.label_nodes(label)?,
                        None => Vec::new(),
                    },
                    None => self.db.all_node_ids()?,
                };
                self.cursors[*cursor as usize] = Cursor::Ids {
                    ids,
                    pos: 0,
                    is_edge: false,
                };
            }
            Op::OpenEdgeScan { cursor, edge_type } => {
                let ids = match edge_type {
                    Some(edge_type) => self.db.edges_of_type(*edge_type)?,
                    None => self.db.all_edge_ids()?,
                };
                self.cursors[*cursor as usize] = Cursor::Ids {
                    ids,
                    pos: 0,
                    is_edge: true,
                };
            }
            Op::OpenIndexScan {
                cursor,
                index,
                value,
            } => {
                let value = self.reg_value(*value)?;
                let ids = self.db.index_lookup(*index, &value)?;
                self.cursors[*cursor as usize] = Cursor::Ids {
                    ids,
                    pos: 0,
                    is_edge: false,
                };
            }
            Op::OpenExpand {
                cursor,
                src,
                direction,
                types,
                any_type,
            } => {
                let types = self.resolve_types(types, *any_type)?;
                // Expanding from a null binding (an optional match that
                // missed) matches nothing instead of failing.
                let items = match (&types, &self.regs[*src as usize]) {
                    (Some(types), _) if types.is_empty() => Vec::new(),
                    (_, Reg::Value(Value::Null)) => Vec::new(),
                    (None, _) => {
                        let node = self.reg_node(*src)?;
                        match direction {
                            0 => self.db.expand(node, Direction::Out, &[])?,
                            1 => self.db.expand(node, Direction::In, &[])?,
                            _ => {
                                let mut both = self.db.expand(node, Direction::Out, &[])?;
                                both.extend(self.db.expand(node, Direction::In, &[])?);
                                both.sort_unstable();
                                both.dedup();
                                both
                            }
                        }
                    }
                    (Some(types), _) => {
                        let node = self.reg_node(*src)?;
                        match direction {
                            0 => self.db.expand(node, Direction::Out, types)?,
                            1 => self.db.expand(node, Direction::In, types)?,
                            _ => {
                                let mut both = self.db.expand(node, Direction::Out, types)?;
                                both.extend(self.db.expand(node, Direction::In, types)?);
                                both.sort_unstable();
                                both.dedup();
                                both
                            }
                        }
                    }
                };
                self.cursors[*cursor as usize] = Cursor::Expand { items, pos: 0 };
            }
            Op::Rewind { cursor, target } => {
                let empty = match &mut self.cursors[*cursor as usize] {
                    Cursor::Ids { ids, pos, .. } => {
                        *pos = 0;
                        ids.is_empty()
                    }
                    Cursor::Expand { items, pos } => {
                        *pos = 0;
                        items.is_empty()
                    }
                    _ => return Err(Error::internal("Rewind on a non-loop cursor")),
                };
                if empty {
                    self.pc = *target as usize;
                }
            }
            Op::Next { cursor, target } => {
                let exhausted = match &mut self.cursors[*cursor as usize] {
                    Cursor::Ids { ids, pos, .. } => {
                        *pos += 1;
                        *pos >= ids.len()
                    }
                    Cursor::Expand { items, pos } => {
                        *pos += 1;
                        *pos >= items.len()
                    }
                    _ => return Err(Error::internal("Next on a non-loop cursor")),
                };
                if exhausted {
                    self.pc = *target as usize;
                }
            }
            Op::Bind { cursor, dest } => {
                let reg = match &self.cursors[*cursor as usize] {
                    Cursor::Ids { ids, pos, is_edge } => {
                        let id = *ids
                            .get(*pos)
                            .ok_or_else(|| Error::internal("Bind on exhausted cursor"))?;
                        if *is_edge {
                            Reg::Edge(id)
                        } else {
                            Reg::Node(id)
                        }
                    }
                    _ => return Err(Error::internal("Bind on a non-id cursor")),
                };
                self.regs[*dest as usize] = reg;
            }
            Op::BindExpand { cursor, edge, node } => {
                let (edge_id, node_id) = match &self.cursors[*cursor as usize] {
                    Cursor::Expand { items, pos } => *items
                        .get(*pos)
                        .ok_or_else(|| Error::internal("BindExpand on exhausted cursor"))?,
                    _ => return Err(Error::internal("BindExpand on a non-expand cursor")),
                };
                self.regs[*edge as usize] = Reg::Edge(edge_id);
                self.regs[*node as usize] = Reg::Node(node_id);
            }
            Op::Column { target, key, dest } => {
                let value = if *key == NO_KEY {
                    Value::Null
                } else {
                    self.column_value(*target, *key)?
                };
                self.regs[*dest as usize] = Reg::Value(value);
            }
            Op::Constant { dest, value } => {
                let constant = self
                    .program
                    .constants
                    .get(*value as usize)
                    .ok_or_else(|| Error::internal("constant index out of range"))?;
                self.regs[*dest as usize] = Reg::Value(constant.clone());
            }
            Op::Copy { dest, src } => {
                self.regs[*dest as usize] = self.regs[*src as usize].clone();
            }
            Op::Null { dest } => {
                self.regs[*dest as usize] = Reg::Value(Value::Null);
            }
            Op::Unary { op, src, dest } => {
                let value = self.reg_value(*src)?;
                let result = match op {
                    0 => match value {
                        Value::Int(value) => Value::Int(value.wrapping_neg()),
                        Value::Float(value) => Value::Float(-value),
                        Value::Duration(value) => Value::Duration(-value),
                        Value::Null => Value::Null,
                        other => {
                            return Err(Error::type_error(format!(
                                "cannot negate {}",
                                other.type_name()
                            )));
                        }
                    },
                    _ => match value {
                        Value::Bool(value) => Value::Bool(!value),
                        Value::Null => Value::Null,
                        other => {
                            return Err(Error::type_error(format!(
                                "cannot apply NOT to {}",
                                other.type_name()
                            )));
                        }
                    },
                };
                self.regs[*dest as usize] = Reg::Value(result);
            }
            Op::Binary {
                op,
                left,
                right,
                dest,
            } => {
                let a = self.reg_value(*left)?;
                let b = self.reg_value(*right)?;
                let result = binary_op(*op, &a, &b)?;
                self.regs[*dest as usize] = Reg::Value(result);
            }
            Op::Subscript { base, index, dest } => {
                let base_value = self.reg_value(*base)?;
                let index_value = self.reg_value(*index)?;
                let result = match (base_value, index_value) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    (Value::List(items), Value::Int(index)) => {
                        let len = items.len() as i64;
                        let resolved = if index < 0 { len + index } else { index };
                        if resolved < 0 || resolved >= len {
                            Value::Null
                        } else {
                            items[resolved as usize].clone()
                        }
                    }
                    (Value::Map(entries), Value::Text(key)) => entries
                        .iter()
                        .find(|(entry, _)| entry.as_ref() == key.as_ref())
                        .map(|(_, value)| value.clone())
                        .unwrap_or(Value::Null),
                    // Dynamic property access: `n[key]` / `r[key]`.
                    (Value::Node(id), Value::Text(key)) => {
                        let key_id = self.db.property_key_id(&key);
                        key_id
                            .and_then(|key_id| {
                                self.db
                                    .get_node(id)
                                    .ok()
                                    .flatten()
                                    .and_then(|record| record.get(key_id).cloned())
                            })
                            .unwrap_or(Value::Null)
                    }
                    (Value::Edge(id), Value::Text(key)) => {
                        let key_id = self.db.property_key_id(&key);
                        key_id
                            .and_then(|key_id| {
                                self.db
                                    .get_edge(id)
                                    .ok()
                                    .flatten()
                                    .and_then(|record| record.get(key_id).cloned())
                            })
                            .unwrap_or(Value::Null)
                    }
                    (Value::List(_), other) => {
                        return Err(Error::type_error(format!(
                            "list index must be an integer, got {}",
                            other.type_name()
                        )));
                    }
                    (Value::Map(_), other) => {
                        return Err(Error::type_error(format!(
                            "map key must be a string, got {}",
                            other.type_name()
                        )));
                    }
                    (other, _) => {
                        return Err(Error::type_error(format!(
                            "cannot index into {}",
                            other.type_name()
                        )));
                    }
                };
                self.regs[*dest as usize] = normalize_reg(result);
            }
            Op::Slice {
                base,
                lower,
                upper,
                open_lower,
                open_upper,
                dest,
            } => {
                let base_value = self.reg_value(*base)?;
                let result = match base_value {
                    Value::Null => Value::Null,
                    Value::List(items) => {
                        let lower_value = self.reg_value(*lower)?;
                        let upper_value = self.reg_value(*upper)?;
                        // An explicitly null bound makes the slice null.
                        if (!open_lower && lower_value.is_null())
                            || (!open_upper && upper_value.is_null())
                        {
                            Value::Null
                        } else {
                            let len = items.len() as i64;
                            let start = if *open_lower {
                                0
                            } else {
                                slice_bound("lower", &lower_value, len)?
                            };
                            let end = if *open_upper {
                                len as usize
                            } else {
                                slice_bound("upper", &upper_value, len)?
                            };
                            let end = end.max(start);
                            Value::List(items[start..end].to_vec().into())
                        }
                    }
                    other => {
                        return Err(Error::type_error(format!(
                            "cannot slice {}",
                            other.type_name()
                        )));
                    }
                };
                self.regs[*dest as usize] = normalize_reg(result);
            }
            Op::BuildList { dest, items } => {
                let values: Vec<Value> = items
                    .iter()
                    .map(|reg| self.reg_value(*reg))
                    .collect::<Result<_>>()?;
                self.regs[*dest as usize] = Reg::Value(Value::List(values.into()));
            }
            Op::Property { target, name, dest } => {
                // Keys may be created by the very statement that reads them,
                // so resolve lazily and only cache successful lookups.
                let key = match self.key_ids.get(*name as usize).copied() {
                    Some(Some(key)) => Some(key),
                    Some(None) => {
                        let Value::Text(key_name) = &self.program.constants[*name as usize] else {
                            return Err(Error::internal("property name is not a string"));
                        };
                        let resolved = self.db.property_key_id(key_name);
                        if resolved.is_some() {
                            self.key_ids[*name as usize] = resolved;
                        }
                        resolved
                    }
                    None => return Err(Error::internal("constant index out of range")),
                };
                let value = match self.regs[*target as usize].clone() {
                    Reg::Value(Value::Null) => Value::Null,
                    Reg::Node(id) | Reg::Value(Value::Node(id)) => match key {
                        Some(key) => self
                            .db
                            .get_node(id)?
                            .and_then(|record| record.get(key).cloned())
                            .unwrap_or(Value::Null),
                        None => Value::Null,
                    },
                    Reg::Edge(id) | Reg::Value(Value::Edge(id)) => match key {
                        Some(key) => self
                            .db
                            .get_edge(id)?
                            .and_then(|record| record.get(key).cloned())
                            .unwrap_or(Value::Null),
                        None => Value::Null,
                    },
                    Reg::Value(Value::Map(entries)) => {
                        let Value::Text(name) = &self.program.constants[*name as usize] else {
                            return Err(Error::internal("property name is not a string"));
                        };
                        entries
                            .iter()
                            .find(|(entry, _)| entry.as_ref() == name.as_ref())
                            .map(|(_, value)| value.clone())
                            .unwrap_or(Value::Null)
                    }
                    Reg::Value(
                        value @ (Value::Date(_)
                        | Value::LocalTime(_)
                        | Value::Time(_)
                        | Value::LocalDateTime(_)
                        | Value::DateTime(_)
                        | Value::Duration(_)
                        | Value::Point(_)),
                    ) => {
                        // Temporal and spatial values expose components as
                        // property-like accessors (`d.year`, `p.x`).
                        let Value::Text(name) = &self.program.constants[*name as usize] else {
                            return Err(Error::internal("property name is not a string"));
                        };
                        temporal_accessor(&value, name)?
                    }
                    other => {
                        return Err(Error::type_error(format!(
                            "property access on {}",
                            other.to_value().type_name()
                        )));
                    }
                };
                self.regs[*dest as usize] = Reg::Value(value);
            }
            Op::IsNull { src, dest } => {
                let value = self.reg_value(*src)?;
                self.regs[*dest as usize] = Reg::Value(Value::Bool(value.is_null()));
            }
            Op::HasLabel { node, label, dest } => {
                let label_id = self.resolve_label(*label)?;
                let result = match (self.regs[*node as usize].clone(), label_id) {
                    (Reg::Node(id) | Reg::Value(Value::Node(id)), Some(label)) => self
                        .db
                        .get_node(id)?
                        .map(|record| record.labels.contains(&label))
                        .unwrap_or(false),
                    _ => false,
                };
                self.regs[*dest as usize] = Reg::Value(Value::Bool(result));
            }
            Op::Function { func, args, dest } => {
                let values: Vec<Value> = args
                    .iter()
                    .map(|arg| self.reg_value(*arg))
                    .collect::<Result<_>>()?;
                let result = self.call_function(*func, &values)?;
                self.regs[*dest as usize] = normalize_reg(result);
            }
            Op::BuildMap { dest, pairs } => {
                let mut entries = Vec::with_capacity(pairs.len());
                for (key, reg) in pairs {
                    entries.push((
                        std::sync::Arc::<str>::from(key.as_str()),
                        self.reg_value(*reg)?,
                    ));
                }
                self.regs[*dest as usize] = Reg::Value(Value::Map(entries.into()));
            }
            Op::IfNot { reg, target } => {
                let value = self.reg_value(*reg)?;
                match value {
                    Value::Bool(true) => {}
                    Value::Bool(false) | Value::Null => self.pc = *target as usize,
                    other => {
                        return Err(Error::type_error(format!(
                            "predicate must be a boolean, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            Op::Goto { target } => {
                self.pc = *target as usize;
            }
            Op::ResultRow { start, count } => {
                let mut row = Vec::with_capacity(*count as usize);
                for idx in 0..*count {
                    row.push(self.regs[(*start + idx) as usize].to_value());
                }
                self.results.push(row);
            }
            Op::Halt => {
                self.pc = self.program.ops.len();
            }
            Op::CollectOpen { cursor } => {
                self.cursors[*cursor as usize] = Cursor::Rows {
                    rows: Vec::new(),
                    pos: 0,
                };
            }
            Op::CollectRow {
                cursor,
                start,
                count,
            } => {
                let row: Vec<Reg> = (0..*count)
                    .map(|idx| self.regs[(*start + idx) as usize].clone())
                    .collect();
                match &mut self.cursors[*cursor as usize] {
                    Cursor::Rows { rows, .. } => rows.push(row),
                    _ => return Err(Error::internal("CollectRow on a non-row cursor")),
                }
            }
            Op::HashAggregate {
                cursor,
                prefix,
                groups,
                aggs,
            } => {
                self.hash_aggregate(*cursor, *prefix, *groups, aggs)?;
            }
            Op::SortRows {
                cursor,
                prefix,
                keys,
                ascending,
            } => {
                let (rows, _) = self.rows_mut(*cursor)?;
                let prefix = *prefix as usize;
                let keys = *keys as usize;
                let ascending = ascending.clone();
                rows.sort_by(|a, b| {
                    for key in 0..keys {
                        let left = &a[prefix + key];
                        let right = &b[prefix + key];
                        let ordering = left.cmp(right);
                        if ordering != std::cmp::Ordering::Equal {
                            return if ascending[key] {
                                ordering
                            } else {
                                ordering.reverse()
                            };
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }
            Op::DedupRows {
                cursor,
                prefix,
                keys,
            } => {
                let (rows, _) = self.rows_mut(*cursor)?;
                let prefix = *prefix as usize;
                let keys = *keys as usize;
                rows.sort_by(|a, b| {
                    for key in 0..keys {
                        let ordering = a[prefix + key].cmp(&b[prefix + key]);
                        if ordering != std::cmp::Ordering::Equal {
                            return ordering;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
                rows.dedup_by(|a, b| (0..keys).all(|key| a[prefix + key] == b[prefix + key]));
            }
            Op::TruncateRows {
                cursor,
                skip,
                limit,
            } => {
                let skip = self.reg_as_count(*skip)?;
                let limit = match self.reg_value(*limit)? {
                    Value::Null => None,
                    Value::Int(value) if value >= 0 => Some(value as usize),
                    other => {
                        return Err(Error::type_error(format!(
                            "LIMIT expects a non-negative integer, got {}",
                            other.type_name()
                        )));
                    }
                };
                let (rows, _) = self.rows_mut(*cursor)?;
                if skip >= rows.len() {
                    rows.clear();
                } else if skip > 0 {
                    rows.drain(..skip);
                }
                if let Some(limit) = limit {
                    rows.truncate(limit);
                }
            }
            Op::CtxHasRows { cursor, dest } => {
                let has_rows = match &self.cursors[*cursor as usize] {
                    Cursor::Rows { rows, .. } => !rows.is_empty(),
                    _ => return Err(Error::internal("CtxHasRows on a non-row cursor")),
                };
                self.regs[*dest as usize] = Reg::Value(Value::Bool(has_rows));
            }
            Op::CtxToList { cursor, dest } => {
                let values = match &self.cursors[*cursor as usize] {
                    Cursor::Rows { rows, .. } => rows
                        .iter()
                        .map(|row| {
                            row.first()
                                .map(Reg::to_value)
                                .ok_or_else(|| Error::internal("CtxToList on an empty row"))
                        })
                        .collect::<Result<Vec<Value>>>()?,
                    _ => return Err(Error::internal("CtxToList on a non-row cursor")),
                };
                self.regs[*dest as usize] = Reg::Value(Value::List(values.into()));
            }
            Op::CtxRewind { cursor, target } => {
                let empty = match &mut self.cursors[*cursor as usize] {
                    Cursor::Rows { rows, pos } => {
                        *pos = 0;
                        rows.is_empty()
                    }
                    _ => return Err(Error::internal("CtxRewind on a non-row cursor")),
                };
                if empty {
                    self.pc = *target as usize;
                }
            }
            Op::CtxNext { cursor, target } => {
                let exhausted = match &mut self.cursors[*cursor as usize] {
                    Cursor::Rows { rows, pos } => {
                        *pos += 1;
                        *pos >= rows.len()
                    }
                    _ => return Err(Error::internal("CtxNext on a non-row cursor")),
                };
                if exhausted {
                    self.pc = *target as usize;
                }
            }
            Op::CtxData {
                cursor,
                start,
                count,
            } => {
                let row = match &self.cursors[*cursor as usize] {
                    Cursor::Rows { rows, pos } => rows
                        .get(*pos)
                        .ok_or_else(|| Error::internal("CtxData on exhausted cursor"))?
                        .clone(),
                    _ => return Err(Error::internal("CtxData on a non-row cursor")),
                };
                for idx in 0..*count as usize {
                    let value = row
                        .get(idx)
                        .ok_or_else(|| Error::internal("row shorter than window"))?
                        .clone();
                    self.regs[(*start as usize) + idx] = value;
                }
            }
            Op::VarExpandOpen {
                cursor,
                src,
                direction,
                types,
                any_type,
                props,
                min,
                max,
            } => {
                let types = self.resolve_types(types, *any_type)?;
                // Expanding from a null binding matches nothing.
                let items = match (&types, &self.regs[*src as usize]) {
                    (Some(types), _) if types.is_empty() => Vec::new(),
                    (_, Reg::Value(Value::Null)) => Vec::new(),
                    _ => {
                        let node = self.reg_node(*src)?;
                        let directions: Vec<Direction> = match direction {
                            0 => vec![Direction::Out],
                            1 => vec![Direction::In],
                            _ => vec![Direction::Out, Direction::In],
                        };
                        let filter: &[u32] = types.as_deref().unwrap_or(&[]);
                        self.db
                            .var_expand(node, &directions, filter, props, *min, *max)?
                    }
                };
                self.cursors[*cursor as usize] = Cursor::Trails { items, pos: 0 };
            }
            Op::ProcedureCallOpen { cursor, name } => {
                let name = match &self.program.constants[*name as usize] {
                    Value::Text(name) => name.clone(),
                    _ => return Err(Error::internal("procedure name is not text")),
                };
                let rows = self.procedure_rows(&name)?;
                self.cursors[*cursor as usize] = Cursor::Rows { rows, pos: 0 };
            }
            Op::ShortestOpen {
                cursor,
                src,
                dst,
                direction,
                types,
                any_type,
                props,
                min,
                max,
                all,
            } => {
                let types = self.resolve_types(types, *any_type)?;
                // A null endpoint matches nothing.
                let target = match dst {
                    Some(reg) => match self.reg_value(*reg)? {
                        Value::Null => Some(None),
                        _ => Some(Some(self.reg_node(*reg)?)),
                    },
                    None => Some(None),
                };
                let items = match (&types, &self.regs[*src as usize], target) {
                    (Some(types), _, _) if types.is_empty() => Vec::new(),
                    (_, Reg::Value(Value::Null), _) => Vec::new(),
                    (_, _, None) => Vec::new(),
                    (_, _, Some(target)) => {
                        let source = self.reg_node(*src)?;
                        let directions: Vec<Direction> = match direction {
                            0 => vec![Direction::Out],
                            1 => vec![Direction::In],
                            _ => vec![Direction::Out, Direction::In],
                        };
                        let filter: &[u32] = types.as_deref().unwrap_or(&[]);
                        self.db.var_shortest_paths(
                            source,
                            target,
                            &directions,
                            filter,
                            props,
                            *min,
                            *max,
                            *all,
                        )?
                    }
                };
                self.cursors[*cursor as usize] = Cursor::Trails { items, pos: 0 };
            }
            Op::VarExpandNext {
                cursor,
                edges,
                nodes,
                node,
                reverse,
                target,
            } => {
                let next = match &mut self.cursors[*cursor as usize] {
                    Cursor::Trails { items, pos } => {
                        let next = items.get(*pos).cloned();
                        if next.is_some() {
                            *pos += 1;
                        }
                        next
                    }
                    _ => return Err(Error::internal("VarExpandNext on a non-trail cursor")),
                };
                match next {
                    Some(trail) => {
                        // The end node stays the traversal's last node even
                        // when the lists are read from the pattern's start.
                        let end = trail.end();
                        let mut values: Vec<Value> =
                            trail.edges.iter().map(|id| Value::Edge(*id)).collect();
                        let mut node_values: Vec<Value> =
                            trail.nodes.iter().map(|id| Value::Node(*id)).collect();
                        if *reverse {
                            values.reverse();
                            node_values.reverse();
                        }
                        self.regs[*edges as usize] = Reg::Value(Value::List(values.into()));
                        self.regs[*nodes as usize] = Reg::Value(Value::List(node_values.into()));
                        self.regs[*node as usize] = Reg::Node(end);
                    }
                    None => self.pc = *target as usize,
                }
            }
            Op::BuildPath { dest, elements } => {
                let mut items: Vec<Value> = Vec::new();
                for element in elements {
                    match element {
                        BuildPathPart::Value(reg) => items.push(self.reg_value(*reg)?),
                        BuildPathPart::Segment { edges, nodes } => {
                            // Interleave the segment's nodes and
                            // relationships; a zero-length segment contains
                            // only its start node, which is already on the
                            // path.
                            let edges = self.reg_value(*edges)?;
                            let nodes = self.reg_value(*nodes)?;
                            let (Value::List(edges), Value::List(nodes)) = (edges, nodes) else {
                                return Err(Error::internal(
                                    "variable-length path parts must be lists",
                                ));
                            };
                            for (index, edge) in edges.iter().enumerate() {
                                if let Some(node) = nodes.get(index)
                                    && items.last() != Some(node)
                                {
                                    items.push(node.clone());
                                }
                                items.push(edge.clone());
                            }
                            if let Some(last) = nodes.last()
                                && items.last() != Some(last)
                            {
                                items.push(last.clone());
                            }
                        }
                    }
                }
                self.regs[*dest as usize] = Reg::Value(Value::List(items.into()));
            }
            Op::ListPush { list, value } => {
                let value = self.reg_value(*value)?;
                let items = match self.reg_value(*list)? {
                    Value::List(items) => items,
                    other => {
                        return Err(Error::internal(format!(
                            "ListPush on {}",
                            other.type_name()
                        )));
                    }
                };
                let mut new_items = items.to_vec();
                new_items.push(value);
                self.regs[*list as usize] = Reg::Value(Value::List(new_items.into()));
            }
            Op::UnwindInit { cursor, list } => {
                let value = self.reg_value(*list)?;
                let items = match value {
                    Value::Null => Vec::new(),
                    Value::List(items) => items.to_vec(),
                    other => {
                        return Err(Error::type_error(format!(
                            "UNWIND expects a list, got {}",
                            other.type_name()
                        )));
                    }
                };
                self.cursors[*cursor as usize] = Cursor::Unwind { items, pos: 0 };
            }
            Op::UnwindNext {
                cursor,
                dest,
                target,
            } => {
                let item = match &mut self.cursors[*cursor as usize] {
                    Cursor::Unwind { items, pos } => {
                        if *pos >= items.len() {
                            None
                        } else {
                            let item = items[*pos].clone();
                            *pos += 1;
                            Some(item)
                        }
                    }
                    _ => return Err(Error::internal("UnwindNext on a non-unwind cursor")),
                };
                match item {
                    Some(item) => self.regs[*dest as usize] = Reg::Value(item),
                    None => self.pc = *target as usize,
                }
            }
            Op::CreateNode {
                labels,
                props,
                dest,
            } => {
                let label_ids = self.resolve_labels(*labels)?;
                let props = self.resolve_props(*props)?;
                let id = self.db.create_node(&label_ids, props)?;
                self.regs[*dest as usize] = Reg::Node(id);
            }
            Op::CreateEdge {
                edge_type,
                src,
                dst,
                props,
                dest,
            } => {
                let type_id = self.resolve_edge_type(*edge_type)?;
                let src_id = self.reg_node(*src)?;
                let dst_id = self.reg_node(*dst)?;
                let props = self.resolve_props(*props)?;
                let id = self
                    .db
                    .create_edge_unchecked(type_id, src_id, dst_id, props)?;
                self.regs[*dest as usize] = Reg::Edge(id);
            }
            Op::CheckMergeProps { props } => {
                let props = self.resolve_props(*props)?;
                if props.iter().any(|(_, value)| value.is_null()) {
                    return Err(Error::unsupported(
                        "MERGE does not allow null property values",
                    ));
                }
            }
            Op::SetProperty { target, key, value } => {
                let key_name = self.reg_text(*key)?;
                let value = self.reg_value(*value)?;
                match self.reg_value(*target)? {
                    Value::Node(id) => {
                        let key_id = self.db.ensure_property_key(&key_name)?;
                        self.db.set_node_property(id, key_id, value)?;
                    }
                    Value::Edge(id) => {
                        let key_id = self.db.ensure_property_key(&key_name)?;
                        self.db.set_edge_property(id, key_id, value)?;
                    }
                    Value::Null => {}
                    other => {
                        return Err(Error::type_error(format!(
                            "SET expects a node or relationship, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            Op::SetLabels {
                target,
                labels,
                add,
            } => {
                // Setting labels on a null binding is a no-op.
                if !matches!(self.regs[*target as usize], Reg::Value(Value::Null)) {
                    let id = self.reg_node(*target)?;
                    let label_ids = self.resolve_labels(*labels)?;
                    if *add {
                        self.db.add_node_labels(id, &label_ids)?;
                    } else {
                        self.db.remove_node_labels(id, &label_ids)?;
                    }
                }
            }
            Op::ReplaceProperties { target, props } => {
                // `SET x = y` copies y's properties when y is an entity.
                let value = self.reg_value(*props)?;
                let entries = match value {
                    Value::Map(_) => self.resolve_props(*props)?,
                    Value::Node(id) => self
                        .db
                        .get_node(id)?
                        .map(|record| record.props)
                        .unwrap_or_default(),
                    Value::Edge(id) => self
                        .db
                        .get_edge(id)?
                        .map(|record| record.props)
                        .unwrap_or_default(),
                    Value::Null => Vec::new(),
                    other => {
                        return Err(Error::type_error(format!(
                            "SET expects a map, node or relationship, got {}",
                            other.type_name()
                        )));
                    }
                };
                // Replacing properties on a null binding is a no-op.
                match self.reg_value(*target)? {
                    Value::Null => {}
                    Value::Node(id) => {
                        self.db.replace_node_properties(id, entries)?;
                    }
                    Value::Edge(id) => {
                        self.db.replace_edge_properties(id, entries)?;
                    }
                    other => {
                        return Err(Error::type_error(format!(
                            "SET expects a node or relationship, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            Op::MergeProperties { target, props } => {
                // Merging into a null binding is a no-op. Entries with a null
                // value remove the property, like `SET n.key = null`.
                if !matches!(self.regs[*target as usize], Reg::Value(Value::Null)) {
                    let entries = self.resolve_props(*props)?;
                    match self.reg_value(*target)? {
                        Value::Node(id) => {
                            for (key, value) in entries {
                                self.db.set_node_property(id, key, value)?;
                            }
                        }
                        Value::Edge(id) => {
                            for (key, value) in entries {
                                self.db.set_edge_property(id, key, value)?;
                            }
                        }
                        other => {
                            return Err(Error::type_error(format!(
                                "SET += expects a node or relationship, got {}",
                                other.type_name()
                            )));
                        }
                    }
                }
            }
            Op::DeleteAny { target, detach } => match self.reg_value(*target)? {
                Value::Node(id) => {
                    self.db.delete_node(id, *detach)?;
                }
                Value::Edge(id) => {
                    self.db.delete_edge(id)?;
                }
                Value::Null => {}
                other => {
                    return Err(Error::type_error(format!(
                        "DELETE expects a node or relationship, got {}",
                        other.type_name()
                    )));
                }
            },
        }
        Ok(())
    }

    fn reg_value(&self, reg: u32) -> Result<Value> {
        self.regs
            .get(reg as usize)
            .map(|reg| reg.to_value())
            .ok_or_else(|| Error::internal("register index out of range"))
    }

    fn reg_node(&self, reg: u32) -> Result<u64> {
        match self.regs.get(reg as usize) {
            Some(Reg::Node(id)) => Ok(*id),
            // Entities that travelled through a list (collect/UNWIND) lose
            // their bound-register form and arrive as plain values.
            Some(Reg::Value(Value::Node(id))) => Ok(*id),
            Some(other) => Err(Error::type_error(format!(
                "expected a node, got {}",
                other.to_value().type_name()
            ))),
            None => Err(Error::internal("register index out of range")),
        }
    }

    fn reg_text(&self, reg: u32) -> Result<String> {
        match self.regs.get(reg as usize) {
            Some(Reg::Value(Value::Text(text))) => Ok(text.to_string()),
            _ => Err(Error::type_error("expected a string")),
        }
    }

    fn reg_as_count(&self, reg: u32) -> Result<usize> {
        match self.reg_value(reg)? {
            Value::Null => Ok(0),
            Value::Int(value) if value >= 0 => Ok(value as usize),
            other => Err(Error::type_error(format!(
                "expected a non-negative integer, got {}",
                other.type_name()
            ))),
        }
    }

    fn column_value(&mut self, target: u32, key: u32) -> Result<Value> {
        match self.regs[target as usize].clone() {
            // Property access on a null binding (an optional match that
            // missed) is null, not an error.
            Reg::Value(Value::Null) => Ok(Value::Null),
            Reg::Node(id) | Reg::Value(Value::Node(id)) => Ok(self
                .db
                .get_node(id)?
                .and_then(|record| record.get(key).cloned())
                .unwrap_or(Value::Null)),
            Reg::Edge(id) | Reg::Value(Value::Edge(id)) => Ok(self
                .db
                .get_edge(id)?
                .and_then(|record| record.get(key).cloned())
                .unwrap_or(Value::Null)),
            other => Err(Error::type_error(format!(
                "property access on {}",
                other.to_value().type_name()
            ))),
        }
    }

    fn resolve_labels(&mut self, reg: u32) -> Result<Vec<u32>> {
        let value = self.reg_value(reg)?;
        let names = match value {
            Value::List(items) => items,
            other => {
                return Err(Error::type_error(format!(
                    "expected a label list, got {}",
                    other.type_name()
                )));
            }
        };
        let mut ids = Vec::with_capacity(names.len());
        for name in names.iter() {
            let Value::Text(name) = name else {
                return Err(Error::type_error("label names must be strings"));
            };
            ids.push(self.db.ensure_label(name)?);
        }
        Ok(ids)
    }

    fn resolve_edge_type(&mut self, reg: u32) -> Result<u32> {
        let name = self.reg_text(reg)?;
        self.db.ensure_edge_type(&name)
    }

    /// Resolve a label name constant to its id, caching successes only so
    /// labels created by the same statement are still found.
    fn resolve_label(&mut self, name: u32) -> Result<Option<u32>> {
        match self.label_ids.get(name as usize).copied() {
            Some(Some(id)) => Ok(Some(id)),
            Some(None) => {
                let Value::Text(label) = &self.program.constants[name as usize] else {
                    return Err(Error::internal("label name is not a string"));
                };
                let resolved = self.db.label_id(label);
                if resolved.is_some() {
                    self.label_ids[name as usize] = resolved;
                }
                Ok(resolved)
            }
            None => Err(Error::internal("constant index out of range")),
        }
    }

    /// Resolve edge type name constants to ids. `None` means the pattern had
    /// no type filter (any type); `Some` with an empty list means no listed
    /// type exists, so nothing can match.
    fn resolve_types(&mut self, names: &[u32], any_type: bool) -> Result<Option<Vec<u32>>> {
        if any_type {
            return Ok(None);
        }
        let mut types = Vec::with_capacity(names.len());
        for name in names {
            let Value::Text(ty) = &self.program.constants[*name as usize] else {
                return Err(Error::internal("edge type name is not a string"));
            };
            if let Some(id) = self.db.edge_type_id(ty) {
                types.push(id);
            }
        }
        Ok(Some(types))
    }

    fn resolve_props(&mut self, reg: u32) -> Result<Vec<(u32, Value)>> {
        let value = self.reg_value(reg)?;
        let entries = match value {
            Value::Map(entries) => entries,
            Value::Null => return Ok(Vec::new()),
            other => {
                return Err(Error::type_error(format!(
                    "expected a property map, got {}",
                    other.type_name()
                )));
            }
        };
        let mut props = Vec::with_capacity(entries.len());
        for (key, value) in entries.iter() {
            let key_id = self.db.ensure_property_key(key)?;
            props.push((key_id, value.clone()));
        }
        Ok(props)
    }

    /// Output rows of a supported built-in procedure.
    fn procedure_rows(&self, name: &str) -> Result<Vec<Vec<Reg>>> {
        let schema = self.db.schema();
        let mut values = Vec::new();
        let mut id = 1u32;
        loop {
            let next = match name {
                "db.labels" => schema.label_name(id),
                "db.propertyKeys" => schema.property_key_name(id),
                "db.relationshipTypes" => schema.edge_type_name(id),
                other => {
                    return Err(Error::unsupported(format!(
                        "procedure `{other}` is not supported"
                    )));
                }
            };
            match next {
                Some(entry) => {
                    values.push(Reg::Value(Value::Text(entry.into())));
                    id += 1;
                }
                None => break,
            }
        }
        Ok(values.into_iter().map(|value| vec![value]).collect())
    }

    fn rows_mut(&mut self, cursor: u32) -> Result<(&mut Vec<Vec<Reg>>, usize)> {
        match &mut self.cursors[cursor as usize] {
            Cursor::Rows { rows, pos } => Ok((rows, *pos)),
            _ => Err(Error::internal("operation on a non-row cursor")),
        }
    }

    fn hash_aggregate(
        &mut self,
        cursor: u32,
        prefix: u32,
        groups: u32,
        aggs: &[AggSpec],
    ) -> Result<()> {
        let (rows, _) = self.rows_mut(cursor)?;
        let rows = std::mem::take(rows);
        let prefix = prefix as usize;
        let group_count = groups as usize;
        let mut table: BTreeMap<Vec<Reg>, Vec<AggState>> = BTreeMap::new();
        for row in &rows {
            let key: Vec<Reg> = row[prefix..prefix + group_count].to_vec();
            let accumulators = table
                .entry(key)
                .or_insert_with(|| aggs.iter().map(|spec| AggState::new(*spec)).collect());
            let mut offset = prefix + group_count;
            for (idx, spec) in aggs.iter().enumerate() {
                let inputs = &row[offset..offset + spec.args as usize];
                accumulators[idx].step(spec, inputs)?;
                offset += spec.args as usize;
            }
        }
        if table.is_empty() && group_count == 0 {
            // Aggregation without grouping over zero rows yields one row with
            // the aggregations' default values (e.g. count(*) = 0).
            table.insert(
                Vec::new(),
                aggs.iter().map(|spec| AggState::new(*spec)).collect(),
            );
        }
        let mut output = Vec::with_capacity(table.len());
        for (key, mut accumulators) in table {
            let mut row = key;
            for (idx, spec) in aggs.iter().enumerate() {
                let state = std::mem::replace(&mut accumulators[idx], AggState::new(*spec));
                row.push(Reg::Value(state.finish(spec)));
            }
            output.push(row);
        }
        if let Cursor::Rows { rows, pos } = &mut self.cursors[cursor as usize] {
            *rows = output;
            *pos = 0;
        }
        Ok(())
    }

    fn call_function(&mut self, func: u8, args: &[Value]) -> Result<Value> {
        match func {
            func::ID => match args.first() {
                Some(Value::Node(id)) | Some(Value::Edge(id)) => Ok(Value::Int(*id as i64)),
                _ => Ok(Value::Null),
            },
            func::LABELS => match args.first() {
                Some(Value::Node(id)) => {
                    let Some(record) = self.db.get_node(*id)? else {
                        return Ok(Value::Null);
                    };
                    let mut names = Vec::with_capacity(record.labels.len());
                    for label in &record.labels {
                        if let Some(name) = self.db.schema().label_name(*label) {
                            names.push(Value::Text(std::sync::Arc::from(name)));
                        }
                    }
                    Ok(Value::List(names.into()))
                }
                _ => Ok(Value::Null),
            },
            func::TYPE => match args.first() {
                Some(Value::Edge(id)) => {
                    let Some(record) = self.db.get_edge(*id)? else {
                        return Ok(Value::Null);
                    };
                    Ok(self
                        .db
                        .schema()
                        .edge_type_name(record.edge_type)
                        .map(|name| Value::Text(std::sync::Arc::from(name)))
                        .unwrap_or(Value::Null))
                }
                _ => Ok(Value::Null),
            },
            func::START_NODE | func::END_NODE => match args.first() {
                Some(Value::Edge(id)) => {
                    let Some(record) = self.db.get_edge(*id)? else {
                        return Ok(Value::Null);
                    };
                    let node = if func == func::START_NODE {
                        record.src
                    } else {
                        record.dst
                    };
                    Ok(Value::Node(node))
                }
                _ => Ok(Value::Null),
            },
            func::COALESCE => Ok(args
                .iter()
                .find(|value| !value.is_null())
                .cloned()
                .unwrap_or(Value::Null)),
            func::TO_STRING => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::Int(value)) => Ok(Value::Text(std::sync::Arc::from(value.to_string()))),
                Some(Value::Bool(value)) => {
                    Ok(Value::Text(std::sync::Arc::from(value.to_string())))
                }
                Some(Value::Float(value)) => {
                    let mut buffer = ryu::Buffer::new();
                    Ok(Value::Text(std::sync::Arc::from(buffer.format(*value))))
                }
                Some(Value::Text(value)) => Ok(Value::Text(value.clone())),
                Some(
                    value @ (Value::Date(_)
                    | Value::LocalTime(_)
                    | Value::Time(_)
                    | Value::LocalDateTime(_)
                    | Value::DateTime(_)
                    | Value::Duration(_)
                    | Value::Point(_)),
                ) => Ok(Value::Text(std::sync::Arc::from(
                    format_value(value).as_str(),
                ))),
                Some(other) => Err(invalid_argument("toString", other)),
            },
            func::TO_INTEGER => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::Int(value)) => Ok(Value::Int(*value)),
                Some(Value::Float(value)) => Ok(truncate_to_int(*value)
                    .map(Value::Int)
                    .unwrap_or(Value::Null)),
                Some(Value::Bool(value)) => Ok(Value::Int(i64::from(*value))),
                Some(Value::Text(text)) => {
                    Ok(text_to_integer(text).map(Value::Int).unwrap_or(Value::Null))
                }
                Some(other) => Err(invalid_argument("toInteger", other)),
            },
            func::TO_FLOAT => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::Int(value)) => Ok(Value::Float(*value as f64)),
                Some(Value::Float(value)) => Ok(Value::Float(*value)),
                Some(Value::Text(text)) => Ok(text
                    .trim()
                    .parse::<f64>()
                    .map(Value::Float)
                    .unwrap_or(Value::Null)),
                Some(other) => Err(invalid_argument("toFloat", other)),
            },
            func::TO_BOOLEAN => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::Bool(value)) => Ok(Value::Bool(*value)),
                Some(Value::Text(text)) => Ok(match text.to_ascii_lowercase().as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::Null,
                }),
                Some(other) => Err(invalid_argument("toBoolean", other)),
            },
            func::SIZE => Ok(match args.first() {
                Some(Value::List(items)) => Value::Int(items.len() as i64),
                Some(Value::Text(text)) => Value::Int(text.chars().count() as i64),
                _ => Value::Null,
            }),
            func::NODES => Ok(match args.first() {
                Some(Value::List(items)) => Value::List(
                    items
                        .iter()
                        .filter(|item| matches!(item, Value::Node(_)))
                        .cloned()
                        .collect::<Vec<_>>()
                        .into(),
                ),
                _ => Value::Null,
            }),
            func::RELATIONSHIPS => Ok(match args.first() {
                Some(Value::List(items)) => Value::List(
                    items
                        .iter()
                        .filter(|item| matches!(item, Value::Edge(_)))
                        .cloned()
                        .collect::<Vec<_>>()
                        .into(),
                ),
                _ => Value::Null,
            }),
            func::LENGTH => Ok(match args.first() {
                Some(Value::List(items)) => Value::Int(
                    items
                        .iter()
                        .filter(|item| matches!(item, Value::Edge(_)))
                        .count() as i64,
                ),
                _ => Value::Null,
            }),
            func::RANGE => call_range(args),
            func::HEAD | func::LAST => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::List(items)) => Ok(match func {
                    func::HEAD => items.first().cloned().unwrap_or(Value::Null),
                    _ => items.last().cloned().unwrap_or(Value::Null),
                }),
                Some(other) => Err(invalid_argument(
                    if func == func::HEAD { "head" } else { "last" },
                    other,
                )),
            },
            func::TAIL => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::List(items)) => {
                    Ok(Value::List(items.iter().skip(1).cloned().collect()))
                }
                Some(other) => Err(invalid_argument("tail", other)),
            },
            func::REVERSE => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::Text(text)) => {
                    Ok(Value::Text(text.chars().rev().collect::<String>().into()))
                }
                Some(Value::List(items)) => Ok(Value::List(items.iter().rev().cloned().collect())),
                Some(other) => Err(invalid_argument("reverse", other)),
            },
            func::SPLIT => call_split(args),
            func::SUBSTRING => call_substring(args),
            func::RAND => Ok(Value::Float(fastrand::f64())),
            func::TO_UPPER => text_map(args, "toUpper", |text| text.to_uppercase()),
            func::TO_LOWER => text_map(args, "toLower", |text| text.to_lowercase()),
            func::TRIM => text_map(args, "trim", |text| text.trim().to_owned()),
            func::LTRIM => text_map(args, "ltrim", |text| text.trim_start().to_owned()),
            func::RTRIM => text_map(args, "rtrim", |text| text.trim_end().to_owned()),
            func::LEFT | func::RIGHT => call_left_right(func, args),
            func::REPLACE => call_replace(args),
            func::ABS => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::Int(value)) => Ok(match value.checked_abs() {
                    Some(value) => Value::Int(value),
                    // `abs(i64::MIN)` overflows into float, like Neo4j.
                    None => Value::Float((*value as f64).abs()),
                }),
                Some(Value::Float(value)) => Ok(Value::Float(value.abs())),
                Some(other) => Err(invalid_argument("abs", other)),
            },
            func::SIGN => match args.first() {
                Some(Value::Null) | None => Ok(Value::Null),
                Some(Value::Int(value)) => Ok(Value::Int(value.signum())),
                Some(Value::Float(value)) => Ok(Value::Int(if value.is_nan() {
                    0
                } else if *value > 0.0 {
                    1
                } else if *value < 0.0 {
                    -1
                } else {
                    0
                })),
                Some(other) => Err(invalid_argument("sign", other)),
            },
            func::CEIL => float_map(args, "ceil", f64::ceil),
            func::FLOOR => float_map(args, "floor", f64::floor),
            func::ROUND => float_map(args, "round", f64::round),
            func::SQRT => float_map(args, "sqrt", f64::sqrt),
            func::EXP => float_map(args, "exp", f64::exp),
            func::LOG => float_map(args, "log", f64::ln),
            func::LOG10 => float_map(args, "log10", f64::log10),
            func::SIN => float_map(args, "sin", f64::sin),
            func::COS => float_map(args, "cos", f64::cos),
            func::TAN => float_map(args, "tan", f64::tan),
            func::COT => float_map(args, "cot", |value| 1.0 / value.tan()),
            func::ASIN => float_map(args, "asin", f64::asin),
            func::ACOS => float_map(args, "acos", f64::acos),
            func::ATAN => float_map(args, "atan", f64::atan),
            func::ATAN2 => match (args.first(), args.get(1)) {
                (Some(Value::Null), _) | (_, Some(Value::Null)) | (None, _) => Ok(Value::Null),
                (Some(first), Some(second)) => {
                    let y = number_argument("atan2", first)?;
                    let x = number_argument("atan2", second)?;
                    Ok(Value::Float(y.atan2(x)))
                }
                (Some(other), None) => Err(invalid_argument("atan2", other)),
            },
            func::DEGREES => float_map(args, "degrees", f64::to_degrees),
            func::RADIANS => float_map(args, "radians", f64::to_radians),
            func::HAVERSIN => float_map(args, "haversin", |value| (1.0 - value.cos()) / 2.0),
            func::PI => Ok(Value::Float(std::f64::consts::PI)),
            func::E => Ok(Value::Float(std::f64::consts::E)),
            func::DATE => call_date(args, self.statement_time),
            func::LOCALTIME => call_localtime(args, self.statement_time),
            func::TIME => call_time(args, self.statement_time),
            func::LOCAL_DATETIME => call_localdatetime(args, self.statement_time),
            func::DATETIME => call_datetime(args, self.statement_time),
            func::DURATION => call_duration(args),
            func::POINT => call_point(args),
            func::DISTANCE => call_distance(args),
            func::DATE_TRUNCATE => call_truncate(TruncateKind::Date, args),
            func::LOCALTIME_TRUNCATE => call_truncate(TruncateKind::LocalTime, args),
            func::TIME_TRUNCATE => call_truncate(TruncateKind::Time, args),
            func::LOCAL_DATETIME_TRUNCATE => call_truncate(TruncateKind::LocalDateTime, args),
            func::DATETIME_TRUNCATE => call_truncate(TruncateKind::DateTime, args),
            func::DATETIME_FROM_EPOCH => call_datetime_from_epoch(args),
            func::DATETIME_FROM_EPOCH_MILLIS => call_datetime_from_epoch_millis(args),
            func::DURATION_BETWEEN => call_duration_between(DurationDiff::Between, args),
            func::DURATION_IN_MONTHS => call_duration_between(DurationDiff::InMonths, args),
            func::DURATION_IN_DAYS => call_duration_between(DurationDiff::InDays, args),
            func::DURATION_IN_SECONDS => call_duration_between(DurationDiff::InSeconds, args),
            func::PROPERTIES => match args.first() {
                Some(Value::Node(id)) => {
                    let Some(record) = self.db.get_node(*id)? else {
                        return Ok(Value::Null);
                    };
                    Ok(Value::Map(self.props_to_map(&record.props)))
                }
                Some(Value::Edge(id)) => {
                    let Some(record) = self.db.get_edge(*id)? else {
                        return Ok(Value::Null);
                    };
                    Ok(Value::Map(self.props_to_map(&record.props)))
                }
                // A map's properties are the map itself.
                Some(Value::Map(_)) => Ok(args.first().cloned().expect("checked above")),
                _ => Ok(Value::Null),
            },
            func::KEYS => match args.first() {
                Some(Value::Node(id)) => {
                    let Some(record) = self.db.get_node(*id)? else {
                        return Ok(Value::Null);
                    };
                    Ok(Value::List(self.prop_keys(&record.props).into()))
                }
                Some(Value::Edge(id)) => {
                    let Some(record) = self.db.get_edge(*id)? else {
                        return Ok(Value::Null);
                    };
                    Ok(Value::List(self.prop_keys(&record.props).into()))
                }
                Some(Value::Map(entries)) => Ok(Value::List(
                    entries
                        .iter()
                        .map(|(key, _)| Value::Text(key.clone()))
                        .collect::<Vec<_>>()
                        .into(),
                )),
                _ => Ok(Value::Null),
            },
            _ => Err(Error::internal("unknown function code")),
        }
    }

    fn props_to_map(
        &self,
        props: &[(u32, Value)],
    ) -> std::sync::Arc<[(std::sync::Arc<str>, Value)]> {
        let entries: Vec<(std::sync::Arc<str>, Value)> = props
            .iter()
            .filter_map(|(key, value)| {
                self.db
                    .schema()
                    .property_key_name(*key)
                    .map(|name| (std::sync::Arc::from(name), value.clone()))
            })
            .collect();
        entries.into()
    }

    fn prop_keys(&self, props: &[(u32, Value)]) -> Vec<Value> {
        props
            .iter()
            .filter_map(|(key, _)| {
                self.db
                    .schema()
                    .property_key_name(*key)
                    .map(|name| Value::Text(std::sync::Arc::from(name)))
            })
            .collect()
    }
}

/// One group's aggregation state: the accumulator plus, for `DISTINCT`
/// aggregations, the values already seen.
struct AggState {
    accumulator: Accumulator,
    seen: std::collections::BTreeSet<Value>,
}

impl AggState {
    fn new(spec: AggSpec) -> Self {
        Self {
            accumulator: Accumulator::new(spec),
            seen: std::collections::BTreeSet::new(),
        }
    }

    fn step(&mut self, spec: &AggSpec, inputs: &[Reg]) -> Result<()> {
        if spec.distinct {
            let value = inputs[0].to_value();
            if value.is_null() || !self.seen.insert(value) {
                return Ok(());
            }
        }
        self.accumulator.step(spec, inputs)
    }

    fn finish(self, spec: &AggSpec) -> Value {
        self.accumulator.finish(spec)
    }
}

/// Aggregation accumulators.
enum Accumulator {
    Count(i64),
    Sum(SumState),
    Avg {
        sum: f64,
        count: i64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect(Vec<Value>),
    /// `percentileCont`: interpolated numeric values and the latest
    /// percentile argument.
    PercentileCont {
        values: Vec<f64>,
        percentile: Option<f64>,
    },
    /// `percentileDisc`: original values (types preserved) and the latest
    /// percentile argument.
    PercentileDisc {
        values: Vec<Value>,
        percentile: Option<f64>,
    },
    /// Welford's online variance for `stDev` (sample) and `stDevP`.
    StDev {
        count: u64,
        mean: f64,
        m2: f64,
        sample: bool,
    },
}

enum SumState {
    Empty,
    Int(i64),
    Float(f64),
}

impl Accumulator {
    fn new(spec: AggSpec) -> Self {
        match spec.func {
            AggFn::Count => Self::Count(0),
            AggFn::Sum => Self::Sum(SumState::Empty),
            AggFn::Avg => Self::Avg { sum: 0.0, count: 0 },
            AggFn::Min => Self::Min(None),
            AggFn::Max => Self::Max(None),
            AggFn::Collect => Self::Collect(Vec::new()),
            AggFn::PercentileCont => Self::PercentileCont {
                values: Vec::new(),
                percentile: None,
            },
            AggFn::PercentileDisc => Self::PercentileDisc {
                values: Vec::new(),
                percentile: None,
            },
            AggFn::StDev | AggFn::StDevP => Self::StDev {
                count: 0,
                mean: 0.0,
                m2: 0.0,
                sample: spec.func == AggFn::StDev,
            },
        }
    }

    /// Feed one row's argument values into the accumulator. Percentile
    /// arguments are validated here so out-of-range values fail at runtime.
    fn step(&mut self, spec: &AggSpec, inputs: &[Reg]) -> Result<()> {
        match self {
            Self::PercentileCont { values, percentile } => {
                *percentile = Some(percentile_argument(inputs.get(1))?);
                if let Some(number) = inputs[0].to_value().as_number() {
                    values.push(number);
                }
                return Ok(());
            }
            Self::PercentileDisc { values, percentile } => {
                *percentile = Some(percentile_argument(inputs.get(1))?);
                let value = inputs[0].to_value();
                if !value.is_null() {
                    values.push(value);
                }
                return Ok(());
            }
            Self::StDev {
                count, mean, m2, ..
            } => {
                if let Some(number) = inputs[0].to_value().as_number() {
                    // Welford's online algorithm keeps the running variance
                    // stable without buffering every value.
                    *count += 1;
                    let delta = number - *mean;
                    *mean += delta / *count as f64;
                    *m2 += delta * (number - *mean);
                }
                return Ok(());
            }
            _ => {}
        }
        self.step_value(spec, inputs[0].to_value());
        Ok(())
    }

    fn step_value(&mut self, spec: &AggSpec, value: Value) {
        match self {
            Self::Count(count) => {
                if spec.star || !value.is_null() {
                    *count += 1;
                }
            }
            Self::Sum(state) => match value {
                Value::Int(value) => {
                    *state = match std::mem::replace(state, SumState::Empty) {
                        SumState::Empty => SumState::Int(value),
                        SumState::Int(previous) => SumState::Int(previous.wrapping_add(value)),
                        SumState::Float(previous) => SumState::Float(previous + value as f64),
                    };
                }
                Value::Float(value) => {
                    *state = match std::mem::replace(state, SumState::Empty) {
                        SumState::Empty => SumState::Float(value),
                        SumState::Int(previous) => SumState::Float(previous as f64 + value),
                        SumState::Float(previous) => SumState::Float(previous + value),
                    };
                }
                _ => {}
            },
            Self::Avg { sum, count } => {
                if let Some(number) = value.as_number() {
                    *sum += number;
                    *count += 1;
                }
            }
            Self::Min(current) => {
                if !value.is_null() && current.as_ref().is_none_or(|c| value < *c) {
                    *current = Some(value);
                }
            }
            Self::Max(current) => {
                if !value.is_null() && current.as_ref().is_none_or(|c| value > *c) {
                    *current = Some(value);
                }
            }
            Self::Collect(items) => {
                if !value.is_null() {
                    items.push(value);
                }
            }
            // Multi-input accumulators are handled by `step` before this
            // point.
            Self::PercentileCont { .. } | Self::PercentileDisc { .. } | Self::StDev { .. } => {}
        }
    }

    fn finish(self, _spec: &AggSpec) -> Value {
        match self {
            Self::Count(count) => Value::Int(count),
            Self::Sum(state) => match state {
                SumState::Empty => Value::Int(0),
                SumState::Int(value) => Value::Int(value),
                SumState::Float(value) => Value::Float(value),
            },
            Self::Avg { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    Value::Float(sum / count as f64)
                }
            }
            Self::Min(value) | Self::Max(value) => value.unwrap_or(Value::Null),
            Self::Collect(items) => Value::List(items.into()),
            Self::PercentileCont {
                mut values,
                percentile,
            } => {
                let Some(percentile) = percentile.filter(|_| !values.is_empty()) else {
                    return Value::Null;
                };
                values.sort_by(f64::total_cmp);
                let position = percentile * (values.len() - 1) as f64;
                let lower = position.floor() as usize;
                let upper = position.ceil() as usize;
                let fraction = position - lower as f64;
                Value::Float(values[lower] + (values[upper] - values[lower]) * fraction)
            }
            Self::PercentileDisc {
                mut values,
                percentile,
            } => {
                let Some(percentile) = percentile.filter(|_| !values.is_empty()) else {
                    return Value::Null;
                };
                values.sort();
                // Nearest-rank method: the first value at or above the
                // requested percentile rank.
                let rank = (percentile * values.len() as f64).ceil().max(1.0);
                values[rank as usize - 1].clone()
            }
            Self::StDev {
                count, m2, sample, ..
            } => {
                if count == 0 {
                    Value::Null
                } else if sample && count == 1 {
                    Value::Float(0.0)
                } else {
                    let divisor = if sample {
                        (count - 1) as f64
                    } else {
                        count as f64
                    };
                    Value::Float((m2 / divisor).sqrt())
                }
            }
        }
    }
}

/// Interpret the percentile argument of `percentileCont`/`percentileDisc`,
/// which must be a number between 0 and 1.
fn percentile_argument(input: Option<&Reg>) -> Result<f64> {
    let value = input.map(Reg::to_value).unwrap_or(Value::Null);
    let number = match value {
        Value::Int(number) => number as f64,
        Value::Float(number) => number,
        other => return Err(invalid_argument("percentile", &other)),
    };
    if !(0.0..=1.0).contains(&number) {
        return Err(Error::new(
            crate::error::ErrorCode::Range,
            "percentile must be between 0 and 1",
        ));
    }
    Ok(number)
}

/// Add a duration to a date: whole months first (clamped), then the days
/// field plus the whole days contained in the seconds field, matching
/// Neo4j's calendar arithmetic.
fn date_plus_duration(date: Date, duration: Duration) -> Result<Date> {
    let out_of_range = || Error::type_error("date arithmetic out of range");
    let date = date
        .add_months(duration.months())
        .ok_or_else(out_of_range)?;
    let extra_days = duration.days() + duration.seconds() / 86_400;
    date.add_days(extra_days).ok_or_else(out_of_range)
}

/// Add a duration to a wall-clock time; months and days do not contribute.
fn localtime_plus_duration(time: LocalTime, duration: Duration) -> LocalTime {
    let rest = duration.seconds() % 86_400;
    let nanos = time.nanos() + rest * 1_000_000_000 + duration.nanos() as i64;
    LocalTime::from_nanos_wrapped(nanos)
}

/// Add a duration to a zone-less datetime: calendar months and days first,
/// then the remaining time of day.
fn localdatetime_plus_duration(
    datetime: LocalDateTime,
    duration: Duration,
) -> Result<LocalDateTime> {
    let out_of_range = || Error::type_error("datetime arithmetic out of range");
    let date = date_plus_duration(datetime.date(), duration)?;
    let nanos_of_day = datetime.time().nanos()
        + (duration.seconds() % 86_400) * 1_000_000_000
        + duration.nanos() as i64;
    let carry = nanos_of_day.div_euclid(86_400_000_000_000);
    let rest = nanos_of_day.rem_euclid(86_400_000_000_000);
    let date = date.add_days(carry).ok_or_else(out_of_range)?;
    Ok(LocalDateTime::new(
        date,
        LocalTime::from_nanos(rest).expect("wrapped time of day"),
    ))
}

/// Add a duration to an offset/zoned datetime; named zones resolve their
/// offset again for the resulting local time.
fn datetime_plus_duration(datetime: &DateTime, duration: Duration) -> Result<DateTime> {
    let local = localdatetime_plus_duration(datetime.local(), duration)?;
    let offset = match datetime.zone() {
        Some(zone) => crate::temporal::construct::zone_offset(zone, local.date(), local.time())
            .map_err(Error::type_error)?,
        None => datetime.offset(),
    };
    DateTime::new(local, offset, datetime.zone().map(Arc::<str>::from))
        .ok_or_else(|| Error::type_error("datetime arithmetic out of range"))
}

/// Temporal `+`; `None` falls back to the numeric/concatenation path.
fn temporal_add(a: &Value, b: &Value) -> Result<Option<Value>> {
    let (temporal, duration) = match (a, b) {
        (Value::Duration(duration), other) | (other, Value::Duration(duration)) => {
            (other, Some(*duration))
        }
        (other, _) => (other, None),
    };
    let Some(duration) = duration else {
        return Ok(None);
    };
    match temporal {
        Value::Null => Ok(Some(Value::Null)),
        Value::Duration(other) => Ok(Some(Value::Duration(
            (*other + duration).ok_or_else(|| Error::type_error("duration overflow"))?,
        ))),
        Value::Date(date) => Ok(Some(Value::Date(date_plus_duration(*date, duration)?))),
        Value::LocalTime(time) => Ok(Some(Value::LocalTime(localtime_plus_duration(
            *time, duration,
        )))),
        Value::Time(time) => Ok(Some(Value::Time(
            Time::new(
                localtime_plus_duration(time.local(), duration),
                time.offset(),
            )
            .expect("valid offset"),
        ))),
        Value::LocalDateTime(datetime) => Ok(Some(Value::LocalDateTime(
            localdatetime_plus_duration(*datetime, duration)?,
        ))),
        Value::DateTime(datetime) => Ok(Some(Value::DateTime(datetime_plus_duration(
            datetime, duration,
        )?))),
        other => Err(Error::type_error(format!(
            "cannot add a duration to {}",
            other.type_name()
        ))),
    }
}

/// Temporal `-`; `None` falls back to the numeric path.
fn temporal_sub(a: &Value, b: &Value) -> Result<Option<Value>> {
    match (a, b) {
        (Value::Duration(left), Value::Duration(right)) => Ok(Some(Value::Duration(
            (*left - *right).ok_or_else(|| Error::type_error("duration overflow"))?,
        ))),
        (Value::Null, Value::Duration(_)) | (Value::Duration(_), Value::Null) => {
            Ok(Some(Value::Null))
        }
        (_, Value::Duration(right)) => temporal_add(a, &Value::Duration(-*right)),
        _ => Ok(None),
    }
}

/// Duration multiplication (and division when `divide` is set); `None`
/// falls back to the numeric path.
fn temporal_scale(a: &Value, b: &Value, divide: bool) -> Result<Option<Value>> {
    match (a, b) {
        (Value::Null, Value::Duration(_)) | (Value::Duration(_), Value::Null) => {
            Ok(Some(Value::Null))
        }
        (Value::Duration(duration), Value::Int(factor)) => {
            scale_duration(*duration, *factor as f64, divide).map(Some)
        }
        (Value::Duration(duration), Value::Float(factor)) => {
            scale_duration(*duration, *factor, divide).map(Some)
        }
        _ => Ok(None),
    }
}

fn scale_duration(duration: Duration, factor: f64, divide: bool) -> Result<Value> {
    if divide && factor == 0.0 {
        return Ok(Value::Null);
    }
    let scaled = if divide {
        duration.divided(factor)
    } else {
        duration.multiplied(factor)
    };
    scaled
        .map(Value::Duration)
        .ok_or_else(|| Error::type_error("duration arithmetic out of range"))
}

/// Operator codes for [`Op::Unary`].
pub mod unary_code {
    /// Negation.
    pub const NEG: u8 = 0;
    /// Boolean not.
    pub const NOT: u8 = 1;
}

/// Operator codes for [`Op::Binary`].
pub mod binary_code {
    use crate::cypher::BinaryOp;

    /// `+`
    pub const ADD: u8 = 0;
    /// `-`
    pub const SUB: u8 = 1;
    /// `*`
    pub const MUL: u8 = 2;
    /// `/`
    pub const DIV: u8 = 3;
    /// `%`
    pub const MOD: u8 = 4;
    /// `^`
    pub const POW: u8 = 5;
    /// `=`
    pub const EQ: u8 = 6;
    /// `<>`
    pub const NE: u8 = 7;
    /// `<`
    pub const LT: u8 = 8;
    /// `<=`
    pub const LE: u8 = 9;
    /// `>`
    pub const GT: u8 = 10;
    /// `>=`
    pub const GE: u8 = 11;
    /// `AND`
    pub const AND: u8 = 12;
    /// `OR`
    pub const OR: u8 = 13;
    /// `XOR`
    pub const XOR: u8 = 14;
    /// `IN`
    pub const IN: u8 = 15;
    /// `STARTS WITH`
    pub const STARTS_WITH: u8 = 16;
    /// `ENDS WITH`
    pub const ENDS_WITH: u8 = 17;
    /// `CONTAINS`
    pub const CONTAINS: u8 = 18;
    /// `=~`
    pub const REGEX: u8 = 19;

    /// Encode a binary operator.
    pub const fn encode(op: BinaryOp) -> u8 {
        match op {
            BinaryOp::Add => ADD,
            BinaryOp::Sub => SUB,
            BinaryOp::Mul => MUL,
            BinaryOp::Div => DIV,
            BinaryOp::Mod => MOD,
            BinaryOp::Pow => POW,
            BinaryOp::Eq => EQ,
            BinaryOp::Ne => NE,
            BinaryOp::Lt => LT,
            BinaryOp::Le => LE,
            BinaryOp::Gt => GT,
            BinaryOp::Ge => GE,
            BinaryOp::And => AND,
            BinaryOp::Or => OR,
            BinaryOp::Xor => XOR,
            BinaryOp::In => IN,
            BinaryOp::StartsWith => STARTS_WITH,
            BinaryOp::EndsWith => ENDS_WITH,
            BinaryOp::Contains => CONTAINS,
            BinaryOp::Regex => REGEX,
        }
    }
}

/// Encode a unary operator.
pub const fn unary_code(op: UnaryOp) -> u8 {
    match op {
        UnaryOp::Neg => unary_code::NEG,
        UnaryOp::Not => unary_code::NOT,
    }
}

fn binary_op(op: u8, a: &Value, b: &Value) -> Result<Value> {
    use binary_code as code;
    match op {
        code::ADD => match temporal_add(a, b)? {
            Some(value) => Ok(value),
            None => numeric_or_concat(a, b, |x, y| x + y, |x, y| x.wrapping_add(y)),
        },
        code::SUB => match temporal_sub(a, b)? {
            Some(value) => Ok(value),
            None => numeric(a, b, |x, y| x - y, |x, y| x.wrapping_sub(y)),
        },
        code::MUL => match temporal_scale(a, b, false)? {
            Some(value) => Ok(value),
            None => match temporal_scale(b, a, false)? {
                Some(value) => Ok(value),
                None => numeric(a, b, |x, y| x * y, |x, y| x.wrapping_mul(y)),
            },
        },
        code::DIV => match temporal_scale(a, b, true)? {
            Some(value) => Ok(value),
            None => match (a, b) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Int(_), Value::Int(0)) => Ok(Value::Null),
                (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x.wrapping_div(*y))),
                _ => match (a.as_number(), b.as_number()) {
                    (Some(x), Some(y)) => Ok(Value::Float(x / y)),
                    _ => Err(type_error("division", a, b)),
                },
            },
        },
        code::MOD => match (a, b) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::Int(_), Value::Int(0)) => Ok(Value::Null),
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x.wrapping_rem(*y))),
            _ => match (a.as_number(), b.as_number()) {
                (Some(x), Some(y)) => Ok(Value::Float(x % y)),
                _ => Err(type_error("modulo", a, b)),
            },
        },
        code::POW => match (a, b) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            _ => match (a.as_number(), b.as_number()) {
                (Some(x), Some(y)) => Ok(Value::Float(x.powf(y))),
                _ => Err(type_error("exponentiation", a, b)),
            },
        },
        code::EQ => Ok(bool_value(value_equals(a, b))),
        code::NE => Ok(bool_value(value_equals(a, b).map(|equal| !equal))),
        code::LT | code::LE | code::GT | code::GE => Ok(compare_value(op, a, b)),
        code::AND => Ok(kleene_and(a, b)),
        code::OR => Ok(kleene_or(a, b)),
        code::XOR => Ok(kleene_xor(a, b)),
        code::IN => Ok(in_list(a, b)),
        code::STARTS_WITH | code::ENDS_WITH | code::CONTAINS => Ok(string_predicate(op, a, b)),
        code::REGEX => Ok(regex_match(a, b)),
        _ => Err(Error::internal("unknown binary operator code")),
    }
}

fn bool_value(value: Option<bool>) -> Value {
    match value {
        Some(value) => Value::Bool(value),
        None => Value::Null,
    }
}

fn numeric(
    a: &Value,
    b: &Value,
    float_op: impl Fn(f64, f64) -> f64,
    int_op: impl Fn(i64, i64) -> i64,
) -> Result<Value> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(int_op(*x, *y))),
        _ => match (a.as_number(), b.as_number()) {
            (Some(x), Some(y)) => Ok(Value::Float(float_op(x, y))),
            _ => Err(type_error("arithmetic", a, b)),
        },
    }
}

fn numeric_or_concat(
    a: &Value,
    b: &Value,
    float_op: impl Fn(f64, f64) -> f64,
    int_op: impl Fn(i64, i64) -> i64,
) -> Result<Value> {
    if let (Value::Text(x), Value::Text(y)) = (a, b) {
        return Ok(Value::Text(format!("{x}{y}").into()));
    }
    match (a, b) {
        (Value::List(x), Value::List(y)) => {
            let mut items = x.to_vec();
            items.extend(y.iter().cloned());
            return Ok(Value::List(items.into()));
        }
        // A scalar concatenates onto a list on either side.
        (Value::List(x), other) => {
            let mut items = x.to_vec();
            items.push(other.clone());
            return Ok(Value::List(items.into()));
        }
        (other, Value::List(y)) => {
            let mut items = vec![other.clone()];
            items.extend(y.iter().cloned());
            return Ok(Value::List(items.into()));
        }
        _ => {}
    }
    numeric(a, b, float_op, int_op)
}

/// Resolve a slice bound against the list length: negative bounds count from
/// the end and everything is clamped into range.
fn slice_bound(which: &str, value: &Value, len: i64) -> Result<usize> {
    let Value::Int(index) = value else {
        return Err(Error::type_error(format!(
            "list slice {which} bound must be an integer, got {}",
            value.type_name()
        )));
    };
    let resolved = if *index < 0 { len + index } else { *index };
    Ok(resolved.clamp(0, len) as usize)
}

fn type_error(operation: &str, a: &Value, b: &Value) -> Error {
    Error::type_error(format!(
        "cannot apply {operation} to {} and {}",
        a.type_name(),
        b.type_name()
    ))
}

/// Lexicographic list ordering for the comparison operators: element-wise,
/// then by length, with a null element making the comparison unknown.
fn list_order(a: &[Value], b: &[Value]) -> Option<std::cmp::Ordering> {
    for (left, right) in a.iter().zip(b.iter()) {
        if left.is_null() || right.is_null() {
            return None;
        }
        match value_order(left, right) {
            Some(std::cmp::Ordering::Equal) => {}
            Some(ordering) => return Some(ordering),
            None => return None,
        }
    }
    Some(a.len().cmp(&b.len()))
}

/// Ordering of two values under the comparison operators; `None` when the
/// types are incomparable (which yields null).
fn value_order(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(_), Value::Int(_))
        | (Value::Float(_), Value::Float(_))
        | (Value::Int(_), Value::Float(_))
        | (Value::Float(_), Value::Int(_)) => {
            if matches!(a, Value::Float(value) if value.is_nan())
                || matches!(b, Value::Float(value) if value.is_nan())
            {
                return None;
            }
            Some(a.as_number()?.total_cmp(&b.as_number()?))
        }
        (Value::Text(x), Value::Text(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::List(x), Value::List(y)) => list_order(x, y),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::LocalTime(x), Value::LocalTime(y)) => Some(x.cmp(y)),
        (Value::Time(x), Value::Time(y)) => Some(x.cmp(y)),
        (Value::LocalDateTime(x), Value::LocalDateTime(y)) => Some(x.cmp(y)),
        (Value::DateTime(x), Value::DateTime(y)) => Some(x.cmp(y)),
        (Value::Duration(x), Value::Duration(y)) => Some(x.cmp(y)),
        (Value::Point(x), Value::Point(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn compare_value(op: u8, a: &Value, b: &Value) -> Value {
    use binary_code as code;
    let ordering = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Value::Null,
        (Value::Int(_), Value::Int(_))
        | (Value::Float(_), Value::Float(_))
        | (Value::Int(_), Value::Float(_))
        | (Value::Float(_), Value::Int(_)) => {
            // NaN is incomparable: every ordering comparison is false.
            if matches!(a, Value::Float(value) if value.is_nan())
                || matches!(b, Value::Float(value) if value.is_nan())
            {
                return Value::Bool(false);
            }
            let (Some(x), Some(y)) = (a.as_number(), b.as_number()) else {
                return Value::Null;
            };
            x.total_cmp(&y)
        }
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::List(x), Value::List(y)) => {
            // Lists and paths are distinct types: comparing one with the
            // other yields null, like any other cross-type ordering.
            if crate::record::is_path_shaped(x) != crate::record::is_path_shaped(y) {
                return Value::Null;
            }
            match list_order(x, y) {
                Some(ordering) => ordering,
                None => return Value::Null,
            }
        }
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::LocalTime(x), Value::LocalTime(y)) => x.cmp(y),
        (Value::Time(x), Value::Time(y)) => x.cmp(y),
        (Value::LocalDateTime(x), Value::LocalDateTime(y)) => x.cmp(y),
        (Value::DateTime(x), Value::DateTime(y)) => x.cmp(y),
        (Value::Duration(x), Value::Duration(y)) => x.cmp(y),
        (Value::Point(x), Value::Point(y)) => x.cmp(y),
        _ => return Value::Null,
    };
    let result = match op {
        code::LT => ordering.is_lt(),
        code::LE => ordering.is_le(),
        code::GT => ordering.is_gt(),
        _ => ordering.is_ge(),
    };
    Value::Bool(result)
}

fn kleene_and(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Value::Bool(false),
        (Value::Bool(true), Value::Bool(true)) => Value::Bool(true),
        (Value::Null, Value::Bool(true)) | (Value::Bool(true), Value::Null) => Value::Null,
        (Value::Null, Value::Null) => Value::Null,
        _ => Value::Null,
    }
}

fn kleene_or(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Bool(true), _) | (_, Value::Bool(true)) => Value::Bool(true),
        (Value::Bool(false), Value::Bool(false)) => Value::Bool(false),
        _ => Value::Null,
    }
}

fn kleene_xor(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => Value::Bool(x ^ y),
        _ => Value::Null,
    }
}

fn in_list(a: &Value, b: &Value) -> Value {
    let Value::List(items) = b else {
        return Value::Null;
    };
    let mut unknown = false;
    for item in items.iter() {
        match value_equals(a, item) {
            Some(true) => return Value::Bool(true),
            Some(false) => {}
            None => unknown = true,
        }
    }
    if unknown {
        Value::Null
    } else {
        Value::Bool(false)
    }
}

fn string_predicate(op: u8, a: &Value, b: &Value) -> Value {
    use binary_code as code;
    match (a, b) {
        (Value::Text(text), Value::Text(needle)) => {
            let result = match op {
                code::STARTS_WITH => text.starts_with(needle.as_ref()),
                code::ENDS_WITH => text.ends_with(needle.as_ref()),
                _ => text.contains(needle.as_ref()),
            };
            Value::Bool(result)
        }
        _ => Value::Null,
    }
}

fn regex_match(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Text(text), Value::Text(pattern)) => match regex::Regex::new(pattern) {
            Ok(regex) => Value::Bool(regex.is_match(text)),
            Err(_) => Value::Null,
        },
        _ => Value::Null,
    }
}

/// Convert entity values returned by functions into entity registers.
fn normalize_reg(value: Value) -> Reg {
    match value {
        Value::Node(id) => Reg::Node(id),
        Value::Edge(id) => Reg::Edge(id),
        other => Reg::Value(other),
    }
}

/// Read a component from a temporal or spatial value (`d.year`, `t.offset`,
/// `du.months`, `p.x`).
fn temporal_accessor(value: &Value, key: &str) -> Result<Value> {
    let integer = Value::Int;
    match value {
        Value::Date(date) => match key {
            "year" => Ok(integer(date.year())),
            "quarter" => Ok(integer(date.quarter() as i64)),
            "month" => Ok(integer(date.month() as i64)),
            "week" => Ok(integer(date.iso_week().1 as i64)),
            "weekYear" => Ok(integer(date.iso_week().0)),
            "day" => Ok(integer(date.day() as i64)),
            "ordinalDay" => Ok(integer(date.ordinal_day() as i64)),
            "weekDay" => Ok(integer(date.iso_weekday() as i64)),
            "dayOfQuarter" => Ok(integer(date.day_of_quarter() as i64)),
            _ => Err(unknown_field(key, value)),
        },
        Value::LocalTime(time) => match key {
            "hour" => Ok(integer(time.hour())),
            "minute" => Ok(integer(time.minute())),
            "second" => Ok(integer(time.second())),
            "millisecond" => Ok(integer(time.millisecond())),
            "microsecond" => Ok(integer(time.microsecond())),
            "nanosecond" => Ok(integer(time.nanosecond())),
            _ => Err(unknown_field(key, value)),
        },
        Value::Time(time) => match key {
            "hour" => Ok(integer(time.local().hour())),
            "minute" => Ok(integer(time.local().minute())),
            "second" => Ok(integer(time.local().second())),
            "millisecond" => Ok(integer(time.local().millisecond())),
            "microsecond" => Ok(integer(time.local().microsecond())),
            "nanosecond" => Ok(integer(time.local().nanosecond())),
            "timezone" | "offset" => Ok(offset_text(time.offset())),
            "offsetMinutes" => Ok(integer(time.offset_minutes())),
            "offsetSeconds" => Ok(integer(time.offset_seconds())),
            _ => Err(unknown_field(key, value)),
        },
        Value::LocalDateTime(datetime) => match key {
            "year" => Ok(integer(datetime.date().year())),
            "quarter" => Ok(integer(datetime.date().quarter() as i64)),
            "month" => Ok(integer(datetime.date().month() as i64)),
            "week" => Ok(integer(datetime.date().iso_week().1 as i64)),
            "weekYear" => Ok(integer(datetime.date().iso_week().0)),
            "day" => Ok(integer(datetime.date().day() as i64)),
            "ordinalDay" => Ok(integer(datetime.date().ordinal_day() as i64)),
            "weekDay" => Ok(integer(datetime.date().iso_weekday() as i64)),
            "dayOfQuarter" => Ok(integer(datetime.date().day_of_quarter() as i64)),
            "hour" => Ok(integer(datetime.time().hour())),
            "minute" => Ok(integer(datetime.time().minute())),
            "second" => Ok(integer(datetime.time().second())),
            "millisecond" => Ok(integer(datetime.time().millisecond())),
            "microsecond" => Ok(integer(datetime.time().microsecond())),
            "nanosecond" => Ok(integer(datetime.time().nanosecond())),
            _ => Err(unknown_field(key, value)),
        },
        Value::DateTime(datetime) => {
            // The calendar and clock accessors come from the local civil
            // datetime; the rest are instant or zone related.
            if let Ok(value) = temporal_accessor(&Value::LocalDateTime(datetime.local()), key) {
                return Ok(value);
            }
            match key {
                "timezone" => Ok(match datetime.zone() {
                    Some(zone) => Value::Text(zone.into()),
                    None => Value::Null,
                }),
                "offset" => Ok(offset_text(datetime.offset())),
                "offsetMinutes" => Ok(integer(datetime.offset() as i64 / 60)),
                "offsetSeconds" => Ok(integer(datetime.offset() as i64)),
                "epochSeconds" => Ok(integer(datetime.epoch_seconds())),
                "epochMillis" => Ok(integer(datetime.epoch_millis())),
                _ => Err(unknown_field(key, value)),
            }
        }
        Value::Duration(duration) => {
            let number = match key {
                "years" => duration.years(),
                "quarters" => duration.quarters(),
                "months" => duration.months(),
                "weeks" => duration.weeks(),
                "days" => duration.days(),
                "hours" => duration.hours(),
                "minutes" => duration.minutes(),
                "seconds" => duration.seconds(),
                "milliseconds" => duration.milliseconds(),
                "microseconds" => duration.microseconds(),
                "nanoseconds" => duration.nanoseconds(),
                "quartersOfYear" => duration.quarters_of_year(),
                "monthsOfQuarter" => duration.months_of_quarter(),
                "monthsOfYear" => duration.months_of_year(),
                "daysOfWeek" => duration.days_of_week(),
                "minutesOfHour" => duration.minutes_of_hour(),
                "secondsOfMinute" => duration.seconds_of_minute(),
                "millisecondsOfSecond" => duration.milliseconds_of_second(),
                "microsecondsOfSecond" => duration.microseconds_of_second(),
                "nanosecondsOfSecond" => duration.nanoseconds_of_second(),
                _ => return Err(unknown_field(key, value)),
            };
            Ok(integer(number))
        }
        Value::Point(point) => match key {
            "x" => Ok(Value::Float(point.x())),
            "y" => Ok(Value::Float(point.y())),
            "z" => Ok(point.z().map(Value::Float).unwrap_or(Value::Null)),
            "longitude" => Ok(Value::Float(point.longitude())),
            "latitude" => Ok(Value::Float(point.latitude())),
            "height" => Ok(point.height().map(Value::Float).unwrap_or(Value::Null)),
            "srid" => Ok(integer(point.srid() as i64)),
            "crs" => Ok(Value::Text(point.crs().into())),
            _ => Err(unknown_field(key, value)),
        },
        _ => Err(unknown_field(key, value)),
    }
}

fn offset_text(offset: i32) -> Value {
    Value::Text(crate::temporal::datetime::format_offset(offset).into())
}

fn unknown_field(key: &str, value: &Value) -> Error {
    Error::type_error(format!(
        "unknown field `{key}` on a {} value",
        value.type_name()
    ))
}

/// Convert a field map into constructor fields, or `None` when the value is
/// not a map at all.
fn field_map(name: &str, value: &Value) -> Result<Option<Vec<(String, Field)>>> {
    let Value::Map(entries) = value else {
        return Ok(None);
    };
    let mut fields = Vec::with_capacity(entries.len());
    for (key, value) in entries.iter() {
        let field = match value {
            Value::Int(number) => Field::Number(*number as f64),
            Value::Float(number) => Field::Number(*number),
            Value::Text(text) => Field::Text(text.to_string()),
            Value::Date(date) => Field::Date(*date),
            Value::LocalTime(time) => Field::LocalTime(*time),
            Value::Time(time) => Field::Time(*time),
            Value::LocalDateTime(datetime) => Field::LocalDateTime(*datetime),
            Value::DateTime(datetime) => Field::DateTime(datetime.clone()),
            other => {
                return Err(Error::type_error(format!(
                    "{name}() map key `{key}` has unsupported type {}",
                    other.type_name()
                )));
            }
        };
        fields.push((key.to_string(), field));
    }
    Ok(Some(fields))
}

/// Construct a date: from a map, a string, a date or another temporal value.
fn call_date(args: &[Value], now: LocalDateTime) -> Result<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::Date(now.date()));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    if let Some(fields) = field_map("date", value)? {
        return construct::date_from_fields(&fields)
            .map(Value::Date)
            .map_err(|message| Error::type_error(format!("date(): {message}")));
    }
    Ok(match value {
        Value::Date(date) => Value::Date(*date),
        Value::DateTime(datetime) => Value::Date(datetime.local().date()),
        Value::LocalDateTime(datetime) => Value::Date(datetime.date()),
        Value::Text(text) => Value::Date(
            text.parse()
                .map_err(|message| Error::type_error(format!("date(): {message}")))?,
        ),
        other => return Err(invalid_argument("date", other)),
    })
}

/// Construct a local time.
fn call_localtime(args: &[Value], now: LocalDateTime) -> Result<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::LocalTime(now.time()));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    if let Some(fields) = field_map("localtime", value)? {
        return construct::localtime_from_fields(&fields)
            .map(Value::LocalTime)
            .map_err(|message| Error::type_error(format!("localtime(): {message}")));
    }
    Ok(match value {
        Value::LocalTime(time) => Value::LocalTime(*time),
        Value::Time(time) => Value::LocalTime(time.local()),
        Value::DateTime(datetime) => Value::LocalTime(datetime.local().time()),
        Value::LocalDateTime(datetime) => Value::LocalTime(datetime.time()),
        Value::Text(text) => Value::LocalTime(
            text.parse()
                .map_err(|message| Error::type_error(format!("localtime(): {message}")))?,
        ),
        other => return Err(invalid_argument("localtime", other)),
    })
}

/// Construct a time with offset.
fn call_time(args: &[Value], now: LocalDateTime) -> Result<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::Time(Time::new(now.time(), 0).expect("zero offset")));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    if let Some(fields) = field_map("time", value)? {
        return construct::time_from_fields(&fields)
            .map(Value::Time)
            .map_err(|message| Error::type_error(format!("time(): {message}")));
    }
    Ok(match value {
        Value::Time(time) => Value::Time(*time),
        Value::LocalTime(time) => Value::Time(Time::new(*time, 0).expect("zero offset")),
        Value::DateTime(datetime) => Value::Time(
            Time::new(datetime.local().time(), datetime.offset()).expect("valid offset"),
        ),
        Value::LocalDateTime(datetime) => {
            Value::Time(Time::new(datetime.time(), 0).expect("zero offset"))
        }
        Value::Text(text) => Value::Time(
            text.parse()
                .map_err(|message| Error::type_error(format!("time(): {message}")))?,
        ),
        other => return Err(invalid_argument("time", other)),
    })
}

/// Construct a zone-less datetime.
fn call_localdatetime(args: &[Value], now: LocalDateTime) -> Result<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::LocalDateTime(now));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    if let Some(fields) = field_map("localdatetime", value)? {
        return construct::localdatetime_from_fields(&fields)
            .map(Value::LocalDateTime)
            .map_err(|message| Error::type_error(format!("localdatetime(): {message}")));
    }
    Ok(match value {
        Value::LocalDateTime(datetime) => Value::LocalDateTime(*datetime),
        Value::DateTime(datetime) => Value::LocalDateTime(datetime.local()),
        Value::Date(date) => Value::LocalDateTime(LocalDateTime::new(
            *date,
            LocalTime::new(0, 0, 0, 0).expect("midnight"),
        )),
        Value::LocalTime(time) => Value::LocalDateTime(LocalDateTime::new(
            Date::new(1970, 1, 1).expect("epoch"),
            *time,
        )),
        Value::Time(time) => Value::LocalDateTime(LocalDateTime::new(
            Date::new(1970, 1, 1).expect("epoch"),
            time.local(),
        )),
        Value::Text(text) => Value::LocalDateTime(
            text.parse()
                .map_err(|message| Error::type_error(format!("localdatetime(): {message}")))?,
        ),
        other => return Err(invalid_argument("localdatetime", other)),
    })
}

/// Construct a datetime with an offset and optional zone.
fn call_datetime(args: &[Value], now: LocalDateTime) -> Result<Value> {
    let Some(value) = args.first() else {
        return Ok(Value::DateTime(
            DateTime::new(now, 0, None).expect("zero offset"),
        ));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    if let Some(fields) = field_map("datetime", value)? {
        return construct::datetime_from_fields(&fields)
            .map(Value::DateTime)
            .map_err(|message| Error::type_error(format!("datetime(): {message}")));
    }
    Ok(match value {
        Value::DateTime(datetime) => Value::DateTime(datetime.clone()),
        Value::LocalDateTime(datetime) => {
            Value::DateTime(DateTime::new(*datetime, 0, None).expect("zero offset"))
        }
        Value::Date(date) => Value::DateTime(
            DateTime::new(
                LocalDateTime::new(*date, LocalTime::new(0, 0, 0, 0).expect("midnight")),
                0,
                None,
            )
            .expect("zero offset"),
        ),
        Value::LocalTime(time) => Value::DateTime(
            DateTime::new(
                LocalDateTime::new(Date::new(1970, 1, 1).expect("epoch"), *time),
                0,
                None,
            )
            .expect("zero offset"),
        ),
        Value::Time(time) => Value::DateTime(
            DateTime::new(
                LocalDateTime::new(Date::new(1970, 1, 1).expect("epoch"), time.local()),
                time.offset(),
                None,
            )
            .expect("valid offset"),
        ),
        Value::Text(text) => Value::DateTime(
            text.parse()
                .map_err(|message| Error::type_error(format!("datetime(): {message}")))?,
        ),
        other => return Err(invalid_argument("datetime", other)),
    })
}

/// Construct a duration.
fn call_duration(args: &[Value]) -> Result<Value> {
    let Some(value) = args.first() else {
        return Err(Error::type_error("duration() requires a map or string"));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    if let Some(fields) = field_map("duration", value)? {
        return construct::duration_from_fields(&fields)
            .map(Value::Duration)
            .map_err(|message| Error::type_error(format!("duration(): {message}")));
    }
    Ok(match value {
        Value::Duration(duration) => Value::Duration(*duration),
        Value::Text(text) => Value::Duration(
            text.parse()
                .map_err(|message| Error::type_error(format!("duration(): {message}")))?,
        ),
        other => return Err(invalid_argument("duration", other)),
    })
}

/// Construct a point from `{x, y, z?, srid?}` or
/// `{longitude, latitude, height?, srid?}`.
fn call_point(args: &[Value]) -> Result<Value> {
    let Some(Value::Map(entries)) = args.first() else {
        match args.first() {
            Some(Value::Null) => return Ok(Value::Null),
            Some(other) => return Err(invalid_argument("point", other)),
            None => return Err(Error::type_error("point() requires a map")),
        }
    };
    let number = |key: &str| -> Option<f64> {
        entries
            .iter()
            .find(|(name, _)| name.as_ref() == key)
            .and_then(|(_, value)| match value {
                Value::Int(number) => Some(*number as f64),
                Value::Float(number) => Some(*number),
                _ => None,
            })
    };
    let srid = entries
        .iter()
        .find(|(name, _)| name.as_ref() == "srid")
        .map(|(_, value)| match value {
            Value::Int(number) => Ok(*number as i32),
            other => Err(Error::type_error(format!(
                "point(): `srid` must be an integer, got {}",
                other.type_name()
            ))),
        })
        .transpose()?;
    let (x, y, z, default_srid) = match (number("x"), number("y")) {
        (Some(x), Some(y)) => {
            let z = number("z");
            let srid = if z.is_some() {
                crate::temporal::point::CARTESIAN_3D
            } else {
                crate::temporal::point::CARTESIAN_2D
            };
            (x, y, z, srid)
        }
        _ => match (number("longitude"), number("latitude")) {
            (Some(x), Some(y)) => {
                let z = number("height");
                let srid = if z.is_some() {
                    crate::temporal::point::WGS84_3D
                } else {
                    crate::temporal::point::WGS84_2D
                };
                (x, y, z, srid)
            }
            _ => {
                return Err(Error::type_error(
                    "point() requires x/y or longitude/latitude",
                ));
            }
        },
    };
    let srid = srid.unwrap_or(default_srid);
    Point::new(srid, x, y, z)
        .map(Value::Point)
        .ok_or_else(|| Error::type_error("point(): invalid srid or coordinates"))
}

/// Which value `*.truncate` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruncateKind {
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    DateTime,
}

/// `*.truncate(unit, value [, map])`: floor a temporal value to a unit and
/// apply component overrides.
fn call_truncate(kind: TruncateKind, args: &[Value]) -> Result<Value> {
    let name = match kind {
        TruncateKind::Date => "date.truncate",
        TruncateKind::LocalTime => "localtime.truncate",
        TruncateKind::Time => "time.truncate",
        TruncateKind::LocalDateTime => "localdatetime.truncate",
        TruncateKind::DateTime => "datetime.truncate",
    };
    let unit_text = match args.first() {
        Some(Value::Text(text)) => text,
        Some(other) => return Err(invalid_argument(name, other)),
        None => return Err(Error::type_error(format!("{name}() requires a unit"))),
    };
    let unit = TruncateUnit::parse(unit_text)
        .ok_or_else(|| Error::type_error(format!("{name}(): unknown unit `{unit_text}`")))?;
    let Some(value) = args.get(1) else {
        return Err(Error::type_error(format!("{name}() requires a value")));
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    let overrides = match args.get(2) {
        Some(map) => field_map(name, map)?.ok_or_else(|| {
            Error::type_error(format!("{name}(): the third argument must be a map"))
        })?,
        None => Vec::new(),
    };
    let midnight = LocalTime::new(0, 0, 0, 0).expect("midnight");
    let epoch = Date::new(1970, 1, 1).expect("epoch");
    let (date, time, offset, zone): (Date, LocalTime, i32, Option<Arc<str>>) = match value {
        Value::Date(date) => (*date, midnight, 0, None),
        Value::LocalTime(time) => (epoch, *time, 0, None),
        Value::Time(time) => (epoch, time.local(), time.offset(), None),
        Value::LocalDateTime(datetime) => (datetime.date(), datetime.time(), 0, None),
        Value::DateTime(datetime) => (
            datetime.local().date(),
            datetime.local().time(),
            datetime.offset(),
            datetime.zone().map(Arc::<str>::from),
        ),
        other => return Err(invalid_argument(name, other)),
    };
    let date = if unit.is_time_unit() {
        date
    } else {
        construct::truncate_date(unit, date, &overrides)
            .map_err(|message| Error::type_error(format!("{name}(): {message}")))?
    };
    let time = construct::truncate_time(unit, time, &overrides)
        .map_err(|message| Error::type_error(format!("{name}(): {message}")))?;
    Ok(match kind {
        TruncateKind::Date => Value::Date(date),
        TruncateKind::LocalTime => Value::LocalTime(time),
        TruncateKind::Time => {
            // A timezone override resolves against the epoch date, since a
            // `TIME` value has no calendar date of its own.
            let offset = match construct::timezone_override(&overrides, epoch, time)
                .map_err(|message| Error::type_error(format!("{name}(): {message}")))?
            {
                Some((offset, _)) => offset,
                None => offset,
            };
            Value::Time(
                Time::new(time, offset).ok_or_else(|| Error::type_error("offset out of range"))?,
            )
        }
        TruncateKind::LocalDateTime => Value::LocalDateTime(LocalDateTime::new(date, time)),
        TruncateKind::DateTime => {
            let (offset, zone) = match construct::timezone_override(&overrides, date, time)
                .map_err(|message| Error::type_error(format!("{name}(): {message}")))?
            {
                Some(pair) => pair,
                None => match &zone {
                    Some(zone) => (
                        construct::zone_offset(zone, date, time)
                            .map_err(|message| Error::type_error(format!("{name}(): {message}")))?,
                        Some(zone.clone()),
                    ),
                    None => (offset, None),
                },
            };
            let local = LocalDateTime::new(date, time);
            Value::DateTime(
                DateTime::new(local, offset, zone)
                    .ok_or_else(|| Error::type_error("offset out of range"))?,
            )
        }
    })
}

/// Which difference function runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurationDiff {
    Between,
    InMonths,
    InDays,
    InSeconds,
}

/// `duration.between`/`inMonths`/`inDays`/`inSeconds`.
fn call_duration_between(kind: DurationDiff, args: &[Value]) -> Result<Value> {
    let name = match kind {
        DurationDiff::Between => "duration.between",
        DurationDiff::InMonths => "duration.inMonths",
        DurationDiff::InDays => "duration.inDays",
        DurationDiff::InSeconds => "duration.inSeconds",
    };
    let (Some(from), Some(to)) = (args.first(), args.get(1)) else {
        return Err(Error::type_error(format!(
            "{name}() requires two arguments"
        )));
    };
    if from.is_null() || to.is_null() {
        return Ok(Value::Null);
    }
    let (Some(from), Some(to)) = (between::instant_of(from), between::instant_of(to)) else {
        let other = if between::instant_of(from).is_none() {
            from
        } else {
            to
        };
        return Err(invalid_argument(name, other));
    };
    let duration = match kind {
        DurationDiff::Between => between::between(&from, &to),
        DurationDiff::InMonths => between::in_months(&from, &to),
        DurationDiff::InDays => between::in_days(&from, &to),
        DurationDiff::InSeconds => between::in_seconds(&from, &to),
    };
    duration
        .map(Value::Duration)
        .ok_or_else(|| Error::type_error(format!("{name}(): out of range")))
}

/// `datetime.fromepoch(seconds, nanos)`: an epoch timestamp in UTC.
fn call_datetime_from_epoch(args: &[Value]) -> Result<Value> {
    match (args.first(), args.get(1)) {
        (Some(Value::Int(seconds)), Some(Value::Int(nanos))) => {
            let local = LocalDateTime::from_epoch(*seconds, *nanos)
                .ok_or_else(|| Error::type_error("datetime.fromepoch(): out of range"))?;
            Ok(Value::DateTime(
                DateTime::new(local, 0, None).expect("zero offset"),
            ))
        }
        (Some(Value::Null), _) | (None, _) | (_, Some(Value::Null)) => Ok(Value::Null),
        (Some(other), _) => Err(invalid_argument("datetime.fromepoch", other)),
    }
}

/// `datetime.fromepochmillis(millis)`: an epoch timestamp in UTC.
fn call_datetime_from_epoch_millis(args: &[Value]) -> Result<Value> {
    match args.first() {
        Some(Value::Int(millis)) => {
            let seconds = millis.div_euclid(1_000);
            let nanos = millis.rem_euclid(1_000) * 1_000_000;
            let local = LocalDateTime::from_epoch(seconds, nanos)
                .ok_or_else(|| Error::type_error("datetime.fromepochmillis(): out of range"))?;
            Ok(Value::DateTime(
                DateTime::new(local, 0, None).expect("zero offset"),
            ))
        }
        Some(Value::Null) | None => Ok(Value::Null),
        Some(other) => Err(invalid_argument("datetime.fromepochmillis", other)),
    }
}

/// `distance(a, b)` between two points.
fn call_distance(args: &[Value]) -> Result<Value> {
    match (args.first(), args.get(1)) {
        (Some(Value::Null), _) | (_, Some(Value::Null)) => Ok(Value::Null),
        (Some(Value::Point(a)), Some(Value::Point(b))) => {
            a.distance(*b).map(Value::Float).map_err(Error::type_error)
        }
        (Some(other), _) => Err(invalid_argument("distance", other)),
        (_, Some(other)) => Err(invalid_argument("distance", other)),
        (None, _) => Err(Error::type_error("distance() requires two points")),
    }
}

/// The current UTC local datetime.
fn system_local_datetime() -> LocalDateTime {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    LocalDateTime::new(
        Date::from_days((now.as_secs() / 86_400) as i64),
        LocalTime::from_nanos(
            (now.subsec_nanos() as i64) + (now.as_secs() % 86_400) as i64 * 1_000_000_000,
        )
        .expect("valid time of day"),
    )
}

/// Largest list `range()` will materialize; guards against runaway queries
/// like `range(0, 9223372036854775807)`.
const MAX_RANGE_ELEMENTS: i64 = 10_000_000;

/// `range(start, end [, step])`: an inclusive arithmetic sequence of
/// integers. A step of zero raises a range error, non-integer arguments a
/// type error, and an inconsistent direction yields the empty list.
fn call_range(args: &[Value]) -> Result<Value> {
    let mut numbers = [0_i64; 3];
    let mut count = 0;
    for (index, number) in numbers.iter_mut().enumerate() {
        match args.get(index) {
            Some(Value::Int(value)) => {
                *number = *value;
                count += 1;
            }
            Some(Value::Null) => return Ok(Value::Null),
            // A missing step defaults to 1; a missing end cannot form one.
            None if index == 2 => break,
            None => return Ok(Value::Null),
            // The TCK classifies non-integer bounds as an argument error,
            // not a type error.
            Some(other) => {
                return Err(Error::new(
                    crate::error::ErrorCode::Misuse,
                    format!("range() does not accept {} values", other.type_name()),
                ));
            }
        }
    }
    if count < 2 {
        return Ok(Value::Null);
    }
    let [start, end, step] = if count == 2 {
        [numbers[0], numbers[1], 1]
    } else {
        numbers
    };
    if step == 0 {
        return Err(Error::new(
            crate::error::ErrorCode::Range,
            "range() step must not be zero",
        ));
    }
    let mut values = Vec::new();
    let mut value = start;
    while if step > 0 { value <= end } else { value >= end } {
        if values.len() as i64 >= MAX_RANGE_ELEMENTS {
            return Err(Error::new(
                crate::error::ErrorCode::Range,
                format!("range() produces more than {MAX_RANGE_ELEMENTS} elements"),
            ));
        }
        values.push(Value::Int(value));
        match value.checked_add(step) {
            Some(next) => value = next,
            None => break,
        }
    }
    Ok(Value::List(values.into()))
}

/// `split(original, delimiter)`: an empty delimiter splits into characters.
fn call_split(args: &[Value]) -> Result<Value> {
    let original = match args.first() {
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(Value::Text(text)) => text,
        Some(other) => return Err(invalid_argument("split", other)),
    };
    let delimiter = match args.get(1) {
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(Value::Text(text)) => text,
        Some(other) => return Err(invalid_argument("split", other)),
    };
    let parts: Vec<Value> = if delimiter.is_empty() {
        original
            .chars()
            .map(|c| Value::Text(c.to_string().into()))
            .collect()
    } else {
        original
            .split(delimiter.as_ref())
            .map(|part| Value::Text(part.into()))
            .collect()
    };
    Ok(Value::List(parts.into()))
}

/// `substring(original, start [, length])`, counted in characters. Negative
/// start or length raise a type error; a start past the end yields the empty
/// string.
fn call_substring(args: &[Value]) -> Result<Value> {
    let text = match args.first() {
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(Value::Text(text)) => text,
        Some(other) => return Err(invalid_argument("substring", other)),
    };
    let start = match args.get(1) {
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Int(start)) if *start >= 0 => *start as usize,
        Some(Value::Int(_)) => {
            return Err(Error::type_error("substring() start must not be negative"));
        }
        Some(other) => return Err(invalid_argument("substring", other)),
        None => 0,
    };
    let length = match args.get(2) {
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Int(length)) if *length >= 0 => Some(*length as usize),
        Some(Value::Int(_)) => {
            return Err(Error::type_error("substring() length must not be negative"));
        }
        Some(other) => return Err(invalid_argument("substring", other)),
        None => None,
    };
    let chars: Vec<char> = text.chars().collect();
    let start = start.min(chars.len());
    let end = match length {
        Some(length) => start.saturating_add(length).min(chars.len()),
        None => chars.len(),
    };
    let slice: String = chars[start..end].iter().collect();
    Ok(Value::Text(slice.into()))
}

/// `left(original, length)` and `right(original, length)`, counted in
/// characters; lengths past the end clamp.
fn call_left_right(func: u8, args: &[Value]) -> Result<Value> {
    let name = if func == func::LEFT { "left" } else { "right" };
    let text = match args.first() {
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(Value::Text(text)) => text,
        Some(other) => return Err(invalid_argument(name, other)),
    };
    let length = match args.get(1) {
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Int(length)) if *length >= 0 => *length as usize,
        Some(Value::Int(_)) => {
            return Err(Error::type_error(format!(
                "{name}() length must not be negative"
            )));
        }
        Some(other) => return Err(invalid_argument(name, other)),
        None => 0,
    };
    let chars = text.chars();
    let slice: String = if func == func::LEFT {
        chars.take(length).collect()
    } else {
        let total = text.chars().count();
        chars.skip(total.saturating_sub(length)).collect()
    };
    Ok(Value::Text(slice.into()))
}

/// `replace(original, search, replacement)`.
fn call_replace(args: &[Value]) -> Result<Value> {
    let mut texts = [""; 3];
    for (slot, index) in texts.iter_mut().zip(0..3) {
        match args.get(index) {
            Some(Value::Null) | None => return Ok(Value::Null),
            Some(Value::Text(text)) => *slot = text,
            Some(other) => return Err(invalid_argument("replace", other)),
        }
    }
    let [original, search, replacement] = texts;
    Ok(Value::Text(original.replace(search, replacement).into()))
}

/// Interpret a value as a number for a math function.
fn number_argument(name: &str, value: &Value) -> Result<f64> {
    match value {
        Value::Int(number) => Ok(*number as f64),
        Value::Float(number) => Ok(*number),
        other => Err(invalid_argument(name, other)),
    }
}

/// Apply a float transformation: null propagates and integers are widened.
fn float_map(args: &[Value], name: &str, f: impl FnOnce(f64) -> f64) -> Result<Value> {
    match args.first() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(value) => Ok(Value::Float(f(number_argument(name, value)?))),
    }
}

/// Apply a string transformation, propagating nulls and rejecting other
/// types.
fn text_map(args: &[Value], name: &str, f: impl FnOnce(&str) -> String) -> Result<Value> {
    match args.first() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(Value::Text(text)) => Ok(Value::Text(f(text).into())),
        Some(other) => Err(invalid_argument(name, other)),
    }
}

/// Invalid argument for a scalar function, reported as a runtime `TypeError`
/// like openCypher's `InvalidArgumentValue`.
fn invalid_argument(name: &str, value: &Value) -> Error {
    Error::type_error(format!(
        "{name}() does not accept {} values",
        value.type_name()
    ))
}

/// Truncate a float toward zero like `toInteger`; NaN and infinities have no
/// integer value and yield `None`, as does anything outside the `i64` range.
fn truncate_to_int(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let truncated = value.trunc();
    // `i64::MAX as f64` rounds up to 2^63, so the upper bound is exclusive.
    if truncated >= i64::MIN as f64 && truncated < i64::MAX as f64 {
        Some(truncated as i64)
    } else {
        None
    }
}

/// Parse text as an integer, accepting the float notation the TCK requires
/// (`toInteger('2.9')` is 2); unparsable text yields `None`.
fn text_to_integer(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(value) = text.parse::<i64>() {
        return Some(value);
    }
    truncate_to_int(text.parse::<f64>().ok()?)
}

pub fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => {
            let mut buffer = ryu::Buffer::new();
            buffer.format(*value).to_owned()
        }
        Value::Text(value) => value.to_string(),
        Value::Bytes(value) => format!("<{} bytes>", value.len()),
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{key}: {}", format_value(value)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Node(id) => format!("(node {id})"),
        Value::Edge(id) => format!("[edge {id}]"),
        Value::Date(value) => value.to_string(),
        Value::LocalTime(value) => value.to_string(),
        Value::Time(value) => value.to_string(),
        Value::LocalDateTime(value) => value.to_string(),
        Value::DateTime(value) => value.to_string(),
        Value::Duration(value) => value.to_string(),
        Value::Point(value) => value.to_string(),
    }
}
