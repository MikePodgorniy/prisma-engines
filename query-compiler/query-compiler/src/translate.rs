mod query;

use itertools::Itertools;
use query::translate_query;
use query_builder::QueryBuilder;
use query_core::{EdgeRef, Node, NodeRef, Query, QueryGraph, QueryGraphBuilderError, QueryGraphDependency};
use query_structure::{PlaceholderType, PrismaValue, SelectionResult};
use thiserror::Error;

use super::expression::{Binding, Expression};

#[derive(Debug, Error)]
pub enum TranslateError {
    #[error("node {0} has no content")]
    NodeContentEmpty(String),

    #[error("query builder error: {0}")]
    QueryBuildFailure(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("query graph build error: {0}")]
    GraphBuildError(#[from] QueryGraphBuilderError),
}

pub type TranslateResult<T> = Result<T, TranslateError>;

pub fn translate(mut graph: QueryGraph, builder: &dyn QueryBuilder) -> TranslateResult<Expression> {
    graph
        .root_nodes()
        .into_iter()
        .map(|node| NodeTranslator::new(&mut graph, node, &[], builder).translate())
        .collect::<TranslateResult<Vec<_>>>()
        .map(Expression::Seq)
}

struct NodeTranslator<'a, 'b> {
    graph: &'a mut QueryGraph,
    node: NodeRef,
    #[allow(dead_code)]
    parent_edges: &'b [EdgeRef],
    query_builder: &'b dyn QueryBuilder,
}

impl<'a, 'b> NodeTranslator<'a, 'b> {
    fn new(
        graph: &'a mut QueryGraph,
        node: NodeRef,
        parent_edges: &'b [EdgeRef],
        query_builder: &'b dyn QueryBuilder,
    ) -> Self {
        Self {
            graph,
            node,
            parent_edges,
            query_builder,
        }
    }

    fn translate(&mut self) -> TranslateResult<Expression> {
        let node = self
            .graph
            .node_content(&self.node)
            .ok_or_else(|| TranslateError::NodeContentEmpty(self.node.id()))?;

        match node {
            Node::Query(_) => self.translate_query(),
            // might be worth having Expression::Unit for this?
            Node::Empty => Ok(Expression::Seq(vec![])),
            n => unimplemented!("{:?}", std::mem::discriminant(n)),
        }
    }

    fn translate_query(&mut self) -> TranslateResult<Expression> {
        self.graph.mark_visited(&self.node);

        // Don't recurse into children if the current node is already a result node.
        let children = if !self.graph.is_result_node(&self.node) {
            self.process_children()?
        } else {
            Vec::new()
        };

        let mut node = self.graph.pluck_node(&self.node);

        for edge in self.parent_edges {
            match self.graph.pluck_edge(edge) {
                QueryGraphDependency::ExecutionOrder => {}
                QueryGraphDependency::ProjectedDataDependency(selection, f) => {
                    let fields = selection
                        .selections()
                        .map(|field| {
                            (
                                field.clone(),
                                PrismaValue::Placeholder {
                                    name: self.graph.edge_source(edge).id(),
                                    r#type: PlaceholderType::Any,
                                },
                            )
                        })
                        .collect_vec();

                    // TODO: there are cases where we look at the number of results in some
                    // dependencies, these won't work with the current implementation and will
                    // need to be re-implemented
                    node = f(node, vec![SelectionResult::new(fields)])?;
                }
                // TODO: implement data dependencies and if/else
                QueryGraphDependency::DataDependency(_) => todo!(),
                QueryGraphDependency::Then => todo!(),
                QueryGraphDependency::Else => todo!(),
            };
        }

        let query: Query = node.try_into().expect("current node must be query");
        let expr = translate_query(query, self.query_builder)?;

        if !children.is_empty() {
            Ok(Expression::Let {
                bindings: vec![Binding::new(self.node.id(), expr)],
                expr: Box::new(Expression::Seq(children)),
            })
        } else {
            Ok(expr)
        }
    }

    fn process_children(&mut self) -> TranslateResult<Vec<Expression>> {
        let mut child_pairs = self.graph.direct_child_pairs(&self.node);

        // Find the positions of all result returning graph nodes.
        let mut result_positions = child_pairs
            .iter()
            .enumerate()
            .filter_map(|(idx, (_, child_node))| {
                if self.graph.subgraph_contains_result(child_node) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Start removing the highest indices first to not invalidate subsequent removals.
        result_positions.sort_unstable();
        result_positions.reverse();

        let result_subgraphs = result_positions
            .into_iter()
            .map(|pos| child_pairs.remove(pos))
            .collect::<Vec<_>>();

        // Because we split from right to left, everything remaining in `child_pairs`
        // doesn't belong into results, and is executed before all result scopes.
        let mut expressions: Vec<Expression> = child_pairs
            .into_iter()
            .map(|(edge, node)| self.process_child_with_dependency(edge, node))
            .collect::<Result<Vec<_>, _>>()?;

        // Fold result scopes into one expression.
        if !result_subgraphs.is_empty() {
            let result_exp = self.fold_result_scopes(result_subgraphs)?;
            expressions.push(result_exp);
        }

        Ok(expressions)
    }

    fn fold_result_scopes(&mut self, result_subgraphs: Vec<(EdgeRef, NodeRef)>) -> TranslateResult<Expression> {
        // if the subgraphs all point to the same result node, we fold them in sequence
        // if not, we can separate them with a getfirstnonempty
        let bindings = result_subgraphs
            .into_iter()
            .map(|(edge, node)| {
                let name = node.id();
                let expr = self.process_child_with_dependency(edge, node)?;
                Ok(Binding { name, expr })
            })
            .collect::<TranslateResult<Vec<_>>>()?;

        let result_nodes = self.graph.result_nodes();
        let result_binding_names = bindings.iter().map(|b| b.name.clone()).collect::<Vec<_>>();

        if result_nodes.len() == 1 {
            Ok(Expression::Let {
                bindings,
                expr: Box::new(Expression::Get {
                    name: result_binding_names
                        .into_iter()
                        .last()
                        .expect("no binding for result node"),
                }),
            })
        } else {
            Ok(Expression::Let {
                bindings,
                expr: Box::new(Expression::GetFirstNonEmpty {
                    names: result_binding_names,
                }),
            })
        }
    }

    fn process_child_with_dependency(&mut self, edge: EdgeRef, node: NodeRef) -> TranslateResult<Expression> {
        let edge_content = self.graph.edge_content(&edge);
        let field = if let Some(QueryGraphDependency::ProjectedDataDependency(selection, _)) = edge_content {
            let mut fields = selection.selections();
            if let Some(first) = fields.next().filter(|_| fields.len() == 0) {
                Some(first.db_name().to_string())
            } else {
                // we need to handle MapField with multiple fields?
                todo!()
            }
        } else {
            None
        };

        // translate plucks the edges coming into node, we need to avoid accessing it afterwards
        let edges = self.graph.incoming_edges(&node);
        let source = self.graph.edge_source(&edge);
        let expr = NodeTranslator::new(self.graph, node, &edges, self.query_builder).translate()?;

        // we insert a MapField expression if the edge was a projected data dependency
        if let Some(field) = field {
            Ok(Expression::Let {
                bindings: vec![Binding::new(
                    source.id(),
                    Expression::MapField {
                        field,
                        records: Box::new(Expression::Get { name: source.id() }),
                    },
                )],
                expr: Box::new(expr),
            })
        } else {
            Ok(expr)
        }
    }
}
