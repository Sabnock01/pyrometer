use shared::context::*;
use shared::range::elem_ty::Dynamic;
use shared::range::Range;

use crate::VarType;
use petgraph::{visit::EdgeRef, Direction};
use shared::{
    analyzer::AnalyzerLike, nodes::*, range::elem::RangeOp, range::elem_ty::DynSide, Edge, Node,
    NodeIdx,
};
use solang_parser::pt::{Expression, Loc, Statement};

pub mod exprs;
use exprs::*;

pub mod analyzers;
pub use analyzers::*;

#[derive(Debug, Clone)]
pub enum ExprRet {
    CtxKilled,
    Single((ContextNode, NodeIdx)),
    Multi(Vec<ExprRet>),
    Fork(Box<ExprRet>, Box<ExprRet>),
}

impl ExprRet {
    pub fn expect_single(&self) -> (ContextNode, NodeIdx) {
        match self {
            ExprRet::Single(inner) => *inner,
            _ => panic!("Expected a single return got multiple"),
        }
    }

    pub fn expect_multi(self) -> Vec<ExprRet> {
        match self {
            ExprRet::Multi(inner) => inner,
            _ => panic!("Expected a multi return got single"),
        }
    }
}

impl<T> ContextBuilder for T where T: AnalyzerLike + Sized + ExprParser {}

pub trait ContextBuilder: AnalyzerLike + Sized + ExprParser {
    fn parse_ctx_statement(
        &mut self,
        stmt: &Statement,
        unchecked: bool,
        parent_ctx: Option<impl Into<NodeIdx> + Clone + Copy>,
    ) where
        Self: Sized,
    {
        // println!("stmt: {:?}\n", stmt);
        if let Some(parent) = parent_ctx {
            match self.node(parent) {
                Node::Context(_) => {
                    let ctx = ContextNode::from(parent.into());
                    if ctx.is_killed(self) {
                        return;
                    }
                    if ctx.live_forks(self).is_empty() {
                        self.parse_ctx_stmt_inner(stmt, unchecked, parent_ctx)
                    } else {
                        ctx.live_forks(self).iter().for_each(|fork_ctx| {
                            self.parse_ctx_stmt_inner(stmt, unchecked, Some(*fork_ctx));
                        });
                    }
                }
                _ => self.parse_ctx_stmt_inner(stmt, unchecked, parent_ctx),
            }
        } else {
            self.parse_ctx_stmt_inner(stmt, unchecked, parent_ctx)
        }
    }

    fn parse_ctx_stmt_inner(
        &mut self,
        stmt: &Statement,
        _unchecked: bool,
        parent_ctx: Option<impl Into<NodeIdx> + Clone + Copy>,
    ) where
        Self: Sized,
    {
        use Statement::*;
        match stmt {
            Block {
                loc,
                unchecked,
                statements,
            } => {
                let parent = parent_ctx.expect("Free floating contexts shouldn't happen");
                let ctx_node = match self.node(parent) {
                    Node::Function(_fn_node) => {
                        let ctx = Context::new(
                            FunctionNode::from(parent.into()),
                            FunctionNode::from(parent.into()).name(self),
                            *loc,
                        );
                        let ctx_node = self.add_node(Node::Context(ctx));
                        self.add_edge(ctx_node, parent, Edge::Context(ContextEdge::Context));
                        ctx_node
                    }
                    Node::Context(_) => {
                        // let ctx = Context::new_subctx(
                        //     ContextNode::from(parent.into()),
                        //     *loc,
                        //     false,
                        //     self,
                        // );
                        // let ctx_node = self.add_node(Node::Context(ctx));
                        // self.add_edge(ctx_node, parent, Edge::Context(ContextEdge::Subcontext));
                        // ctx_node
                        parent.into()
                    }
                    e => todo!(
                        "Expected a context to be created by a function or context but got: {:?}",
                        e
                    ),
                };

                // optionally add named input and named outputs into context
                self.graph()
                    .edges_directed(parent.into(), Direction::Incoming)
                    .filter(|edge| *edge.weight() == Edge::FunctionParam)
                    .map(|edge| FunctionParamNode::from(edge.source()))
                    .collect::<Vec<FunctionParamNode>>()
                    .iter()
                    .for_each(|param_node| {
                        let func_param = param_node.underlying(self);
                        if let Some(cvar) =
                            ContextVar::maybe_new_from_func_param(self, func_param.clone())
                        {
                            let cvar_node = self.add_node(Node::ContextVar(cvar));
                            self.add_edge(
                                cvar_node,
                                ctx_node,
                                Edge::Context(ContextEdge::Variable),
                            );
                        }
                    });

                self.graph()
                    .edges_directed(parent.into(), Direction::Incoming)
                    .filter(|edge| *edge.weight() == Edge::FunctionReturn)
                    .map(|edge| FunctionReturnNode::from(edge.source()))
                    .collect::<Vec<FunctionReturnNode>>()
                    .iter()
                    .for_each(|ret_node| {
                        let func_ret = ret_node.underlying(self);
                        if let Some(cvar) =
                            ContextVar::maybe_new_from_func_ret(self, func_ret.clone())
                        {
                            let cvar_node = self.add_node(Node::ContextVar(cvar));
                            self.add_edge(
                                cvar_node,
                                ctx_node,
                                Edge::Context(ContextEdge::Variable),
                            );
                        }
                    });

                let forks = ContextNode::from(ctx_node).live_forks(self);
                if forks.is_empty() {
                    statements.iter().for_each(|stmt| {
                        self.parse_ctx_statement(stmt, *unchecked, Some(ctx_node))
                    });
                } else {
                    forks.into_iter().for_each(|fork| {
                        statements.iter().for_each(|stmt| {
                            self.parse_ctx_statement(stmt, *unchecked, Some(fork))
                        });
                    });
                }
            }
            VariableDefinition(loc, var_decl, maybe_expr) => {
                let ctx = ContextNode::from(
                    parent_ctx
                        .expect("No context for variable definition?")
                        .into(),
                );
                let forks = ctx.live_forks(self);
                if forks.is_empty() {
                    let name = var_decl.name.clone().expect("Variable wasn't named");
                    let (lhs_ctx, ty) = self.parse_ctx_expr(&var_decl.ty, ctx).expect_single();
                    let ty = VarType::try_from_idx(self, ty).expect("Not a known type");
                    let var = ContextVar {
                        loc: Some(*loc),
                        name: name.to_string(),
                        display_name: name.to_string(),
                        storage: var_decl.storage.clone(),
                        is_tmp: false,
                        tmp_of: None,
                        ty,
                    };
                    if let Some(rhs) = maybe_expr {
                        let rhs_paths = self.parse_ctx_expr(rhs, ctx);
                        self.match_var_def(*loc, &var, &rhs_paths);
                    } else {
                        let lhs = ContextVarNode::from(self.add_node(Node::ContextVar(var)));
                        self.add_edge(lhs, lhs_ctx, Edge::Context(ContextEdge::Variable));
                    }
                } else {
                    forks.into_iter().for_each(|ctx| {
                        let name = var_decl.name.clone().expect("Variable wasn't named");
                        let (lhs_ctx, ty) = self.parse_ctx_expr(&var_decl.ty, ctx).expect_single();
                        let ty = VarType::try_from_idx(self, ty).expect("Not a known type");
                        let var = ContextVar {
                            loc: Some(*loc),
                            name: name.to_string(),
                            display_name: name.to_string(),
                            storage: var_decl.storage.clone(),
                            is_tmp: false,
                            tmp_of: None,
                            ty,
                        };
                        if let Some(rhs) = maybe_expr {
                            let rhs_paths = self.parse_ctx_expr(rhs, ctx);
                            self.match_var_def(*loc, &var, &rhs_paths);
                        } else {
                            let lhs = ContextVarNode::from(self.add_node(Node::ContextVar(var)));
                            self.add_edge(lhs, lhs_ctx, Edge::Context(ContextEdge::Variable));
                        }
                    });
                }
            }
            Assembly {
                loc: _,
                dialect: _,
                flags: _,
                block: _yul_block,
            } => {}
            Args(_loc, _args) => {}
            If(loc, if_expr, true_expr, maybe_false_expr) => {
                let ctx = ContextNode::from(parent_ctx.expect("Dangling if statement").into());
                let forks = ctx.live_forks(self);
                if forks.is_empty() {
                    self.cond_op_stmt(*loc, if_expr, true_expr, maybe_false_expr, ctx)
                } else {
                    forks.into_iter().for_each(|parent| {
                        self.cond_op_stmt(*loc, if_expr, true_expr, maybe_false_expr, parent.into())
                    })
                }
            }
            While(_loc, _cond, _body) => {}
            Expression(_loc, expr) => {
                if let Some(parent) = parent_ctx {
                    let _paths = self.parse_ctx_expr(expr, ContextNode::from(parent.into()));
                }
            }
            For(_loc, _maybe_for_start, _maybe_for_middle, _maybe_for_end, _maybe_for_body) => {}
            DoWhile(_loc, _while_stmt, _while_expr) => {}
            Continue(_loc) => {}
            Break(_loc) => {}
            Return(loc, maybe_ret_expr) => {
                if let Some(ret_expr) = maybe_ret_expr {
                    if let Some(parent) = parent_ctx {
                        let forks = ContextNode::from(parent.into()).live_forks(self);
                        if forks.is_empty() {
                            let paths =
                                self.parse_ctx_expr(ret_expr, ContextNode::from(parent.into()));
                            // println!("return paths: {:?}", paths);
                            match paths {
                                ExprRet::CtxKilled => {}
                                ExprRet::Single((ctx, expr)) => {
                                    // println!("adding return: {:?}", ctx.path(self));
                                    self.add_edge(expr, ctx, Edge::Context(ContextEdge::Return));
                                    ctx.set_return_node(*loc, expr.into(), self);
                                }
                                ExprRet::Multi(rets) => {
                                    rets.into_iter().for_each(|expr_ret| {
                                        let (ctx, expr) = expr_ret.expect_single();
                                        self.add_edge(
                                            expr,
                                            ctx,
                                            Edge::Context(ContextEdge::Return),
                                        );
                                        ctx.set_return_node(*loc, expr.into(), self);
                                    });
                                }
                                ExprRet::Fork(_world1, _world2) => {
                                    todo!("here")
                                }
                            }
                        } else {
                            forks.into_iter().for_each(|parent| {
                                let paths =
                                    self.parse_ctx_expr(ret_expr, ContextNode::from(parent));
                                match paths {
                                    ExprRet::CtxKilled => {}
                                    ExprRet::Single((ctx, expr)) => {
                                        self.add_edge(
                                            expr,
                                            ctx,
                                            Edge::Context(ContextEdge::Return),
                                        );
                                        ctx.set_return_node(*loc, expr.into(), self);
                                    }
                                    ExprRet::Multi(rets) => {
                                        rets.into_iter().for_each(|expr_ret| {
                                            let (ctx, expr) = expr_ret.expect_single();
                                            self.add_edge(
                                                expr,
                                                ctx,
                                                Edge::Context(ContextEdge::Return),
                                            );
                                            ctx.set_return_node(*loc, expr.into(), self);
                                        });
                                    }
                                    ExprRet::Fork(_world1, _world2) => {
                                        todo!("here")
                                    }
                                }
                            });
                        }
                    }
                }
            }
            Revert(loc, _maybe_err_path, _exprs) => {
                if let Some(parent) = parent_ctx {
                    let parent = ContextNode::from(parent.into());
                    let forks = parent.live_forks(self);
                    if forks.is_empty() {
                        parent.kill(self, *loc);
                    } else {
                        forks.into_iter().for_each(|parent| {
                            parent.kill(self, *loc);
                        });
                    }
                }
            }
            RevertNamedArgs(_loc, _maybe_err_path, _named_args) => {}
            Emit(_loc, _emit_expr) => {}
            Try(_loc, _try_expr, _maybe_returns, _clauses) => {}
            Error(_loc) => {}
        }
    }

    fn match_var_def(&mut self, loc: Loc, var: &ContextVar, rhs_paths: &ExprRet) {
        match rhs_paths {
            ExprRet::CtxKilled => {}
            ExprRet::Single((rhs_ctx, rhs)) => {
                let lhs = ContextVarNode::from(self.add_node(Node::ContextVar(var.clone())));
                self.add_edge(lhs, *rhs_ctx, Edge::Context(ContextEdge::Variable));
                let rhs = ContextVarNode::from(*rhs);
                let (_, new_lhs) = self.assign(loc, lhs, rhs, *rhs_ctx).expect_single();
                self.add_edge(new_lhs, *rhs_ctx, Edge::Context(ContextEdge::Variable));
            }
            ExprRet::Multi(rets) => {
                rets.into_iter().for_each(|expr_ret| {
                    self.match_var_def(loc, var, expr_ret);
                });
            }
            ExprRet::Fork(world1, world2) => {
                self.match_var_def(loc, var, world1);
                self.match_var_def(loc, var, world2);
            }
        }
    }

    fn match_expr(&mut self, paths: &ExprRet) {
        match paths {
            ExprRet::CtxKilled => {}
            ExprRet::Single((ctx, expr)) => {
                self.add_edge(*expr, *ctx, Edge::Context(ContextEdge::Call));
            }
            ExprRet::Multi(rets) => {
                rets.iter().for_each(|expr_ret| {
                    self.match_expr(expr_ret);
                });
            }
            ExprRet::Fork(world1, world2) => {
                self.match_expr(world1);
                self.match_expr(world2);
            }
        }
    }

    fn parse_ctx_expr(&mut self, expr: &Expression, ctx: ContextNode) -> ExprRet {
        if ctx.is_killed(self) {
            return ExprRet::CtxKilled;
        }

        // println!("has any forks: {}", ctx.forks(self).len());
        if ctx.live_forks(self).is_empty() {
            // println!("has no live forks");
            self.parse_ctx_expr_inner(expr, ctx)
        } else {
            // println!("has live forks");
            ctx.live_forks(self).iter().for_each(|fork_ctx| {
                // println!("fork_ctx: {}\n", fork_ctx.underlying(self).path);
                self.parse_ctx_expr(expr, *fork_ctx);
            });
            ExprRet::Multi(vec![])
        }
    }

    fn parse_ctx_expr_inner(&mut self, expr: &Expression, ctx: ContextNode) -> ExprRet {
        use Expression::*;
        println!("ctx: {}, {:?}\n", ctx.underlying(self).path, expr);
        match expr {
            Variable(ident) => self.variable(ident, ctx),
            // literals
            NumberLiteral(loc, int, exp) => self.number_literal(ctx, *loc, int, exp),
            AddressLiteral(loc, addr) => self.address_literal(ctx, *loc, addr),
            StringLiteral(lits) => ExprRet::Multi(
                lits.iter()
                    .map(|lit| self.string_literal(ctx, lit.loc, &lit.string))
                    .collect(),
            ),
            BoolLiteral(loc, b) => self.bool_literal(ctx, *loc, *b),
            // bin ops
            Add(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Add, false)
            }
            AssignAdd(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Add, true)
            }
            Subtract(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Sub, false)
            }
            AssignSubtract(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Sub, true)
            }
            Multiply(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Mul, false)
            }
            AssignMultiply(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Mul, true)
            }
            Divide(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Div, false)
            }
            AssignDivide(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Div, true)
            }
            Modulo(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Mod, false)
            }
            AssignModulo(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Mod, true)
            }
            ShiftLeft(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Shl, false)
            }
            AssignShiftLeft(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Shl, true)
            }
            ShiftRight(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Shr, false)
            }
            AssignShiftRight(loc, lhs_expr, rhs_expr) => {
                self.op_expr(*loc, lhs_expr, rhs_expr, ctx, RangeOp::Shr, true)
            }
            ConditionalOperator(loc, if_expr, true_expr, false_expr) => {
                self.cond_op_expr(*loc, if_expr, true_expr, false_expr, ctx)
            }
            // assign
            Assign(loc, lhs_expr, rhs_expr) => self.assign_exprs(*loc, lhs_expr, rhs_expr, ctx),
            List(loc, params) => self.list(ctx, *loc, params),
            // array
            ArraySubscript(_loc, ty_expr, None) => self.array_ty(ty_expr, ctx),
            ArraySubscript(loc, ty_expr, Some(index_expr)) => {
                self.index_into_array(*loc, ty_expr, index_expr, ctx)
            }
            Type(_loc, ty) => {
                if let Some(builtin) = Builtin::try_from_ty(ty.clone()) {
                    if let Some(idx) = self.builtins().get(&builtin) {
                        ExprRet::Single((ctx, *idx))
                    } else {
                        let idx = self.add_node(Node::Builtin(builtin.clone()));
                        self.builtins_mut().insert(builtin, idx);
                        ExprRet::Single((ctx, idx))
                    }
                } else {
                    todo!("??")
                }
            }
            MemberAccess(loc, member_expr, ident) => {
                self.member_access(*loc, member_expr, ident, ctx)
            }
            // // comparator
            Equal(loc, lhs, rhs) => self.cmp(*loc, lhs, RangeOp::Eq, rhs, ctx),
            Less(loc, lhs, rhs) => self.cmp(*loc, lhs, RangeOp::Lt, rhs, ctx),
            More(loc, lhs, rhs) => self.cmp(*loc, lhs, RangeOp::Gt, rhs, ctx),
            LessEqual(loc, lhs, rhs) => self.cmp(*loc, lhs, RangeOp::Lte, rhs, ctx),
            MoreEqual(loc, lhs, rhs) => self.cmp(*loc, lhs, RangeOp::Gte, rhs, ctx),

            Not(loc, expr) => self.not(*loc, expr, ctx),
            FunctionCall(loc, func_expr, input_exprs) => {
                let (_ctx, func_idx) = self.parse_ctx_expr(func_expr, ctx).expect_single();
                match self.node(func_idx) {
                    Node::Function(underlying) => {
                        if let Some(func_name) = &underlying.name {
                            match &*func_name.name {
                                "require" | "assert" => {
                                    self.handle_require(input_exprs, ctx);
                                    return ExprRet::Multi(vec![]);
                                }
                                _ => {}
                            }
                        }
                    }
                    Node::Builtin(_ty) => {
                        // it is a cast
                        let (ctx, cvar) = self.parse_ctx_expr(&input_exprs[0], ctx).expect_single();

                        let new_var = self.advance_var_in_ctx(cvar.into(), *loc, ctx);
                        new_var.underlying_mut(self).ty =
                            VarType::try_from_idx(self, func_idx).expect("");
                        if let Some(r) = ContextVarNode::from(cvar).range(self) {
                            // TODO: cast the ranges appropriately (set cap or convert to signed/unsigned concrete)
                            new_var.set_range_min(self, r.range_min());
                            new_var.set_range_max(self, r.range_max());
                        }
                        return ExprRet::Single((ctx, new_var.into()));
                    }
                    _ => todo!(),
                }

                let _inputs: Vec<_> = input_exprs
                    .into_iter()
                    .map(|expr| self.parse_ctx_expr(expr, ctx))
                    .collect();

                // todo!("func call")
                // vec![func_idx]
                ExprRet::Single((ctx, func_idx))
            }

            e => todo!("{:?}", e),
        }
    }

    fn assign_exprs(
        &mut self,
        loc: Loc,
        lhs_expr: &Expression,
        rhs_expr: &Expression,
        ctx: ContextNode,
    ) -> ExprRet {
        let lhs_paths = self.parse_ctx_expr(&lhs_expr, ctx);
        let rhs_paths = self.parse_ctx_expr(&rhs_expr, ctx);
        self.match_assign_sides(loc, &lhs_paths, &rhs_paths, ctx)
    }

    fn match_assign_sides(
        &mut self,
        loc: Loc,
        lhs_paths: &ExprRet,
        rhs_paths: &ExprRet,
        ctx: ContextNode,
    ) -> ExprRet {
        match (lhs_paths, rhs_paths) {
            (ExprRet::Single((_lhs_ctx, lhs)), ExprRet::Single((rhs_ctx, rhs))) => {
                let lhs_cvar = ContextVarNode::from(*lhs);
                let rhs_cvar = ContextVarNode::from(*rhs);
                self.assign(loc, lhs_cvar, rhs_cvar, *rhs_ctx)
            }
            (l @ ExprRet::Single((_lhs_ctx, _lhs)), ExprRet::Multi(rhs_sides)) => ExprRet::Multi(
                rhs_sides
                    .iter()
                    .map(|expr_ret| self.match_assign_sides(loc, l, expr_ret, ctx))
                    .collect(),
            ),
            (ExprRet::Multi(lhs_sides), r @ ExprRet::Single(_)) => ExprRet::Multi(
                lhs_sides
                    .iter()
                    .map(|expr_ret| self.match_assign_sides(loc, expr_ret, r, ctx))
                    .collect(),
            ),
            (ExprRet::Multi(lhs_sides), ExprRet::Multi(rhs_sides)) => {
                // try to zip sides if they are the same length
                if lhs_sides.len() == rhs_sides.len() {
                    ExprRet::Multi(
                        lhs_sides
                            .iter()
                            .zip(rhs_sides.iter())
                            .map(|(lhs_expr_ret, rhs_expr_ret)| {
                                self.match_assign_sides(loc, lhs_expr_ret, rhs_expr_ret, ctx)
                            })
                            .collect(),
                    )
                } else {
                    ExprRet::Multi(
                        rhs_sides
                            .iter()
                            .map(|rhs_expr_ret| {
                                self.match_assign_sides(loc, lhs_paths, rhs_expr_ret, ctx)
                            })
                            .collect(),
                    )
                }
            }
            (ExprRet::Fork(lhs_world1, lhs_world2), ExprRet::Fork(rhs_world1, rhs_world2)) => {
                ExprRet::Fork(
                    Box::new(ExprRet::Fork(
                        Box::new(self.match_assign_sides(loc, lhs_world1, rhs_world1, ctx)),
                        Box::new(self.match_assign_sides(loc, lhs_world1, rhs_world2, ctx)),
                    )),
                    Box::new(ExprRet::Fork(
                        Box::new(self.match_assign_sides(loc, lhs_world2, rhs_world1, ctx)),
                        Box::new(self.match_assign_sides(loc, lhs_world2, rhs_world2, ctx)),
                    )),
                )
            }
            (l @ ExprRet::Single(_), ExprRet::Fork(world1, world2)) => ExprRet::Fork(
                Box::new(self.match_assign_sides(loc, l, world1, ctx)),
                Box::new(self.match_assign_sides(loc, l, world2, ctx)),
            ),
            (m @ ExprRet::Multi(_), ExprRet::Fork(world1, world2)) => ExprRet::Fork(
                Box::new(self.match_assign_sides(loc, m, world1, ctx)),
                Box::new(self.match_assign_sides(loc, m, world2, ctx)),
            ),
            (e, f) => todo!("any: {:?} {:?}", e, f),
        }
    }

    fn assign(
        &mut self,
        loc: Loc,
        lhs_cvar: ContextVarNode,
        rhs_cvar: ContextVarNode,
        ctx: ContextNode,
    ) -> ExprRet {
        let (new_lower_bound, new_upper_bound) = if let Some(range) = rhs_cvar.range(self) {
            (range.range_min(), range.range_max())
        } else {
            if let Some(range) = lhs_cvar.range(self) {
                (
                    Dynamic::new(rhs_cvar.into(), DynSide::Min, loc).into(),
                    Dynamic::new(rhs_cvar.into(), DynSide::Max, loc).into(),
                )
            } else {
                panic!("in assign, both lhs and rhs had no range")
            }
        };

        let new_lhs = self.advance_var_in_ctx(lhs_cvar, loc, ctx);
        new_lhs.set_range_min(self, new_lower_bound.into());
        new_lhs.set_range_max(self, new_upper_bound.into());

        ExprRet::Single((ctx, new_lhs.into()))
    }

    fn advance_var_in_ctx(
        &mut self,
        cvar_node: ContextVarNode,
        loc: Loc,
        ctx: ContextNode,
    ) -> ContextVarNode {
        println!(
            "advancing: {} in {}",
            cvar_node.display_name(self),
            ctx.underlying(self).path
        );
        let mut new_cvar = cvar_node.underlying(self).clone();
        new_cvar.loc = Some(loc);
        let new_cvarnode = self.add_node(Node::ContextVar(new_cvar));
        if let Some(old_ctx) = cvar_node.maybe_ctx(self) {
            if old_ctx != ctx {
                self.add_edge(new_cvarnode, ctx, Edge::Context(ContextEdge::Variable));
            } else {
                self.add_edge(new_cvarnode, cvar_node.0, Edge::Context(ContextEdge::Prev));
            }
        } else {
            self.add_edge(new_cvarnode, cvar_node.0, Edge::Context(ContextEdge::Prev));
        }

        ContextVarNode::from(new_cvarnode)
    }

    fn advance_var_underlying(&mut self, cvar_node: ContextVarNode, loc: Loc) -> &mut ContextVar {
        let mut new_cvar = cvar_node.underlying(self).clone();
        new_cvar.loc = Some(loc);
        let new_cvarnode = self.add_node(Node::ContextVar(new_cvar));
        self.add_edge(new_cvarnode, cvar_node.0, Edge::Context(ContextEdge::Prev));
        ContextVarNode::from(new_cvarnode).underlying_mut(self)
    }
}
