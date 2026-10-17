//! Code generator: logical [`Plan`] to a VDBE [`Program`].
//!
//! Compilation is continuation-passing: `compile(plan, ctx, out)` emits code
//! that produces rows, invoking `out` for every row. Loops (scans and
//! expansions) are emitted around the continuation, so pattern matching
//! becomes nested cursor loops; pipelines with `Sort`, `Aggregate`,
//! `Distinct` and `Skip`/`Limit` collect rows into row cursors and iterate
//! the processed result.

use hashbrown::HashMap;

use crate::cypher::{Expr, QuantifierKind, SetItem};
use crate::error::{Error, Result};
use crate::planner::Prepared;
use crate::planner::plan::{CreateItem, Plan};
use crate::record::Value;
use crate::vdbe::{AggSpec, Op, Program, binary_code, func, unary_code};

/// Register/variable context for the row currently being compiled.
#[derive(Clone)]
struct Ctx {
    vars: HashMap<String, u32>,
    window_start: u32,
    window_len: u32,
}

/// Code generator state.
struct Codegen {
    ops: Vec<Op>,
    constants: Vec<Value>,
    next_reg: u32,
    next_cursor: u32,
}

/// Compile a prepared query into a VDBE program.
pub fn compile(prepared: &Prepared) -> Result<Program> {
    let mut cg = Codegen {
        ops: Vec::new(),
        constants: Vec::new(),
        next_reg: 0,
        next_cursor: 0,
    };
    let mut ctx = Ctx {
        vars: HashMap::new(),
        window_start: 0,
        window_len: 0,
    };
    let columns = prepared.columns.clone();
    cg.compile(&prepared.plan, &mut ctx, &mut |cg, ctx| {
        if !columns.is_empty() {
            cg.emit(Op::ResultRow {
                start: ctx.window_start,
                count: ctx.window_len,
            });
        }
        Ok(())
    })?;
    cg.emit(Op::Halt);
    Ok(Program {
        ops: cg.ops,
        constants: cg.constants,
        cursor_count: cg.next_cursor as usize,
        register_count: cg.next_reg as usize,
        columns: prepared.columns.clone(),
        mutating: prepared.mutating,
    })
}

impl Codegen {
    fn emit(&mut self, op: Op) -> u32 {
        self.ops.push(op);
        (self.ops.len() - 1) as u32
    }

    fn patch(&mut self, at: u32, target: u32) -> Result<()> {
        match &mut self.ops[at as usize] {
            Op::Rewind { target: t, .. }
            | Op::Next { target: t, .. }
            | Op::IfNot { target: t, .. }
            | Op::Goto { target: t }
            | Op::CtxRewind { target: t, .. }
            | Op::CtxNext { target: t, .. }
            | Op::UnwindNext { target: t, .. }
            | Op::VarExpandNext { target: t, .. } => *t = target,
            other => {
                return Err(Error::internal(format!(
                    "attempted to patch a non-jump instruction: {other:?}"
                )));
            }
        }
        Ok(())
    }

    fn alloc_reg(&mut self) -> u32 {
        let reg = self.next_reg;
        self.next_reg += 1;
        reg
    }

    fn alloc_cursor(&mut self) -> u32 {
        let cursor = self.next_cursor;
        self.next_cursor += 1;
        cursor
    }

    fn const_index(&mut self, value: Value) -> u32 {
        let index = self.constants.len() as u32;
        self.constants.push(value);
        index
    }

    fn const_reg(&mut self, value: Value) -> u32 {
        let index = self.constants.len() as u32;
        self.constants.push(value);
        let reg = self.alloc_reg();
        self.emit(Op::Constant {
            dest: reg,
            value: index,
        });
        reg
    }

    fn null_reg(&mut self) -> u32 {
        let reg = self.alloc_reg();
        self.emit(Op::Null { dest: reg });
        reg
    }

    #[allow(clippy::type_complexity)]
    fn compile(
        &mut self,
        plan: &Plan,
        ctx: &mut Ctx,
        out: &mut dyn FnMut(&mut Codegen, &mut Ctx) -> Result<()>,
    ) -> Result<()> {
        match plan {
            Plan::Empty => out(self, ctx),
            Plan::EmptyResult => Ok(()),
            Plan::NodeScan {
                input,
                var,
                label,
                seek,
                props,
            } => {
                self.compile(input, ctx, &mut |cg, ctx| {
                    let cursor = cg.alloc_cursor();
                    let rewind;
                    if let Some(seek) = seek {
                        let value_reg = cg.const_reg(seek.value.clone());
                        cg.emit(Op::OpenIndexScan {
                            cursor,
                            index: seek.index_id,
                            value: value_reg,
                        });
                        rewind = cg.emit(Op::Rewind { cursor, target: 0 });
                    } else {
                        let label_reg = label
                            .as_ref()
                            .map(|name| cg.const_index(Value::from(name.as_str())));
                        cg.emit(Op::OpenNodeScan {
                            cursor,
                            label: label_reg,
                        });
                        rewind = cg.emit(Op::Rewind { cursor, target: 0 });
                    }
                    let loop_start = cg.ops.len() as u32;
                    let dest = cg.alloc_reg();
                    cg.emit(Op::Bind { cursor, dest });
                    ctx.vars.insert(var.clone(), dest);
                    ctx.window_len = cg.next_reg - ctx.window_start;
                    let mut skip_jumps = Vec::new();
                    // Index scans must re-check the label.
                    if seek.is_some()
                        && let Some(label) = label
                    {
                        let check = cg.alloc_reg();
                        let label_reg = cg.const_index(Value::from(label.as_str()));
                        cg.emit(Op::HasLabel {
                            node: dest,
                            label: label_reg,
                            dest: check,
                        });
                        skip_jumps.push(cg.emit(Op::IfNot {
                            reg: check,
                            target: 0,
                        }));
                    }
                    cg.emit_property_filters(dest, props, &mut skip_jumps)?;
                    out(cg, ctx)?;
                    let skip_target = cg.ops.len() as u32;
                    for jump in skip_jumps {
                        cg.patch(jump, skip_target)?;
                    }
                    let next = cg.emit(Op::Next { cursor, target: 0 });
                    cg.emit(Op::Goto { target: loop_start });
                    let end = cg.ops.len() as u32;
                    cg.patch(rewind, end)?;
                    cg.patch(next, end)?;
                    Ok(())
                })
            }
            Plan::AllNodesScan { input, var } => {
                let rewritten = Plan::NodeScan {
                    input: input.clone(),
                    var: var.clone(),
                    label: None,
                    seek: None,
                    props: Vec::new(),
                };
                self.compile(&rewritten, ctx, out)
            }
            Plan::EdgeScan {
                input,
                var,
                edge_type,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                let cursor = cg.alloc_cursor();
                cg.emit(Op::OpenEdgeScan {
                    cursor,
                    edge_type: *edge_type,
                });
                let rewind = cg.emit(Op::Rewind { cursor, target: 0 });
                let loop_start = cg.ops.len() as u32;
                let dest = cg.alloc_reg();
                cg.emit(Op::Bind { cursor, dest });
                ctx.vars.insert(var.clone(), dest);
                ctx.window_len = cg.next_reg - ctx.window_start;
                out(cg, ctx)?;
                let next = cg.emit(Op::Next { cursor, target: 0 });
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(rewind, end)?;
                cg.patch(next, end)?;
                Ok(())
            }),
            Plan::VarExpand {
                input,
                src,
                edge,
                dst,
                direction,
                types,
                props,
                min,
                max,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                let src_reg = *ctx.vars.get(src).ok_or_else(|| undefined(src))?;
                let cursor = cg.alloc_cursor();
                let direction_code = match direction {
                    crate::cypher::RelDirection::Outgoing => 0u8,
                    crate::cypher::RelDirection::Incoming => 1u8,
                    crate::cypher::RelDirection::Undirected => 2u8,
                };
                let type_regs: Vec<u32> = types
                    .iter()
                    .map(|name| cg.const_index(Value::from(name.as_str())))
                    .collect();
                cg.emit(Op::VarExpandOpen {
                    cursor,
                    src: src_reg,
                    direction: direction_code,
                    types: type_regs,
                    any_type: types.is_empty(),
                    props: props.clone(),
                    min: *min,
                    max: *max,
                });
                let loop_start = cg.ops.len() as u32;
                let edge_reg = cg.alloc_reg();
                let node_reg = cg.alloc_reg();
                let jump = cg.emit(Op::VarExpandNext {
                    cursor,
                    edges: edge_reg,
                    node: node_reg,
                    target: 0,
                });
                if let Some(edge_var) = edge {
                    ctx.vars.insert(edge_var.clone(), edge_reg);
                }
                ctx.vars.insert(dst.clone(), node_reg);
                ctx.window_len = cg.next_reg - ctx.window_start;
                out(cg, ctx)?;
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(jump, end)?;
                Ok(())
            }),
            Plan::Expand {
                input,
                src,
                edge,
                dst,
                direction,
                types,
                props,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                let src_reg = *ctx.vars.get(src).ok_or_else(|| undefined(src))?;
                let cursor = cg.alloc_cursor();
                let direction_code = match direction {
                    crate::cypher::RelDirection::Outgoing => 0u8,
                    crate::cypher::RelDirection::Incoming => 1u8,
                    crate::cypher::RelDirection::Undirected => 2u8,
                };
                let type_regs: Vec<u32> = types
                    .iter()
                    .map(|name| cg.const_index(Value::from(name.as_str())))
                    .collect();
                cg.emit(Op::OpenExpand {
                    cursor,
                    src: src_reg,
                    direction: direction_code,
                    types: type_regs,
                    any_type: types.is_empty(),
                });
                let rewind = cg.emit(Op::Rewind { cursor, target: 0 });
                let loop_start = cg.ops.len() as u32;
                let edge_reg = cg.alloc_reg();
                let node_reg = cg.alloc_reg();
                cg.emit(Op::BindExpand {
                    cursor,
                    edge: edge_reg,
                    node: node_reg,
                });
                if let Some(edge_var) = edge {
                    ctx.vars.insert(edge_var.clone(), edge_reg);
                }
                ctx.vars.insert(dst.clone(), node_reg);
                ctx.window_len = cg.next_reg - ctx.window_start;
                let mut skip_jumps = Vec::new();
                cg.emit_property_filters(edge_reg, props, &mut skip_jumps)?;
                out(cg, ctx)?;
                let skip_target = cg.ops.len() as u32;
                for jump in skip_jumps {
                    cg.patch(jump, skip_target)?;
                }
                let next = cg.emit(Op::Next { cursor, target: 0 });
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(rewind, end)?;
                cg.patch(next, end)?;
                Ok(())
            }),
            Plan::Union { parts, all: _ } => {
                // Each part produces the same output columns; concatenating
                // their rows is just compiling them in sequence.
                for part in parts {
                    self.compile(part, ctx, out)?;
                }
                Ok(())
            }
            Plan::Filter { input, predicate } => self.compile(input, ctx, &mut |cg, ctx| {
                let condition = cg.compile_expr(predicate, ctx)?;
                let jump = cg.emit(Op::IfNot {
                    reg: condition,
                    target: 0,
                });
                out(cg, ctx)?;
                let end = cg.ops.len() as u32;
                cg.patch(jump, end)?;
                Ok(())
            }),
            Plan::BindPath {
                input,
                var,
                elements,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                let mut regs = Vec::with_capacity(elements.len());
                for (name, splice) in elements {
                    let reg = *ctx.vars.get(name).ok_or_else(|| undefined(name))?;
                    regs.push((reg, *splice));
                }
                let dest = cg.alloc_reg();
                cg.emit(Op::BuildPath {
                    dest,
                    elements: regs,
                });
                ctx.vars.insert(var.clone(), dest);
                ctx.window_len = cg.next_reg - ctx.window_start;
                out(cg, ctx)
            }),
            Plan::Foreach {
                input,
                var,
                list,
                plan,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                let list_reg = cg.compile_expr(list, ctx)?;
                let cursor = cg.alloc_cursor();
                cg.emit(Op::UnwindInit {
                    cursor,
                    list: list_reg,
                });
                let loop_start = cg.ops.len() as u32;
                let element = cg.alloc_reg();
                let jump = cg.emit(Op::UnwindNext {
                    cursor,
                    dest: element,
                    target: 0,
                });
                let mut inner_ctx = ctx.clone();
                inner_ctx.vars.insert(var.clone(), element);
                // The body's rows are discarded; the input row passes through.
                cg.compile(plan, &mut inner_ctx, &mut |_cg, _ctx| Ok(()))?;
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(jump, end)?;
                out(cg, ctx)
            }),
            Plan::ProcedureCall {
                input,
                procedure,
                columns,
                yields,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                let cursor = cg.alloc_cursor();
                let name = cg.const_index(Value::from(procedure.as_str()));
                cg.emit(Op::ProcedureCallOpen { cursor, name });
                let base = cg.next_reg;
                for _ in columns {
                    cg.alloc_reg();
                }
                let mut inner_ctx = ctx.clone();
                for (column, alias) in yields {
                    let Some(index) = columns.iter().position(|name| name == column) else {
                        return Err(Error::internal("procedure yield column is missing"));
                    };
                    inner_ctx.vars.insert(alias.clone(), base + index as u32);
                }
                inner_ctx.window_start = base;
                inner_ctx.window_len = columns.len() as u32;
                let empty = cg.emit(Op::CtxRewind { cursor, target: 0 });
                let loop_start = cg.ops.len() as u32;
                cg.emit(Op::CtxData {
                    cursor,
                    start: base,
                    count: columns.len() as u32,
                });
                out(cg, &mut inner_ctx)?;
                let next = cg.emit(Op::CtxNext { cursor, target: 0 });
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(empty, end)?;
                cg.patch(next, end)?;
                Ok(())
            }),
            Plan::ShortestPath {
                input,
                src,
                dst,
                edge,
                direction,
                types,
                props,
                min,
                max,
                all,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                let src_reg = *ctx.vars.get(src).ok_or_else(|| undefined(src))?;
                // An unbound target searches for the shortest path to every
                // reachable node; the search binds the variable below.
                let dst_reg = ctx.vars.get(dst).copied();
                let cursor = cg.alloc_cursor();
                let direction_code = match direction {
                    crate::cypher::RelDirection::Outgoing => 0u8,
                    crate::cypher::RelDirection::Incoming => 1u8,
                    crate::cypher::RelDirection::Undirected => 2u8,
                };
                let type_regs: Vec<u32> = types
                    .iter()
                    .map(|name| cg.const_index(Value::from(name.as_str())))
                    .collect();
                cg.emit(Op::ShortestOpen {
                    cursor,
                    src: src_reg,
                    dst: dst_reg,
                    direction: direction_code,
                    types: type_regs,
                    any_type: types.is_empty(),
                    props: props.clone(),
                    min: *min,
                    max: *max,
                    all: *all,
                });
                let loop_start = cg.ops.len() as u32;
                let edge_reg = cg.alloc_reg();
                let node_reg = cg.alloc_reg();
                let jump = cg.emit(Op::VarExpandNext {
                    cursor,
                    edges: edge_reg,
                    node: node_reg,
                    target: 0,
                });
                if let Some(edge_var) = edge {
                    ctx.vars.insert(edge_var.clone(), edge_reg);
                }
                // A bound target keeps its register; only an unbound target
                // is bound by the search.
                if dst_reg.is_none() {
                    ctx.vars.insert(dst.clone(), node_reg);
                }
                ctx.window_len = cg.next_reg - ctx.window_start;
                out(cg, ctx)?;
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(jump, end)?;
                Ok(())
            }),
            Plan::PatternPredicate { input, inner, var } => {
                self.compile(input, ctx, &mut |cg, outer_ctx| {
                    let cursor = cg.alloc_cursor();
                    cg.emit(Op::CollectOpen { cursor });
                    cg.compile(inner, outer_ctx, &mut |cg, inner_ctx| {
                        cg.emit(Op::CollectRow {
                            cursor,
                            start: inner_ctx.window_start,
                            count: inner_ctx.window_len,
                        });
                        Ok(())
                    })?;
                    let dest = cg.alloc_reg();
                    cg.emit(Op::CtxHasRows { cursor, dest });
                    let mut out_ctx = outer_ctx.clone();
                    out_ctx.vars.insert(var.clone(), dest);
                    out(cg, &mut out_ctx)
                })
            }
            Plan::PatternComprehension {
                input,
                inner,
                project,
                var,
            } => self.compile(input, ctx, &mut |cg, outer_ctx| {
                let cursor = cg.alloc_cursor();
                cg.emit(Op::CollectOpen { cursor });
                let mut slot = 0;
                cg.compile(inner, outer_ctx, &mut |cg, inner_ctx| {
                    let value = cg.compile_expr(project, inner_ctx)?;
                    slot = cg.alloc_reg();
                    cg.emit(Op::Copy {
                        dest: slot,
                        src: value,
                    });
                    cg.emit(Op::CollectRow {
                        cursor,
                        start: slot,
                        count: 1,
                    });
                    Ok(())
                })?;
                let dest = cg.alloc_reg();
                cg.emit(Op::CtxToList { cursor, dest });
                let mut out_ctx = outer_ctx.clone();
                out_ctx.vars.insert(var.clone(), dest);
                out(cg, &mut out_ctx)
            }),
            Plan::OptionalMatch { input, inner, vars } => {
                self.compile(input, ctx, &mut |cg, outer_ctx| {
                    let cursor = cg.alloc_cursor();
                    cg.emit(Op::CollectOpen { cursor });
                    let mut produced: Option<Ctx> = None;
                    cg.compile(inner, outer_ctx, &mut |cg, ctx| {
                        produced = Some(ctx.clone());
                        cg.emit(Op::CollectRow {
                            cursor,
                            start: ctx.window_start,
                            count: ctx.window_len,
                        });
                        Ok(())
                    })?;
                    let Some(inner_ctx) = produced else {
                        // The inner plan can never produce rows: only the
                        // null-extended path exists.
                        let mut null_ctx = outer_ctx.clone();
                        for var in vars {
                            let reg = cg.null_reg();
                            null_ctx.vars.insert(var.clone(), reg);
                        }
                        null_ctx.window_len = cg.next_reg - null_ctx.window_start;
                        return out(cg, &mut null_ctx);
                    };
                    // When the pattern missed, append one row with the new
                    // variables nulled so the replay below sees a row.
                    let empty_jump = cg.emit(Op::CtxRewind { cursor, target: 0 });
                    let skip_null = cg.emit(Op::Goto { target: 0 });
                    let null_start = cg.ops.len() as u32;
                    for var in vars {
                        let Some(reg) = inner_ctx.vars.get(var).copied() else {
                            return Err(Error::internal("optional variable is not bound"));
                        };
                        cg.emit(Op::Null { dest: reg });
                    }
                    cg.emit(Op::CollectRow {
                        cursor,
                        start: inner_ctx.window_start,
                        count: inner_ctx.window_len,
                    });
                    let replay_start = cg.ops.len() as u32;
                    cg.patch(empty_jump, null_start)?;
                    cg.patch(skip_null, replay_start)?;
                    // Replay the collected (or null-extended) rows once.
                    let loop_start = cg.ops.len() as u32;
                    cg.emit(Op::CtxData {
                        cursor,
                        start: inner_ctx.window_start,
                        count: inner_ctx.window_len,
                    });
                    let mut replay_ctx = inner_ctx.clone();
                    out(cg, &mut replay_ctx)?;
                    let next = cg.emit(Op::CtxNext { cursor, target: 0 });
                    cg.emit(Op::Goto { target: loop_start });
                    let end = cg.ops.len() as u32;
                    cg.patch(next, end)?;
                    Ok(())
                })
            }
            Plan::Project { input, items } => {
                self.compile(input, ctx, &mut |cg, in_ctx| {
                    // Evaluate into scratch registers first so the output
                    // window is contiguous.
                    let mut values = Vec::with_capacity(items.len());
                    for (expr, _) in items {
                        values.push(cg.compile_expr(expr, in_ctx)?);
                    }
                    let base = cg.next_reg;
                    let mut new_ctx = Ctx {
                        vars: HashMap::new(),
                        window_start: base,
                        window_len: items.len() as u32,
                    };
                    for ((_, name), value) in items.iter().zip(values) {
                        let slot = cg.alloc_reg();
                        cg.emit(Op::Copy {
                            dest: slot,
                            src: value,
                        });
                        new_ctx.vars.insert(name.clone(), slot);
                    }
                    out(cg, &mut new_ctx)
                })
            }
            Plan::Aggregate {
                input,
                groups,
                aggs,
            } => {
                let cursor = self.alloc_cursor();
                self.emit(Op::CollectOpen { cursor });
                let mut layout: Option<(u32, u32, u32)> = None;
                self.compile(input, ctx, &mut |cg, in_ctx| {
                    let mut group_values = Vec::with_capacity(groups.len());
                    for (expr, _) in groups {
                        group_values.push(cg.compile_expr(expr, in_ctx)?);
                    }
                    let mut agg_values = Vec::with_capacity(aggs.len());
                    for item in aggs {
                        agg_values.push(cg.compile_expr(&item.argument, in_ctx)?);
                    }
                    let prefix = cg.next_reg - in_ctx.window_start;
                    let group_base = cg.next_reg;
                    for value in group_values {
                        let slot = cg.alloc_reg();
                        cg.emit(Op::Copy {
                            dest: slot,
                            src: value,
                        });
                    }
                    for value in agg_values {
                        let slot = cg.alloc_reg();
                        cg.emit(Op::Copy {
                            dest: slot,
                            src: value,
                        });
                    }
                    layout = Some((in_ctx.window_start, prefix, group_base));
                    cg.emit(Op::CollectRow {
                        cursor,
                        start: in_ctx.window_start,
                        count: prefix + (groups.len() + aggs.len()) as u32,
                    });
                    Ok(())
                })?;
                let (_, prefix, group_base) =
                    layout.ok_or_else(|| Error::internal("empty aggregate input"))?;
                let specs: Vec<AggSpec> = aggs
                    .iter()
                    .map(|item| AggSpec {
                        func: item.func,
                        star: false,
                        distinct: item.distinct,
                    })
                    .collect();
                self.emit(Op::HashAggregate {
                    cursor,
                    prefix,
                    groups: groups.len() as u32,
                    aggs: specs,
                });
                let rewind = self.emit(Op::CtxRewind { cursor, target: 0 });
                let loop_start = self.ops.len() as u32;
                let output_count = (groups.len() + aggs.len()) as u32;
                self.emit(Op::CtxData {
                    cursor,
                    start: group_base,
                    count: output_count,
                });
                let mut new_ctx = Ctx {
                    vars: HashMap::new(),
                    window_start: group_base,
                    window_len: output_count,
                };
                for (idx, (_, name)) in groups.iter().enumerate() {
                    new_ctx.vars.insert(name.clone(), group_base + idx as u32);
                }
                for (idx, item) in aggs.iter().enumerate() {
                    new_ctx.vars.insert(
                        item.name.clone(),
                        group_base + groups.len() as u32 + idx as u32,
                    );
                }
                out(self, &mut new_ctx)?;
                let next = self.emit(Op::CtxNext { cursor, target: 0 });
                self.emit(Op::Goto { target: loop_start });
                let end = self.ops.len() as u32;
                self.patch(rewind, end)?;
                self.patch(next, end)?;
                Ok(())
            }
            Plan::Distinct { input } => {
                let cursor = self.alloc_cursor();
                self.emit(Op::CollectOpen { cursor });
                let mut produced: Option<Ctx> = None;
                self.compile(input, ctx, &mut |cg, in_ctx| {
                    produced = Some(in_ctx.clone());
                    cg.emit(Op::CollectRow {
                        cursor,
                        start: in_ctx.window_start,
                        count: in_ctx.window_len,
                    });
                    Ok(())
                })?;
                let mut row_ctx =
                    produced.ok_or_else(|| Error::internal("empty distinct input"))?;
                self.emit(Op::DedupRows {
                    cursor,
                    prefix: 0,
                    keys: row_ctx.window_len,
                });
                let rewind = self.emit(Op::CtxRewind { cursor, target: 0 });
                let loop_start = self.ops.len() as u32;
                self.emit(Op::CtxData {
                    cursor,
                    start: row_ctx.window_start,
                    count: row_ctx.window_len,
                });
                out(self, &mut row_ctx)?;
                let next = self.emit(Op::CtxNext { cursor, target: 0 });
                self.emit(Op::Goto { target: loop_start });
                let end = self.ops.len() as u32;
                self.patch(rewind, end)?;
                self.patch(next, end)?;
                Ok(())
            }
            Plan::Sort { input, keys } => {
                let cursor = self.alloc_cursor();
                self.emit(Op::CollectOpen { cursor });
                let mut produced: Option<(Ctx, u32)> = None;
                self.compile(input, ctx, &mut |cg, in_ctx| {
                    let mut key_values = Vec::with_capacity(keys.len());
                    for (expr, _) in keys {
                        key_values.push(cg.compile_expr(expr, in_ctx)?);
                    }
                    let prefix = cg.next_reg - in_ctx.window_start;
                    for value in key_values {
                        let slot = cg.alloc_reg();
                        cg.emit(Op::Copy {
                            dest: slot,
                            src: value,
                        });
                    }
                    produced = Some((in_ctx.clone(), prefix));
                    cg.emit(Op::CollectRow {
                        cursor,
                        start: in_ctx.window_start,
                        count: prefix + keys.len() as u32,
                    });
                    Ok(())
                })?;
                let (mut row_ctx, prefix) =
                    produced.ok_or_else(|| Error::internal("empty sort input"))?;
                self.emit(Op::SortRows {
                    cursor,
                    prefix,
                    keys: keys.len() as u32,
                    ascending: keys.iter().map(|(_, ascending)| *ascending).collect(),
                });
                let rewind = self.emit(Op::CtxRewind { cursor, target: 0 });
                let loop_start = self.ops.len() as u32;
                self.emit(Op::CtxData {
                    cursor,
                    start: row_ctx.window_start,
                    count: row_ctx.window_len,
                });
                out(self, &mut row_ctx)?;
                let next = self.emit(Op::CtxNext { cursor, target: 0 });
                self.emit(Op::Goto { target: loop_start });
                let end = self.ops.len() as u32;
                self.patch(rewind, end)?;
                self.patch(next, end)?;
                Ok(())
            }
            Plan::Skip { input, count } => {
                let count_reg = self.compile_expr(count, ctx)?;
                self.compile_truncated(input, ctx, out, Some(count_reg), None)
            }
            Plan::Limit { input, count } => {
                let count_reg = self.compile_expr(count, ctx)?;
                self.compile_truncated(input, ctx, out, None, Some(count_reg))
            }
            Plan::Unwind { input, expr, alias } => self.compile(input, ctx, &mut |cg, ctx| {
                let list_reg = cg.compile_expr(expr, ctx)?;
                let cursor = cg.alloc_cursor();
                cg.emit(Op::UnwindInit {
                    cursor,
                    list: list_reg,
                });
                let alias_reg = cg.alloc_reg();
                let loop_start = cg.ops.len() as u32;
                let jump = cg.emit(Op::UnwindNext {
                    cursor,
                    dest: alias_reg,
                    target: 0,
                });
                ctx.vars.insert(alias.clone(), alias_reg);
                ctx.window_len = cg.next_reg - ctx.window_start;
                out(cg, ctx)?;
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(jump, end)?;
                Ok(())
            }),
            Plan::Create { input, items } => self.compile(input, ctx, &mut |cg, ctx| {
                for item in items {
                    cg.compile_create_item(item, ctx, None)?;
                }
                out(cg, ctx)
            }),
            Plan::Merge {
                input,
                match_plan,
                items,
                path,
                path_vars,
                on_create,
                on_match,
            } => self.compile(input, ctx, &mut |cg, outer_ctx| {
                let columns = path_vars.len() as u32 + 1;
                // Property maps are compiled up front so MERGE can reject
                // null values before matching, as Neo4j does.
                let mut item_props = Vec::with_capacity(items.len());
                for item in items {
                    let props = cg.compile_item_props(item, outer_ctx)?;
                    cg.emit(Op::CheckMergeProps { props });
                    item_props.push(props);
                }
                let cursor = cg.alloc_cursor();
                cg.emit(Op::CollectOpen { cursor });
                // Match phase: one row per existing match.
                let mut match_base: Option<u32> = None;
                cg.compile(match_plan, outer_ctx, &mut |cg, match_ctx| {
                    let base = cg.reorder_window(match_ctx, path_vars)?;
                    match_base = Some(base);
                    cg.const_reg(Value::Bool(false));
                    cg.emit(Op::CollectRow {
                        cursor,
                        start: base,
                        count: columns,
                    });
                    Ok(())
                })?;
                // Create phase: the pattern did not match, so create the
                // unbound elements and collect the created row.
                let empty_jump = cg.emit(Op::CtxRewind { cursor, target: 0 });
                let skip_create = cg.emit(Op::Goto { target: 0 });
                let create_start = cg.ops.len() as u32;
                for (item, props) in items.iter().zip(&item_props) {
                    cg.compile_create_item(item, outer_ctx, Some(*props))?;
                }
                if let Some((var, elements)) = path {
                    let mut regs = Vec::with_capacity(elements.len());
                    for (name, splice) in elements {
                        regs.push((cg.var_reg(outer_ctx, name)?, *splice));
                    }
                    let dest = cg.alloc_reg();
                    cg.emit(Op::BuildPath {
                        dest,
                        elements: regs,
                    });
                    outer_ctx.vars.insert(var.clone(), dest);
                    outer_ctx.window_len = cg.next_reg - outer_ctx.window_start;
                }
                let base = cg.reorder_window(outer_ctx, path_vars)?;
                cg.const_reg(Value::Bool(true));
                cg.emit(Op::CollectRow {
                    cursor,
                    start: base,
                    count: columns,
                });
                let replay_start = cg.ops.len() as u32;
                cg.patch(empty_jump, create_start)?;
                cg.patch(skip_create, replay_start)?;
                // Replay: ON CREATE for created rows, ON MATCH otherwise.
                let replay_base = match_base.unwrap_or(base);
                // Keep the outer variables; the merge variables come from
                // the collected row.
                let mut replay_ctx = outer_ctx.clone();
                replay_ctx.window_start = replay_base;
                replay_ctx.window_len = path_vars.len() as u32;
                for (index, var) in path_vars.iter().enumerate() {
                    replay_ctx
                        .vars
                        .insert(var.clone(), replay_base + index as u32);
                }
                let loop_start = cg.ops.len() as u32;
                cg.emit(Op::CtxData {
                    cursor,
                    start: replay_base,
                    count: columns,
                });
                let created_flag = replay_base + path_vars.len() as u32;
                let not_created = cg.emit(Op::IfNot {
                    reg: created_flag,
                    target: 0,
                });
                for set_item in on_create {
                    cg.compile_set_item(set_item, &replay_ctx)?;
                }
                let after_create = cg.emit(Op::Goto { target: 0 });
                let match_start = cg.ops.len() as u32;
                cg.patch(not_created, match_start)?;
                for set_item in on_match {
                    cg.compile_set_item(set_item, &replay_ctx)?;
                }
                let after_match = cg.ops.len() as u32;
                cg.patch(after_create, after_match)?;
                out(cg, &mut replay_ctx)?;
                let next = cg.emit(Op::CtxNext { cursor, target: 0 });
                cg.emit(Op::Goto { target: loop_start });
                let end = cg.ops.len() as u32;
                cg.patch(next, end)?;
                Ok(())
            }),
            Plan::SetItems { input, items } => self.compile(input, ctx, &mut |cg, ctx| {
                for item in items {
                    cg.compile_set_item(item, ctx)?;
                }
                out(cg, ctx)
            }),
            Plan::RemoveItems { input, items } => self.compile(input, ctx, &mut |cg, ctx| {
                for item in items {
                    match item {
                        SetItem::Property { variable, key, .. } => {
                            let target = cg.var_reg(ctx, variable)?;
                            let key_reg = cg.const_reg(Value::from(key.as_str()));
                            let null = cg.null_reg();
                            cg.emit(Op::SetProperty {
                                target,
                                key: key_reg,
                                value: null,
                            });
                        }
                        SetItem::Label { variable, labels } => {
                            let target = cg.var_reg(ctx, variable)?;
                            let labels_reg = cg.label_list_reg(labels);
                            cg.emit(Op::SetLabels {
                                target,
                                labels: labels_reg,
                                add: false,
                            });
                        }
                        SetItem::Replace { .. } | SetItem::Append { .. } => {
                            return Err(Error::unsupported(
                                "REMOVE with whole-property replacement is not supported",
                            ));
                        }
                    }
                }
                out(cg, ctx)
            }),
            Plan::Delete {
                input,
                exprs,
                detach,
            } => self.compile(input, ctx, &mut |cg, ctx| {
                for expr in exprs {
                    let target = cg.compile_expr(expr, ctx)?;
                    cg.emit(Op::DeleteAny {
                        target,
                        detach: *detach,
                    });
                }
                out(cg, ctx)
            }),
        }
    }

    /// Compile `input`, truncate collected rows, then iterate the result.
    fn compile_truncated(
        &mut self,
        input: &Plan,
        ctx: &mut Ctx,
        out: &mut dyn FnMut(&mut Codegen, &mut Ctx) -> Result<()>,
        skip: Option<u32>,
        limit: Option<u32>,
    ) -> Result<()> {
        let cursor = self.alloc_cursor();
        self.emit(Op::CollectOpen { cursor });
        let mut produced: Option<Ctx> = None;
        self.compile(input, ctx, &mut |cg, in_ctx| {
            produced = Some(in_ctx.clone());
            cg.emit(Op::CollectRow {
                cursor,
                start: in_ctx.window_start,
                count: in_ctx.window_len,
            });
            Ok(())
        })?;
        let mut row_ctx = produced.ok_or_else(|| Error::internal("empty truncation input"))?;
        let skip_reg = match skip {
            Some(reg) => reg,
            None => self.null_reg(),
        };
        let limit_reg = match limit {
            Some(reg) => reg,
            None => self.null_reg(),
        };
        self.emit(Op::TruncateRows {
            cursor,
            skip: skip_reg,
            limit: limit_reg,
        });
        let rewind = self.emit(Op::CtxRewind { cursor, target: 0 });
        let loop_start = self.ops.len() as u32;
        self.emit(Op::CtxData {
            cursor,
            start: row_ctx.window_start,
            count: row_ctx.window_len,
        });
        out(self, &mut row_ctx)?;
        let next = self.emit(Op::CtxNext { cursor, target: 0 });
        self.emit(Op::Goto { target: loop_start });
        let end = self.ops.len() as u32;
        self.patch(rewind, end)?;
        self.patch(next, end)?;
        Ok(())
    }

    fn emit_property_filters(
        &mut self,
        entity_reg: u32,
        props: &[(String, Value)],
        skip_jumps: &mut Vec<u32>,
    ) -> Result<()> {
        for (key, value) in props {
            let column = self.alloc_reg();
            let name = self.const_index(Value::from(key.as_str()));
            self.emit(Op::Property {
                target: entity_reg,
                name,
                dest: column,
            });
            let constant = self.const_reg(value.clone());
            let equal = self.alloc_reg();
            self.emit(Op::Binary {
                op: binary_code::EQ,
                left: column,
                right: constant,
                dest: equal,
            });
            skip_jumps.push(self.emit(Op::IfNot {
                reg: equal,
                target: 0,
            }));
        }
        Ok(())
    }

    /// Copy the given variables into a fresh contiguous window in that order
    /// and return its base, so both MERGE paths collect rows alike.
    fn reorder_window(&mut self, ctx: &Ctx, vars: &[String]) -> Result<u32> {
        let mut sources = Vec::with_capacity(vars.len());
        for var in vars {
            sources.push(*ctx.vars.get(var).ok_or_else(|| undefined(var))?);
        }
        let base = self.next_reg;
        for src in sources {
            let dest = self.alloc_reg();
            self.emit(Op::Copy { dest, src });
        }
        Ok(base)
    }

    /// Compile the property map of a create item.
    fn compile_item_props(&mut self, item: &CreateItem, ctx: &Ctx) -> Result<u32> {
        match item {
            CreateItem::Node {
                props, prop_exprs, ..
            }
            | CreateItem::Edge {
                props, prop_exprs, ..
            } => self.compile_map(props, prop_exprs, ctx),
        }
    }

    fn compile_create_item(
        &mut self,
        item: &CreateItem,
        ctx: &mut Ctx,
        prebuilt_props: Option<u32>,
    ) -> Result<()> {
        match item {
            CreateItem::Node {
                var,
                labels,
                props,
                prop_exprs,
            } => {
                let labels_reg = self.label_list_reg(labels);
                let props_reg = match prebuilt_props {
                    Some(reg) => reg,
                    None => self.compile_map(props, prop_exprs, ctx)?,
                };
                let dest = self.alloc_reg();
                self.emit(Op::CreateNode {
                    labels: labels_reg,
                    props: props_reg,
                    dest,
                });
                ctx.vars.insert(var.clone(), dest);
                Ok(())
            }
            CreateItem::Edge {
                var,
                edge_type,
                src,
                dst,
                props,
                prop_exprs,
            } => {
                let src_reg = self.var_reg(ctx, src)?;
                let dst_reg = self.var_reg(ctx, dst)?;
                let type_reg = self.const_reg(Value::from(edge_type.as_str()));
                let props_reg = match prebuilt_props {
                    Some(reg) => reg,
                    None => self.compile_map(props, prop_exprs, ctx)?,
                };
                let dest = self.alloc_reg();
                self.emit(Op::CreateEdge {
                    edge_type: type_reg,
                    src: src_reg,
                    dst: dst_reg,
                    props: props_reg,
                    dest,
                });
                ctx.vars.insert(var.clone(), dest);
                Ok(())
            }
        }
    }

    fn compile_set_item(&mut self, item: &SetItem, ctx: &Ctx) -> Result<()> {
        match item {
            SetItem::Property {
                variable,
                key,
                value,
            } => {
                let target = self.var_reg(ctx, variable)?;
                let key_reg = self.const_reg(Value::from(key.as_str()));
                let value_reg = self.compile_expr(value, ctx)?;
                self.emit(Op::SetProperty {
                    target,
                    key: key_reg,
                    value: value_reg,
                });
                Ok(())
            }
            SetItem::Replace { variable, value } => {
                let target = self.var_reg(ctx, variable)?;
                let props_reg = self.compile_expr(value, ctx)?;
                self.emit(Op::ReplaceProperties {
                    target,
                    props: props_reg,
                });
                Ok(())
            }
            SetItem::Append { variable, value } => {
                let target = self.var_reg(ctx, variable)?;
                let props_reg = self.compile_expr(value, ctx)?;
                self.emit(Op::MergeProperties {
                    target,
                    props: props_reg,
                });
                Ok(())
            }
            SetItem::Label { variable, labels } => {
                let target = self.var_reg(ctx, variable)?;
                let labels_reg = self.label_list_reg(labels);
                self.emit(Op::SetLabels {
                    target,
                    labels: labels_reg,
                    add: true,
                });
                Ok(())
            }
        }
    }

    fn label_list_reg(&mut self, labels: &[String]) -> u32 {
        let items: Vec<Value> = labels
            .iter()
            .map(|label| Value::from(label.as_str()))
            .collect();
        self.const_reg(Value::List(items.into()))
    }

    fn compile_map(
        &mut self,
        props: &[(String, Value)],
        prop_exprs: &[(String, Expr)],
        ctx: &Ctx,
    ) -> Result<u32> {
        if props.is_empty() && prop_exprs.is_empty() {
            return Ok(self.const_reg(Value::Map(Vec::new().into())));
        }
        let mut pairs = Vec::with_capacity(props.len() + prop_exprs.len());
        for (key, value) in props {
            let reg = self.const_reg(value.clone());
            pairs.push((key.clone(), reg));
        }
        for (key, expr) in prop_exprs {
            let reg = self.compile_expr(expr, ctx)?;
            pairs.push((key.clone(), reg));
        }
        let dest = self.alloc_reg();
        self.emit(Op::BuildMap { dest, pairs });
        Ok(dest)
    }

    fn var_reg(&self, ctx: &Ctx, name: &str) -> Result<u32> {
        ctx.vars.get(name).copied().ok_or_else(|| undefined(name))
    }

    /// Compile `all|any|none|single(var IN list WHERE predicate)` with
    /// three-valued logic: a predicate that is null never decides the result
    /// on its own, but makes it null when the outcome is otherwise undecided.
    fn compile_quantifier(
        &mut self,
        kind: QuantifierKind,
        var: &str,
        list: &Expr,
        predicate: &Expr,
        ctx: &Ctx,
        dest: u32,
    ) -> Result<()> {
        use QuantifierKind::*;
        let list_reg = self.compile_expr(list, ctx)?;
        let cursor = self.alloc_cursor();
        self.emit(Op::UnwindInit {
            cursor,
            list: list_reg,
        });
        let result = self.alloc_reg();
        let saw_null = self.alloc_reg();
        let false_reg = self.const_reg(Value::Bool(false));
        let true_reg = self.const_reg(Value::Bool(true));
        let null_reg = self.null_reg();
        // `all` and `none` start true, `any` false; `single` counts matches.
        let start = match kind {
            All | None => true_reg,
            Any => false_reg,
            Single => self.const_reg(Value::Int(0)),
        };
        self.emit(Op::Copy {
            dest: result,
            src: start,
        });
        self.emit(Op::Copy {
            dest: saw_null,
            src: false_reg,
        });
        let element = self.alloc_reg();
        let loop_start = self.ops.len() as u32;
        let jump_end = self.emit(Op::UnwindNext {
            cursor,
            dest: element,
            target: 0,
        });
        let mut inner = ctx.clone();
        inner.vars.insert(var.to_owned(), element);
        let condition = self.compile_expr(predicate, &inner)?;
        let is_null = self.alloc_reg();
        self.emit(Op::IsNull {
            src: condition,
            dest: is_null,
        });
        let not_null = self.emit(Op::IfNot {
            reg: is_null,
            target: 0,
        });
        // Null predicate: remember it and move on.
        self.emit(Op::Copy {
            dest: saw_null,
            src: true_reg,
        });
        self.emit(Op::Goto { target: loop_start });
        let decided = self.ops.len() as u32;
        self.patch(not_null, decided)?;
        let not_true = self.emit(Op::IfNot {
            reg: condition,
            target: 0,
        });
        // Predicate is true.
        let mut break_jump: Option<u32> = Option::None;
        match kind {
            All => {
                // One satisfied element does not decide `all`.
                self.emit(Op::Goto { target: loop_start });
            }
            None => {
                self.emit(Op::Copy {
                    dest: result,
                    src: false_reg,
                });
                break_jump = Some(self.emit(Op::Goto { target: 0 }));
            }
            Any => {
                self.emit(Op::Copy {
                    dest: result,
                    src: true_reg,
                });
                break_jump = Some(self.emit(Op::Goto { target: 0 }));
            }
            Single => {
                let one = self.const_reg(Value::Int(1));
                let next = self.alloc_reg();
                self.emit(Op::Binary {
                    op: binary_code::ADD,
                    left: result,
                    right: one,
                    dest: next,
                });
                self.emit(Op::Copy {
                    dest: result,
                    src: next,
                });
                self.emit(Op::Goto { target: loop_start });
            }
        }
        // Predicate is false.
        let false_branch = self.ops.len() as u32;
        self.patch(not_true, false_branch)?;
        match kind {
            All => {
                // One unsatisfied element decides `all`.
                self.emit(Op::Copy {
                    dest: result,
                    src: false_reg,
                });
                break_jump = Some(self.emit(Op::Goto { target: 0 }));
            }
            None | Any | Single => {
                self.emit(Op::Goto { target: loop_start });
            }
        }
        let end = self.ops.len() as u32;
        self.patch(jump_end, end)?;
        if let Some(break_jump) = break_jump {
            self.patch(break_jump, end)?;
        }
        match kind {
            All | None => {
                // A false result stands; otherwise a seen null makes it null.
                let no_null = self.emit(Op::IfNot {
                    reg: saw_null,
                    target: 0,
                });
                let settled = self.emit(Op::IfNot {
                    reg: result,
                    target: 0,
                });
                self.emit(Op::Copy {
                    dest: result,
                    src: null_reg,
                });
                let done = self.ops.len() as u32;
                self.patch(no_null, done)?;
                self.patch(settled, done)?;
            }
            Any => {
                // A true result stands; otherwise a seen null makes it null.
                let no_null = self.emit(Op::IfNot {
                    reg: saw_null,
                    target: 0,
                });
                let inverted = self.alloc_reg();
                self.emit(Op::Unary {
                    op: unary_code::NOT,
                    src: result,
                    dest: inverted,
                });
                let settled = self.emit(Op::IfNot {
                    reg: inverted,
                    target: 0,
                });
                self.emit(Op::Copy {
                    dest: result,
                    src: null_reg,
                });
                let done = self.ops.len() as u32;
                self.patch(no_null, done)?;
                self.patch(settled, done)?;
            }
            Single => {
                // `result` counts matches: >1 false, 1 true, 0 false, with
                // any seen null turning the undecided outcomes null.
                let one = self.const_reg(Value::Int(1));
                let more = self.alloc_reg();
                self.emit(Op::Binary {
                    op: binary_code::GT,
                    left: result,
                    right: one,
                    dest: more,
                });
                let at_most_one = self.emit(Op::IfNot {
                    reg: more,
                    target: 0,
                });
                self.emit(Op::Copy {
                    dest: result,
                    src: false_reg,
                });
                let done = self.emit(Op::Goto { target: 0 });
                let check_one = self.ops.len() as u32;
                self.patch(at_most_one, check_one)?;
                let exactly_one = self.alloc_reg();
                self.emit(Op::Binary {
                    op: binary_code::EQ,
                    left: result,
                    right: one,
                    dest: exactly_one,
                });
                let none_matched = self.emit(Op::IfNot {
                    reg: exactly_one,
                    target: 0,
                });
                let no_null = self.emit(Op::IfNot {
                    reg: saw_null,
                    target: 0,
                });
                self.emit(Op::Copy {
                    dest: result,
                    src: null_reg,
                });
                let matched_done = self.emit(Op::Goto { target: 0 });
                let matched = self.ops.len() as u32;
                self.patch(no_null, matched)?;
                self.emit(Op::Copy {
                    dest: result,
                    src: true_reg,
                });
                let matched_true_done = self.emit(Op::Goto { target: 0 });
                let zero = self.ops.len() as u32;
                self.patch(none_matched, zero)?;
                let no_null_zero = self.emit(Op::IfNot {
                    reg: saw_null,
                    target: 0,
                });
                self.emit(Op::Copy {
                    dest: result,
                    src: null_reg,
                });
                let zero_done = self.emit(Op::Goto { target: 0 });
                let zero_false = self.ops.len() as u32;
                self.patch(no_null_zero, zero_false)?;
                self.emit(Op::Copy {
                    dest: result,
                    src: false_reg,
                });
                let single_done = self.ops.len() as u32;
                self.patch(done, single_done)?;
                self.patch(matched_done, single_done)?;
                self.patch(matched_true_done, single_done)?;
                self.patch(zero_done, single_done)?;
            }
        }
        self.emit(Op::Copy { dest, src: result });
        Ok(())
    }

    fn compile_expr(&mut self, expr: &Expr, ctx: &Ctx) -> Result<u32> {
        let reg = self.alloc_reg();
        self.compile_expr_into(expr, ctx, reg)?;
        Ok(reg)
    }

    fn compile_expr_into(&mut self, expr: &Expr, ctx: &Ctx, dest: u32) -> Result<()> {
        match expr {
            Expr::Literal(value) => {
                let index = self.constants.len() as u32;
                self.constants.push(value.clone());
                self.emit(Op::Constant { dest, value: index });
            }
            Expr::Parameter(name) => {
                return Err(Error::internal(format!(
                    "unsubstituted parameter `${name}`"
                )));
            }
            Expr::Variable(name) => {
                let src = self.var_reg(ctx, name)?;
                self.emit(Op::Copy { dest, src });
            }
            Expr::Property { base, key } => {
                let base_reg = self.compile_expr(base, ctx)?;
                // Resolve the key at runtime: it may be created by this very
                // statement (`CREATE (n {num: 1}) RETURN n.num`).
                let name = self.const_index(Value::from(key.as_str()));
                self.emit(Op::Property {
                    target: base_reg,
                    name,
                    dest,
                });
            }
            Expr::Subscript { base, index } => {
                let base_reg = self.compile_expr(base, ctx)?;
                let index_reg = self.compile_expr(index, ctx)?;
                self.emit(Op::Subscript {
                    base: base_reg,
                    index: index_reg,
                    dest,
                });
            }
            Expr::ListComprehension {
                var,
                list,
                predicate,
                projection,
            } => {
                let list_reg = self.compile_expr(list, ctx)?;
                let cursor = self.alloc_cursor();
                self.emit(Op::UnwindInit {
                    cursor,
                    list: list_reg,
                });
                self.emit(Op::BuildList {
                    dest,
                    items: Vec::new(),
                });
                let element = self.alloc_reg();
                let loop_start = self.ops.len() as u32;
                let jump = self.emit(Op::UnwindNext {
                    cursor,
                    dest: element,
                    target: 0,
                });
                let mut inner = ctx.clone();
                inner.vars.insert(var.clone(), element);
                let mut skip = None;
                if let Some(predicate) = predicate {
                    let condition = self.compile_expr(predicate, &inner)?;
                    skip = Some(self.emit(Op::IfNot {
                        reg: condition,
                        target: 0,
                    }));
                }
                let value = match projection {
                    Some(projection) => self.compile_expr(projection, &inner)?,
                    None => element,
                };
                self.emit(Op::ListPush { list: dest, value });
                if let Some(skip) = skip {
                    let target = self.ops.len() as u32;
                    self.patch(skip, target)?;
                }
                self.emit(Op::Goto { target: loop_start });
                let end = self.ops.len() as u32;
                self.patch(jump, end)?;
            }
            Expr::Quantifier {
                kind,
                var,
                list,
                predicate,
            } => {
                self.compile_quantifier(*kind, var, list, predicate, ctx, dest)?;
            }
            Expr::Reduce {
                accumulator,
                init,
                var,
                list,
                expression,
            } => {
                let acc = self.compile_expr(init, ctx)?;
                let list_reg = self.compile_expr(list, ctx)?;
                let cursor = self.alloc_cursor();
                self.emit(Op::UnwindInit {
                    cursor,
                    list: list_reg,
                });
                let element = self.alloc_reg();
                let loop_start = self.ops.len() as u32;
                let jump = self.emit(Op::UnwindNext {
                    cursor,
                    dest: element,
                    target: 0,
                });
                let mut inner = ctx.clone();
                inner.vars.insert(var.clone(), element);
                inner.vars.insert(accumulator.clone(), acc);
                let value = self.compile_expr(expression, &inner)?;
                self.emit(Op::Copy {
                    dest: acc,
                    src: value,
                });
                self.emit(Op::Goto { target: loop_start });
                let end = self.ops.len() as u32;
                self.patch(jump, end)?;
                self.emit(Op::Copy { dest, src: acc });
            }
            Expr::Slice { base, lower, upper } => {
                let base_reg = self.compile_expr(base, ctx)?;
                let lower_reg = match lower {
                    Some(lower) => self.compile_expr(lower, ctx)?,
                    None => self.null_reg(),
                };
                let upper_reg = match upper {
                    Some(upper) => self.compile_expr(upper, ctx)?,
                    None => self.null_reg(),
                };
                self.emit(Op::Slice {
                    base: base_reg,
                    lower: lower_reg,
                    upper: upper_reg,
                    open_lower: lower.is_none(),
                    open_upper: upper.is_none(),
                    dest,
                });
            }
            Expr::Unary { op, expr } => {
                let src = self.compile_expr(expr, ctx)?;
                self.emit(Op::Unary {
                    op: unary_code(*op),
                    src,
                    dest,
                });
            }
            Expr::Binary { op, left, right } => {
                let left_reg = self.compile_expr(left, ctx)?;
                let right_reg = self.compile_expr(right, ctx)?;
                self.emit(Op::Binary {
                    op: binary_code::encode(*op),
                    left: left_reg,
                    right: right_reg,
                    dest,
                });
            }
            Expr::Function {
                name,
                distinct,
                star,
                args,
            } => {
                if *distinct || *star {
                    return Err(Error::unsupported(
                        "DISTINCT/* function arguments are only valid in aggregations",
                    ));
                }
                let func_code = scalar_function(name)?;
                let arg_regs: Vec<u32> = args
                    .iter()
                    .map(|arg| self.compile_expr(arg, ctx))
                    .collect::<Result<_>>()?;
                self.emit(Op::Function {
                    func: func_code,
                    args: arg_regs,
                    dest,
                });
            }
            Expr::List(items) => {
                let item_regs: Vec<u32> = items
                    .iter()
                    .map(|item| self.compile_expr(item, ctx))
                    .collect::<Result<_>>()?;
                self.emit(Op::BuildList {
                    dest,
                    items: item_regs,
                });
            }
            Expr::Map(entries) => {
                let mut pairs = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    let reg = self.compile_expr(value, ctx)?;
                    pairs.push((key.clone(), reg));
                }
                self.emit(Op::BuildMap { dest, pairs });
            }
            Expr::Case {
                subject,
                alternatives,
                default,
            } => {
                let subject_reg = match subject {
                    Some(subject) => Some(self.compile_expr(subject, ctx)?),
                    None => None,
                };
                let mut end_jumps = Vec::new();
                for (when, then) in alternatives {
                    let when_reg = self.compile_expr(when, ctx)?;
                    let condition = if let Some(subject_reg) = subject_reg {
                        let reg = self.alloc_reg();
                        self.emit(Op::Binary {
                            op: binary_code::EQ,
                            left: subject_reg,
                            right: when_reg,
                            dest: reg,
                        });
                        reg
                    } else {
                        when_reg
                    };
                    let skip = self.emit(Op::IfNot {
                        reg: condition,
                        target: 0,
                    });
                    self.compile_expr_into(then, ctx, dest)?;
                    end_jumps.push(self.emit(Op::Goto { target: 0 }));
                    let next = self.ops.len() as u32;
                    self.patch(skip, next)?;
                }
                match default {
                    Some(default) => self.compile_expr_into(default, ctx, dest)?,
                    None => {
                        self.emit(Op::Null { dest });
                    }
                }
                let end = self.ops.len() as u32;
                for jump in end_jumps {
                    self.patch(jump, end)?;
                }
            }
            Expr::IsNull { expr, negated } => {
                let src = self.compile_expr(expr, ctx)?;
                let test = self.alloc_reg();
                self.emit(Op::IsNull { src, dest: test });
                if *negated {
                    self.emit(Op::Unary {
                        op: unary_code::NOT,
                        src: test,
                        dest,
                    });
                } else {
                    self.emit(Op::Copy { dest, src: test });
                }
            }
            Expr::Pattern(_) | Expr::PatternComprehension { .. } => {
                return Err(Error::syntax(
                    "pattern expressions are only allowed in predicates",
                ));
            }
            Expr::LabelCheck { expr, labels } => {
                let node_reg = self.compile_expr(expr, ctx)?;
                let mut checks = Vec::new();
                for label in labels {
                    let label_reg = self.const_index(Value::from(label.as_str()));
                    let check = self.alloc_reg();
                    self.emit(Op::HasLabel {
                        node: node_reg,
                        label: label_reg,
                        dest: check,
                    });
                    checks.push(check);
                }
                let mut current = checks[0];
                for check in &checks[1..] {
                    let combined = self.alloc_reg();
                    self.emit(Op::Binary {
                        op: binary_code::AND,
                        left: current,
                        right: *check,
                        dest: combined,
                    });
                    current = combined;
                }
                self.emit(Op::Copy { dest, src: current });
            }
        }
        Ok(())
    }
}

fn undefined(name: &str) -> Error {
    Error::new(
        crate::error::ErrorCode::NotFound,
        format!("variable `{name}` is not defined"),
    )
}

fn scalar_function(name: &str) -> Result<u8> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "id" => Ok(func::ID),
        "labels" => Ok(func::LABELS),
        "type" => Ok(func::TYPE),
        "startnode" => Ok(func::START_NODE),
        "endnode" => Ok(func::END_NODE),
        "coalesce" => Ok(func::COALESCE),
        "tostring" => Ok(func::TO_STRING),
        "tointeger" => Ok(func::TO_INTEGER),
        "tofloat" => Ok(func::TO_FLOAT),
        "toboolean" => Ok(func::TO_BOOLEAN),
        "size" => Ok(func::SIZE),
        "properties" => Ok(func::PROPERTIES),
        "keys" => Ok(func::KEYS),
        "nodes" => Ok(func::NODES),
        "relationships" => Ok(func::RELATIONSHIPS),
        "length" => Ok(func::LENGTH),
        "range" => Ok(func::RANGE),
        "head" => Ok(func::HEAD),
        "last" => Ok(func::LAST),
        "tail" => Ok(func::TAIL),
        "reverse" => Ok(func::REVERSE),
        "split" => Ok(func::SPLIT),
        "substring" => Ok(func::SUBSTRING),
        "rand" => Ok(func::RAND),
        "toupper" => Ok(func::TO_UPPER),
        "tolower" => Ok(func::TO_LOWER),
        "trim" => Ok(func::TRIM),
        "ltrim" => Ok(func::LTRIM),
        "rtrim" => Ok(func::RTRIM),
        "left" => Ok(func::LEFT),
        "right" => Ok(func::RIGHT),
        "replace" => Ok(func::REPLACE),
        "abs" => Ok(func::ABS),
        "ceil" => Ok(func::CEIL),
        "floor" => Ok(func::FLOOR),
        "round" => Ok(func::ROUND),
        "sign" => Ok(func::SIGN),
        "sqrt" => Ok(func::SQRT),
        "exp" => Ok(func::EXP),
        "log" => Ok(func::LOG),
        "log10" => Ok(func::LOG10),
        "sin" => Ok(func::SIN),
        "cos" => Ok(func::COS),
        "tan" => Ok(func::TAN),
        "cot" => Ok(func::COT),
        "asin" => Ok(func::ASIN),
        "acos" => Ok(func::ACOS),
        "atan" => Ok(func::ATAN),
        "atan2" => Ok(func::ATAN2),
        "degrees" => Ok(func::DEGREES),
        "radians" => Ok(func::RADIANS),
        "haversin" => Ok(func::HAVERSIN),
        "pi" => Ok(func::PI),
        "e" => Ok(func::E),
        other => Err(Error::unsupported(format!(
            "function `{other}` is not supported yet"
        ))),
    }
}
