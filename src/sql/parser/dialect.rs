//! Oracle-compatible parsing with a guard at the upstream infix growth boundary.
use sqlparser::{
    ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments},
    dialect::{Dialect, OracleDialect},
    keywords::Keyword,
    parser::{Parser, ParserError},
};
use std::any::TypeId;

#[derive(Debug)]
pub(super) struct BoundedOracle {
    pub(super) max_depth: usize,
}

impl Dialect for BoundedOracle {
    fn dialect(&self) -> TypeId {
        TypeId::of::<OracleDialect>()
    }
    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        OracleDialect.identifier_quote_style(identifier)
    }
    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        OracleDialect.is_delimited_identifier_start(ch)
    }
    fn is_identifier_start(&self, ch: char) -> bool {
        OracleDialect.is_identifier_start(ch)
    }
    fn is_identifier_part(&self, ch: char) -> bool {
        OracleDialect.is_identifier_part(ch)
    }
    fn supports_outer_join_operator(&self) -> bool {
        true
    }
    fn supports_connect_by(&self) -> bool {
        true
    }
    fn supports_execute_immediate(&self) -> bool {
        true
    }
    fn supports_match_recognize(&self) -> bool {
        true
    }
    fn supports_window_function_null_treatment_arg(&self) -> bool {
        true
    }
    fn supports_boolean_literals(&self) -> bool {
        false
    }
    fn supports_comment_on(&self) -> bool {
        true
    }
    fn supports_create_table_select(&self) -> bool {
        true
    }
    fn supports_set_stmt_without_operator(&self) -> bool {
        true
    }
    fn supports_group_by_expr(&self) -> bool {
        true
    }
    fn supports_quote_delimited_string(&self) -> bool {
        true
    }
    fn supports_comment_optimizer_hint(&self) -> bool {
        true
    }
    fn supports_insert_table_alias(&self) -> bool {
        true
    }
    fn supports_insert_table_query(&self) -> bool {
        true
    }
    fn get_reserved_keywords_for_select_item_operator(&self) -> &[Keyword] {
        OracleDialect.get_reserved_keywords_for_select_item_operator()
    }
    fn get_next_precedence(&self, parser: &Parser) -> Option<Result<u8, ParserError>> {
        OracleDialect.get_next_precedence(parser)
    }

    fn parse_infix(
        &self,
        _parser: &mut Parser,
        expr: &Expr,
        _precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        // Upstream's Pratt loop does not count left-associative tree growth as
        // recursion. Refuse the next growth while its owned left input is still
        // bounded, including on syntax-error cleanup inside the upstream parser.
        match check_infix_depth(expr, self.max_depth) {
            Ok(()) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

fn check_infix_depth(root: &Expr, limit: usize) -> Result<(), ParserError> {
    let mut work = vec![(root, 1)];
    while let Some((expr, depth)) = work.pop() {
        if depth >= limit {
            return Err(ParserError::RecursionLimitExceeded);
        }
        let next = depth + 1;
        match expr {
            Expr::BinaryOp { left, right, .. } => {
                work.push((left, next));
                work.push((right, next));
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr) => work.push((expr, next)),
            Expr::InList { expr, list, .. } => {
                work.push((expr, next));
                work.extend(list.iter().map(|expr| (expr, next)));
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                work.push((expr, next));
                work.push((low, next));
                work.push((high, next));
            }
            Expr::Like { expr, pattern, .. } => {
                work.push((expr, next));
                work.push((pattern, next));
            }
            Expr::Function(function) => {
                if !matches!(function.parameters, FunctionArguments::None)
                    || function.filter.is_some()
                    || function.over.is_some()
                    || !function.within_group.is_empty()
                {
                    return Err(unsupported_infix_input());
                }
                if let FunctionArguments::List(list) = &function.args {
                    if !list.clauses.is_empty() {
                        return Err(unsupported_infix_input());
                    }
                    for arg in &list.args {
                        match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                                work.push((expr, next))
                            }
                            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {}
                            _ => return Err(unsupported_infix_input()),
                        }
                    }
                } else if !matches!(function.args, FunctionArguments::None) {
                    return Err(unsupported_infix_input());
                }
            }
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => {}
            // Do not allow an unsupported expression (e.g. a chain of casts)
            // to become an unmeasured subtree of a supported infix operator.
            _ => {
                return Err(unsupported_infix_input());
            }
        }
    }
    Ok(())
}

fn unsupported_infix_input() -> ParserError {
    ParserError::ParserError("ORA-SQLLite unsupported infix input".to_owned())
}
