use std::collections::HashSet;

use arroyo_datastream::WindowType;
use arroyo_rpc::TIMESTAMP_FIELD;
use datafusion_common::{
    plan_err,
    tree_node::{TreeNode, TreeNodeRewriter, TreeNodeVisitor, VisitRecursion},
    Column, DFField, DataFusionError, Result as DFResult,
};

use aggregate::AggregateRewriter;
use datafusion_expr::{expr::Alias, Aggregate, Expr, Extension, LogicalPlan};
use join::JoinRewriter;

use crate::rewriters::AsyncUdfRewriter;
use crate::{
    extension::{
        aggregate::{AggregateExtension, AGGREGATE_EXTENSION_NAME},
        join::JOIN_NODE_NAME,
    },
    find_window,
    rewriters::SourceRewriter,
    schemas::{add_timestamp_field, has_timestamp_field},
    ArroyoSchemaProvider, WindowBehavior,
};

use self::window_fn::WindowFunctionRewriter;

mod aggregate;
mod join;
mod window_fn;

#[derive(Debug, Default)]
struct WindowDetectingVisitor {
    window: Option<WindowType>,
    fields: HashSet<DFField>,
}

impl WindowDetectingVisitor {
    fn get_window(logical_plan: &LogicalPlan) -> DFResult<Option<WindowType>> {
        let mut visitor = WindowDetectingVisitor {
            window: None,
            fields: HashSet::new(),
        };
        logical_plan.visit(&mut visitor)?;
        Ok(visitor.window.take())
    }
}

fn extract_column(expr: &Expr) -> Option<&Column> {
    match expr {
        Expr::Column(column) => Some(column),
        Expr::Alias(Alias { expr, .. }) => extract_column(expr),
        _ => None,
    }
}

impl TreeNodeVisitor for WindowDetectingVisitor {
    type N = LogicalPlan;

    fn post_visit(&mut self, node: &Self::N) -> DFResult<VisitRecursion> {
        match node {
            LogicalPlan::Projection(projection) => {
                let window_expressions = projection
                    .expr
                    .iter()
                    .enumerate()
                    .filter_map(|(index, expr)| {
                        if let Some(column) = extract_column(expr) {
                            let input_field = projection
                                .input
                                .schema()
                                .field_with_name(column.relation.as_ref(), &column.name);
                            let input_field = match input_field {
                                Ok(field) => field,
                                Err(err) => {
                                    return Some(Err(err));
                                }
                            };
                            if self.fields.contains(input_field) {
                                return self.window.clone().map(|window| Ok((index, window)));
                            }
                        }
                        find_window(expr)
                            .map(|option| option.map(|inner| (index, inner)))
                            .map_err(|err| DataFusionError::Plan(err.to_string()))
                            .transpose()
                    })
                    .collect::<DFResult<Vec<_>>>()?;
                self.fields.clear();
                for (index, window) in window_expressions {
                    // if there's already a window they should match
                    if let Some(existing_window) = &self.window {
                        if *existing_window != window {
                            return plan_err!(
                                "can't window by both {:?} and {:?}",
                                existing_window,
                                window
                            );
                        }
                        self.fields.insert(projection.schema.field(index).clone());
                    } else {
                        // If the input doesn't have an input window, we shouldn't be creating a window.
                        return plan_err!(
                            "can't call a windowing function without grouping by it in an aggregate"
                        );
                    }
                }
            }
            LogicalPlan::SubqueryAlias(subquery_alias) => {
                // translate the fields to the output schema
                self.fields = self
                    .fields
                    .drain()
                    .map(|field| {
                        Ok(subquery_alias
                            .schema
                            .field(
                                subquery_alias
                                    .input
                                    .schema()
                                    .index_of_column(&field.qualified_column())?,
                            )
                            .clone())
                    })
                    .collect::<DFResult<HashSet<_>>>()?;
            }
            LogicalPlan::Aggregate(Aggregate {
                input,
                group_expr,
                aggr_expr: _,
                schema,
                ..
            }) => {
                let window_expressions = group_expr
                    .iter()
                    .enumerate()
                    .filter_map(|(index, expr)| {
                        if let Some(column) = extract_column(expr) {
                            let input_field = input
                                .schema()
                                .field_with_name(column.relation.as_ref(), &column.name);
                            let input_field = match input_field {
                                Ok(field) => field,
                                Err(err) => {
                                    return Some(Err(err));
                                }
                            };
                            if self.fields.contains(input_field) {
                                return self.window.clone().map(|window| Ok((index, window)));
                            }
                        }
                        find_window(expr)
                            .map(|option| option.map(|inner| (index, inner)))
                            .map_err(|err| DataFusionError::Plan(err.to_string()))
                            .transpose()
                    })
                    .collect::<DFResult<Vec<_>>>()?;
                self.fields.clear();
                for (index, window) in window_expressions {
                    // if there's already a window they should match
                    if let Some(existing_window) = &self.window {
                        if *existing_window != window {
                            return Err(DataFusionError::Plan(
                                "window expressions do not match".to_string(),
                            ));
                        }
                    } else {
                        self.window = Some(window);
                    }
                    self.fields.insert(schema.field(index).clone());
                }
            }
            LogicalPlan::Extension(Extension { node }) => match node.name() {
                AGGREGATE_EXTENSION_NAME => {
                    let aggregate_extension = node
                        .as_any()
                        .downcast_ref::<AggregateExtension>()
                        .expect("should be aggregate extension");

                    match &aggregate_extension.window_behavior {
                        WindowBehavior::FromOperator {
                            window,
                            window_field,
                            window_index: _,
                            is_nested,
                        } => {
                            if self.window.is_some() && !*is_nested {
                                return Err(DataFusionError::Plan(
                                    "aggregate node should not be recalculating window, as input is windowed.".to_string(),
                                ));
                            }
                            self.window = Some(window.clone());
                            self.fields.insert(window_field.clone());
                        }
                        WindowBehavior::InData => {
                            let input_fields = self.fields.clone();
                            self.fields.clear();
                            for field in node.schema().fields() {
                                if input_fields.contains(field) {
                                    self.fields.insert(field.clone());
                                }
                            }
                            if self.fields.is_empty() {
                                return Err(DataFusionError::Plan(
                                    "must have window in aggregate. Make sure you are calling one of the windowing functions (hop, tumble, session) or using the window field of the input".to_string(),
                                ));
                            }
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(VisitRecursion::Continue)
    }

    fn pre_visit(&mut self, node: &Self::N) -> DFResult<VisitRecursion> {
        let LogicalPlan::Extension(Extension { node }) = node else {
            return Ok(VisitRecursion::Continue);
        };
        match node.name() {
            // handle Join in the pre-join, as each side needs to be checked separately.
            JOIN_NODE_NAME => {
                let input_windows: HashSet<_> = node
                    .inputs()
                    .iter()
                    .map(|input| Self::get_window(input))
                    .collect::<DFResult<HashSet<_>>>()?;
                if input_windows.len() > 1 {
                    return Err(DataFusionError::Plan(
                        "can't handle mixed windowing between left and right".to_string(),
                    ));
                }
                self.window = input_windows
                    .into_iter()
                    .next()
                    .expect("join has at least one input");
                return Ok(VisitRecursion::Skip);
            }
            _ => {}
        }
        Ok(VisitRecursion::Continue)
    }
}

// This is one rewriter so that we can rely on inputs having already been rewritten
// ensuring they have _timestamp field, amongst other things.
pub struct ArroyoRewriter<'a> {
    pub(crate) schema_provider: &'a ArroyoSchemaProvider,
}

impl<'a> TreeNodeRewriter for ArroyoRewriter<'a> {
    type N = LogicalPlan;

    fn mutate(&mut self, mut node: Self::N) -> DFResult<Self::N> {
        match node {
            LogicalPlan::Projection(ref mut projection) => {
                if !has_timestamp_field(&projection.schema) {
                    let timestamp_field = projection
                        .input
                        .schema()
                        .fields_with_unqualified_name(TIMESTAMP_FIELD).first().cloned().ok_or_else(|| {
                            DataFusionError::Plan("No timestamp field found in projection input. Query should've been rewritten".to_string())
                        })?;
                    projection.schema = add_timestamp_field(
                        projection.schema.clone(),
                        timestamp_field.qualifier().cloned(),
                    )
                    .expect("in projection");
                    projection.expr.push(Expr::Column(Column {
                        relation: timestamp_field.qualifier().cloned(),
                        name: "_timestamp".to_string(),
                    }));
                }

                return AsyncUdfRewriter::new(self.schema_provider).mutate(node);
            }
            LogicalPlan::Aggregate(aggregate) => {
                return AggregateRewriter {}.mutate(LogicalPlan::Aggregate(aggregate));
            }
            LogicalPlan::Join(join) => {
                return JoinRewriter {}.mutate(LogicalPlan::Join(join));
            }
            LogicalPlan::TableScan(table_scan) => {
                return SourceRewriter {
                    schema_provider: self.schema_provider,
                }
                .mutate(LogicalPlan::TableScan(table_scan));
            }
            LogicalPlan::Filter(_) => {}
            LogicalPlan::Window(_) => {
                return WindowFunctionRewriter {}.mutate(node);
            }
            LogicalPlan::Sort(_) => {
                return plan_err!("ORDER BY is not currently supported ({})", node.display());
            }
            LogicalPlan::CrossJoin(_) => {
                return plan_err!("CROSS JOIN is not currently supported ({})", node.display());
            }
            LogicalPlan::Repartition(_) => {
                return plan_err!(
                    "Repartitions are not currently supported ({})",
                    node.display()
                );
            }
            LogicalPlan::Union(_) => {}
            LogicalPlan::EmptyRelation(_) => {}
            LogicalPlan::Subquery(_) => {}
            LogicalPlan::SubqueryAlias(_) => {}
            LogicalPlan::Limit(_) => {
                return plan_err!("LIMIT is not currently supported ({})", node.display());
            }
            LogicalPlan::Statement(s) => {
                return plan_err!("Unsupported statement: {}", s.display());
            }
            LogicalPlan::Values(_) => {}
            LogicalPlan::Explain(_) => {
                return plan_err!("EXPLAIN is not supported ({})", node.display());
            }
            LogicalPlan::Analyze(_) => {
                return plan_err!("ANALYZE is not supported ({})", node.display());
            }
            LogicalPlan::Extension(_) => {}
            LogicalPlan::Distinct(_) => {}
            LogicalPlan::Prepare(_) => {
                return plan_err!("Prepared statements are not supported ({})", node.display())
            }
            LogicalPlan::Dml(_) => {}
            LogicalPlan::Ddl(_) => {}
            LogicalPlan::Copy(_) => {
                return plan_err!("COPY is not supported ({})", node.display());
            }
            LogicalPlan::DescribeTable(_) => {
                return plan_err!("DESCRIBE is not supported ({})", node.display());
            }
            LogicalPlan::Unnest(_) => {}
            LogicalPlan::RecursiveQuery(_) => {
                return plan_err!("Recursive CTEs are not supported ({})", node.display());
            }
        }
        Ok(node)
    }
}
