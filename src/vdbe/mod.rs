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

use crate::cypher::UnaryOp;
use crate::engine::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::matrix::Direction;
use crate::planner::plan::AggFn;
use crate::record::{Value, value_equals};

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
        /// `(register, splice)` elements in path order.
        elements: Vec<(u32, bool)>,
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
        /// Destination register for the end node.
        node: u32,
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
        items: Vec<(Vec<u64>, u64)>,
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
                node,
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
                    Some((edge_ids, end)) => {
                        let values: Vec<Value> =
                            edge_ids.iter().map(|id| Value::Edge(*id)).collect();
                        self.regs[*edges as usize] = Reg::Value(Value::List(values.into()));
                        self.regs[*node as usize] = Reg::Node(end);
                    }
                    None => self.pc = *target as usize,
                }
            }
            Op::BuildPath { dest, elements } => {
                let mut items: Vec<Value> = Vec::new();
                for (reg, splice) in elements {
                    let value = self.reg_value(*reg)?;
                    match (splice, value) {
                        (true, Value::List(inner)) => items.extend(inner.iter().cloned()),
                        (_, value) => items.push(value),
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
            for (idx, spec) in aggs.iter().enumerate() {
                let input = &row[prefix + group_count + idx];
                accumulators[idx].step(spec, input);
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

    fn step(&mut self, spec: &AggSpec, input: &Reg) {
        if spec.distinct {
            let value = input.to_value();
            if value.is_null() || !self.seen.insert(value) {
                return;
            }
        }
        self.accumulator.step(spec, input);
    }

    fn finish(self, spec: &AggSpec) -> Value {
        self.accumulator.finish(spec)
    }
}

/// Aggregation accumulators.
enum Accumulator {
    Count(i64),
    Sum(SumState),
    Avg { sum: f64, count: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect(Vec<Value>),
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
        }
    }

    fn step(&mut self, spec: &AggSpec, input: &Reg) {
        let value = input.to_value();
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
        }
    }
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
        code::ADD => numeric_or_concat(a, b, |x, y| x + y, |x, y| x.wrapping_add(y)),
        code::SUB => numeric(a, b, |x, y| x - y, |x, y| x.wrapping_sub(y)),
        code::MUL => numeric(a, b, |x, y| x * y, |x, y| x.wrapping_mul(y)),
        code::DIV => match (a, b) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::Int(_), Value::Int(0)) => Ok(Value::Null),
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x.wrapping_div(*y))),
            _ => match (a.as_number(), b.as_number()) {
                (Some(x), Some(y)) => Ok(Value::Float(x / y)),
                _ => Err(type_error("division", a, b)),
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

fn compare_value(op: u8, a: &Value, b: &Value) -> Value {
    use binary_code as code;
    let ordering = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Value::Null,
        (Value::Int(_), Value::Int(_))
        | (Value::Float(_), Value::Float(_))
        | (Value::Int(_), Value::Float(_))
        | (Value::Float(_), Value::Int(_)) => {
            let (Some(x), Some(y)) = (a.as_number(), b.as_number()) else {
                return Value::Null;
            };
            x.total_cmp(&y)
        }
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
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
    }
}
