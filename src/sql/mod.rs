#[allow(dead_code, reason = "later SQL frontend layers consume the owned AST")]
mod ast;
#[allow(
    dead_code,
    unused_imports,
    reason = "later Phase 4B binders and Phase 4C execution consume the bound IR"
)]
pub(crate) mod bound;
mod limits;
#[allow(
    dead_code,
    reason = "later SQL frontend layers consume checked name primitives"
)]
mod name;
#[allow(
    dead_code,
    reason = "later SQL frontend layers consume source-mapped parser tokens"
)]
mod parser;
#[allow(
    dead_code,
    reason = "later SQL frontend layers consume checked type primitives"
)]
mod types;

pub use limits::SqlLimits;
#[allow(
    unused_imports,
    reason = "later SQL frontend layers import these shared name primitives"
)]
pub(crate) use name::{Identifier, ParameterKey, ParameterStyle};
#[allow(
    unused_imports,
    reason = "later SQL frontend layers import these shared type primitives"
)]
pub(crate) use types::{SqlSpan, SqlType};
