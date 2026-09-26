pub(crate) mod program;

use std::collections::BTreeMap;

use crate::{DbError, ErrorCode, Result, Value, scalar::TruthValue};

use super::{
    catalog::{
        CatalogReader,
        types::{CatalogName, ColumnDef, ConstraintKind, IndexId, TableId},
    },
    constraint::program::{CheckMemory, CheckProgram},
};

/// The complete, validated set of local constraints for one table. Keeping a
/// cloned view means DML validates the same catalog graph that owns its system
/// indexes without retaining a catalog-reader borrow across page mutation.
pub(crate) struct ConstraintCatalog {
    constraints: Box<[LocalConstraint]>,
    index_owners: BTreeMap<IndexId, CatalogName>,
}

pub(crate) enum LocalConstraint {
    PrimaryKey {
        name: CatalogName,
        ordinals: Box<[u16]>,
    },
    Unique {
        name: CatalogName,
        index_id: IndexId,
    },
    NotNull {
        name: CatalogName,
        ordinal: u16,
    },
    Check {
        name: CatalogName,
        program: CheckProgram,
    },
}

impl ConstraintCatalog {
    pub(crate) fn open(catalog: &CatalogReader, table: TableId) -> Result<Self> {
        let mut ordered = catalog
            .constraints_for_table(table)
            .map(|definition| {
                let name = definition.name().clone();
                let local = match definition.kind() {
                    ConstraintKind::PrimaryKey => LocalConstraint::PrimaryKey {
                        name,
                        ordinals: constraint_ordinals(catalog, definition.id())?,
                    },
                    ConstraintKind::Unique => LocalConstraint::Unique {
                        name,
                        index_id: definition.index_id().ok_or_else(|| {
                            DbError::new(
                                ErrorCode::Corruption,
                                "unique constraint has no system index",
                            )
                        })?,
                    },
                    ConstraintKind::NotNull => LocalConstraint::NotNull {
                        name,
                        ordinal: constraint_ordinal(catalog, definition.id())?,
                    },
                    ConstraintKind::Check => LocalConstraint::Check {
                        name,
                        program: CheckProgram::from_bytes(
                            definition.check_program().to_vec().into_boxed_slice(),
                        )?,
                    },
                    ConstraintKind::ForeignKey => {
                        return Err(DbError::new(
                            ErrorCode::Unsupported,
                            "foreign-key enforcement is not available",
                        ));
                    }
                };
                Ok((definition.id(), local, definition.index_id()))
            })
            .collect::<Result<Vec<_>>>()?;
        ordered.sort_by_key(|(id, _, _)| *id);

        let mut constraints = Vec::with_capacity(ordered.len());
        let mut index_owners = BTreeMap::new();
        for (_, local, index_id) in ordered {
            if let Some(index_id) = index_id {
                let name = match &local {
                    LocalConstraint::PrimaryKey { name, .. }
                    | LocalConstraint::Unique { name, .. } => name,
                    LocalConstraint::NotNull { .. } | LocalConstraint::Check { .. } => {
                        return Err(DbError::new(
                            ErrorCode::Corruption,
                            "non-key constraint owns a system index",
                        ));
                    }
                };
                if index_owners.insert(index_id, name.clone()).is_some() {
                    return Err(DbError::new(
                        ErrorCode::Corruption,
                        "system index has multiple local constraint owners",
                    ));
                }
            }
            constraints.push(local);
        }
        Ok(Self {
            constraints: constraints.into_boxed_slice(),
            index_owners,
        })
    }

    pub(crate) fn validate_row(
        &self,
        columns: &[ColumnDef],
        values: &[Value],
        memory: &mut impl CheckMemory,
    ) -> Result<()> {
        for constraint in self.constraints.iter() {
            match constraint {
                LocalConstraint::PrimaryKey { name, ordinals } => {
                    for ordinal in ordinals {
                        if matches!(row_value(values, *ordinal)?, Value::Null) {
                            return Err(violation(name));
                        }
                    }
                }
                LocalConstraint::Unique { .. } => {}
                LocalConstraint::NotNull { name, ordinal } => {
                    if matches!(row_value(values, *ordinal)?, Value::Null) {
                        return Err(violation(name));
                    }
                }
                LocalConstraint::Check { name, program } => {
                    // Keep the encoded program live while the evaluator streams
                    // it. The evaluator accounts for its stack and values; this
                    // reservation makes decoding itself subject to the same
                    // statement budget and is released on every path.
                    let program_bytes = u64::try_from(program.as_bytes().len()).map_err(|_| {
                        DbError::new(
                            ErrorCode::ResourceLimit,
                            "check program length exceeds work-memory accounting",
                        )
                    })?;
                    memory.reserve_check_bytes(program_bytes)?;
                    let result = program.evaluate(columns, values, memory);
                    memory.release_check_bytes(program_bytes)?;
                    if result? == TruthValue::False {
                        return Err(violation(name));
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn violation_name_for_index(&self, index: IndexId) -> Option<&CatalogName> {
        self.index_owners.get(&index)
    }
}

fn constraint_ordinals(
    catalog: &CatalogReader,
    id: super::catalog::types::ConstraintId,
) -> Result<Box<[u16]>> {
    let ordinals = catalog
        .columns_for_constraint(id)
        .map(ColumnDef::ordinal)
        .collect::<Vec<_>>();
    if ordinals.is_empty() {
        return Err(DbError::new(
            ErrorCode::Corruption,
            "key constraint has no constrained columns",
        ));
    }
    Ok(ordinals.into_boxed_slice())
}

fn constraint_ordinal(
    catalog: &CatalogReader,
    id: super::catalog::types::ConstraintId,
) -> Result<u16> {
    let mut columns = catalog.columns_for_constraint(id);
    let ordinal = columns
        .next()
        .map(ColumnDef::ordinal)
        .ok_or_else(|| DbError::new(ErrorCode::Corruption, "not-null constraint has no column"))?;
    if columns.next().is_some() {
        return Err(DbError::new(
            ErrorCode::Corruption,
            "not-null constraint has multiple columns",
        ));
    }
    Ok(ordinal)
}

fn row_value(values: &[Value], ordinal: u16) -> Result<&Value> {
    values.get(usize::from(ordinal)).ok_or_else(|| {
        DbError::new(
            ErrorCode::Type,
            "constraint row value count does not match table schema",
        )
    })
}

fn violation(name: &CatalogName) -> DbError {
    DbError::new(
        ErrorCode::Constraint,
        format!("constraint {} is violated", name.as_str()),
    )
}
