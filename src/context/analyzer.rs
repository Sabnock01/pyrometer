use solang_parser::pt::Loc;
use crate::Range;
use crate::ContextVarNode;
use petgraph::visit::EdgeRef;
use petgraph::{Direction, Directed, graph::*};
use petgraph::graph::Edges;
use crate::ContextNode;
use crate::Edge;
use crate::AnalyzerLike;
use crate::Concrete;
use crate::NodeIdx;
use crate::VarType;
use crate::Node;
use crate::ContextEdge;
use std::collections::{BTreeSet, BTreeMap};
use ariadne::{Report, ReportKind, Label, Source, Span, ColorGenerator, Color};

#[derive(Debug, Copy, Clone)]
pub enum Relative {
	Eq,
	Lt,
	Lte,
	Gt,
	Gte
}

impl ToString for Relative {
	fn to_string(&self) -> String {
		use Relative::*;
		match self {
			Eq => "==".to_string(),
			Lt => "<".to_string(),
			Lte => "<=".to_string(),
			Gt => ">".to_string(),
			Gte => ">=".to_string()
		}
	}
}

#[derive(Debug, Clone)]
pub enum RelativeTarget {
	Concrete(Concrete),
	Dynamic(NodeIdx),
}

#[derive(Debug, Clone)]
pub enum Analysis {
	Relative(Relative, RelativeTarget),
}

impl Analysis {
	pub fn relative_string(&self) -> String {
		match self {
			Analysis::Relative(rel, _) => rel.to_string()
		}
	}

	pub fn relative_target_string(&self, analyzer: &impl AnalyzerLike) -> String {
		match self {
			Analysis::Relative(_, target) => {
				match target {
					RelativeTarget::Concrete(concrete) => {
						match concrete {
							Concrete::Uint(_, val) => val.to_string(),
							Concrete::Int(_, val) => val.to_string(),
							_ => panic!("non-number bound")
						}
					}
					RelativeTarget::Dynamic(idx) => {
						let as_var = ContextVarNode::from(*idx);
						let name = as_var.name(analyzer);
						if let Some(range) = as_var.range(analyzer) {
							format!("\"{}\"\n \"{}\" has the bounds: {:?} to {:?}", name, name, range.min, range.max)
						} else {
							format!("{}", name)
						}
					}
				}
			}
		}
	}
}

#[derive(Debug, Clone)]
pub enum ArrayAccess {
	MinSize,
	MaxSize
}

#[derive(Debug, Clone)]
pub struct ArrayAccessAnalysis {
	pub arr_def: ContextVarNode,
	pub arr_loc: LocSpan,
	pub access_loc: LocSpan,
	pub analysis: Analysis,
	pub analysis_ty: ArrayAccess,
}

#[derive(Debug, Copy, Clone)]
pub struct LocSpan(pub Loc);

impl Span for LocSpan {
	type SourceId = usize;
	fn source(&self) -> &Self::SourceId {
		match self.0 {
			Loc::File(ref f, _, _) => f,
			_ => todo!("handle non file loc")
		}
	}

	fn start(&self) -> usize {
		match self.0 {
			Loc::File(_, start, _) => start,
			_ => todo!("handle non file loc")
		}
	}

	fn end(&self) -> usize {
		match self.0 {
			Loc::File(_, _, end) => end,
			_ => todo!("handle non file loc")
		}
	}
}

pub trait ReportDisplay {
	fn report_kind(&self) -> ReportKind;
	fn msg(&self, analyzer: &(impl AnalyzerLike + Search)) -> String;
	fn labels(&self, analyzer: &(impl AnalyzerLike + Search)) -> Vec<Label<LocSpan>>;
	fn report(&self, analyzer: &(impl AnalyzerLike + Search)) -> Report<LocSpan>;
	fn print_report(&self, src: (usize, &str), analyzer: &(impl AnalyzerLike + Search));
}

impl ReportDisplay for ArrayAccessAnalysis {
	fn report_kind(&self) -> ReportKind {
		ReportKind::Advice
	}
	fn msg(&self, analyzer: &impl AnalyzerLike) -> String {
		match self.analysis_ty {
			ArrayAccess::MinSize => format!("Minimum array length: length must be {} {}", self.analysis.relative_string(), self.analysis.relative_target_string(analyzer)),
			ArrayAccess::MaxSize => "Maximum array length: length must be {}{}".to_string(),
		}
	}
	fn labels(&self, _analyzer: &impl AnalyzerLike) -> Vec<Label<LocSpan>> {
		vec![
			Label::new(self.arr_loc)
				.with_message("Array accessed here")
				.with_color(Color::Green),
			Label::new(self.access_loc)
				.with_message("Length enforced by this")
				.with_color(Color::Cyan)
		]
	}
	fn report(&self, analyzer: &(impl AnalyzerLike + Search)) -> Report<LocSpan> {
		let mut report = Report::build(self.report_kind(), *self.arr_loc.source(), self.arr_loc.start())
			.with_message(self.msg(analyzer));
		
		for label in self.labels(analyzer).into_iter() {
			report = report.with_label(label);
		}

		report.finish()
	}
	fn print_report(&self, src: (usize, &str), analyzer: &(impl AnalyzerLike + Search)) {
		let report = self.report(analyzer);
		report.print((src.0, Source::from(src.1))).unwrap()
	}
}

pub trait ContextAnalyzer: AnalyzerLike + Search + ArrayAccessAnalyzer {}


pub trait Search: AnalyzerLike {
	fn search_for_ancestor(&self, start: NodeIdx, edge_ty: &Edge) -> Option<NodeIdx> {
		let edges = self.graph().edges_directed(start, Direction::Outgoing);
		if let Some(edge) = edges.clone().find(|edge| edge.weight() == edge_ty) {
			Some(edge.target())
		} else {
			edges.map(|edge| edge.target())
				.filter_map(|node| self.search_for_ancestor(node, edge_ty))
				.take(1)
				.next()
		}
	}
	/// Finds any child nodes that have some edge `edge_ty` incoming. Builds up a set of these
	/// 
	/// i.e.: a -my_edge-> b -other_edge-> c -my_edge-> d
	///
	/// This function would build a set { b, d } if we are looking for `my_edge` and start at a.
	fn search_children(&self, start: NodeIdx, edge_ty: &Edge) -> BTreeSet<NodeIdx> {
		let edges = self.graph().edges_directed(start, Direction::Incoming);
		let mut this_children: BTreeSet<NodeIdx> = edges.clone().filter_map(|edge| {
				if edge.weight() == edge_ty {
					Some(edge.source())
				} else {
					None
				}
			}).collect();

		this_children.extend(edges.flat_map(|edge| self.search_children(edge.source(), edge_ty)).collect::<BTreeSet<NodeIdx>>());
		this_children
	}

	/// Finds any child nodes that have some edge `edge_ty` incoming. Builds up a mapping of these
	/// 
	/// i.e.: a -my_edge-> b -other_edge-> c -my_edge-> d
	///
	/// This function would build a map { a: [b], c: [d] } if we are looking for `my_edge` and start at a.
	fn nodes_with_children(&self, start: NodeIdx, edge_ty: &Edge) -> Option<BTreeMap<NodeIdx, BTreeSet<NodeIdx>>> {
		let edges = self.graph().edges_directed(start, Direction::Incoming);
		let mut map: BTreeMap<NodeIdx, BTreeSet<NodeIdx>> = Default::default();

		let this_children: BTreeSet<NodeIdx> = edges.clone().filter_map(|edge| {
				if edge.weight() == edge_ty {
					Some(edge.source())
				} else {
					None
				}
			}).collect();
		
		if !this_children.is_empty() {
			map.insert(start, this_children);
		}
		map.extend(edges.filter_map(|edge| self.nodes_with_children(edge.source(), edge_ty)).flatten().collect::<BTreeMap<NodeIdx, BTreeSet<NodeIdx>>>());
		if map.is_empty() {
			None
		} else {
			Some(map)
		}
	}
}

pub trait ArrayAccessAnalyzer: Search + AnalyzerLike + Sized {
	fn min_size_to_prevent_access_revert(&self, ctx: ContextNode) -> Vec<ArrayAccessAnalysis> {
		let mut analyses = Default::default();

		if let Some(arrays) = self.nodes_with_children(ctx.into(), &Edge::Context(ContextEdge::IndexAccess)) {
			analyses = arrays.iter().flat_map(|(array, accesses)| {
				accesses.iter().map(|access| {
					let cvar_idx = *self.search_children(*access, &Edge::Context(ContextEdge::Index)).iter().take(1).next().expect("IndexAccess without Index");
					let cvar = ContextVarNode::from(cvar_idx).underlying(self);
					match &cvar.ty {
						VarType::Concrete(conc_node) => {
							// its a concrete index, the analysis should be a Gt the concrete value
							match conc_node.underlying(self) {
							    c @ &Concrete::Uint(..) => {
							    	ArrayAccessAnalysis {
										arr_def: ContextVarNode::from(*array),
										arr_loc: LocSpan(ContextVarNode::from(*array).loc(self)),
										access_loc: LocSpan(cvar.loc.expect("No loc for access")),
								    	analysis: Analysis::Relative(Relative::Gt, RelativeTarget::Concrete(c.clone())),
								    	analysis_ty: ArrayAccess::MinSize,
								    }
							    },
							    e => panic!("Attempt to index into an array with a {:?}", e)
							}
						}
						VarType::BuiltIn(_bn, maybe_range) => {
							// its a variable index, the analysis should be a Gt the variable
							// the range will tell us more about the actual bounds
							if let Some(_range) = maybe_range {
								ArrayAccessAnalysis {
									arr_def: ContextVarNode::from(*array),
									arr_loc: LocSpan(ContextVarNode::from(*array).loc(self)),
									access_loc: LocSpan(cvar.loc.expect("No loc for access")),
									analysis: Analysis::Relative(Relative::Gt, RelativeTarget::Dynamic(cvar_idx)),
									analysis_ty: ArrayAccess::MinSize,
								}
							} else {
								ArrayAccessAnalysis {
									arr_def: ContextVarNode::from(*array),
									arr_loc: LocSpan(ContextVarNode::from(*array).loc(self)),
									access_loc: LocSpan(cvar.loc.expect("No loc for access")),
									analysis: Analysis::Relative(Relative::Gt, RelativeTarget::Dynamic(*access)),
									analysis_ty: ArrayAccess::MinSize,
								}
							}
						},
						e => panic!("Attempt to index into an array with a {:?}", e)
						
					}
				}).collect::<Vec<ArrayAccessAnalysis>>()
			}).collect();
		}

		analyses
	}

	fn max_size_to_prevent_access_revert(&self, ctx: ContextNode) -> BTreeMap<NodeIdx, Vec<Analysis>> {
		todo!()
	}
}

pub trait JumpAnalyzer: AnalyzerLike {}