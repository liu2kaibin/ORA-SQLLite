use std::collections::HashMap;

use super::{
    WorkMemory,
    expr::{int_decimal, validate_value},
    memory::{checked_add_bytes, owned_value_payload_bytes, owned_values_bytes, resource_limit},
};
use crate::sql::{ParameterKey, SqlSpan};
use crate::{
    DataType, DbError, ErrorCode, ParameterIdentifier, ParameterMetadata, ParameterValue,
    Parameters, Result, Value,
    oracle_type::{OracleTypeDescriptor, normalize_for_type},
};

/// One immutable owned slot per logical parameter, in prepared metadata order.
pub(crate) struct ParameterFrame {
    values: Box<[Value]>,
}

impl std::fmt::Debug for ParameterFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParameterFrame").finish_non_exhaustive()
    }
}

impl ParameterFrame {
    pub(crate) fn bind(
        metadata: &[ParameterMetadata],
        parameters: Parameters<'_>,
        memory: &mut WorkMemory,
    ) -> Result<Self> {
        let mut supplied = Vec::with_capacity(metadata.len());
        match parameters {
            Parameters::None if metadata.is_empty() => {}
            Parameters::Positional(values) if values.len() == metadata.len() => {
                for (index, (meta, value)) in metadata.iter().zip(values).enumerate() {
                    let position = u16::try_from(index + 1).map_err(|_| bind_error())?;
                    if meta.identifier() != &ParameterIdentifier::Position(position) {
                        return Err(bind_error());
                    }
                    supplied.push(value);
                }
            }
            Parameters::Named(values) if values.len() == metadata.len() => {
                let mut named = HashMap::with_capacity(values.len());
                for (name, value) in values {
                    if named.insert(normalize_name(name)?, value).is_some() {
                        return Err(bind_error());
                    }
                }
                for meta in metadata {
                    let ParameterIdentifier::Name(name) = meta.identifier() else {
                        return Err(bind_error());
                    };
                    // Removing both checks duplicate metadata and leaves extras observable.
                    supplied.push(
                        named
                            .remove(&normalize_name(name)?)
                            .ok_or_else(bind_error)?,
                    );
                }
                if !named.is_empty() {
                    return Err(bind_error());
                }
            }
            _ => return Err(bind_error()),
        }
        // Validate the entire input before budget reservation or owned value copies.
        for (meta, supplied) in metadata.iter().zip(&supplied) {
            validate_parameter(meta.data_type(), meta.allow_oracle_text_source, supplied)?;
        }
        let bytes = owned_values_bytes(supplied.iter().map(|value| value.value()))?;
        memory.reserve(bytes)?;
        let values = metadata
            .iter()
            .zip(supplied)
            .map(|(meta, supplied)| {
                Ok(match (meta.data_type(), supplied.value()) {
                    (Some(DataType::Decimal), Value::Int64(value)) => {
                        Value::Decimal(int_decimal(*value)?)
                    }
                    (_, value) => value.clone(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { values })
    }

    /// Access a canonical slot whose validity was established by the planner.
    pub(crate) fn get(&self, slot: usize) -> &Value {
        &self.values[slot]
    }

    pub(super) fn checked_get(&self, slot: u16) -> Result<&Value> {
        self.values.get(usize::from(slot)).ok_or_else(|| {
            DbError::new(
                ErrorCode::Internal,
                "parameter slot is absent from its frame",
            )
        })
    }

    /// Normalize descriptor-bearing Oracle parameter slots atomically before
    /// any source operator starts. The shared frame is then visible to both
    /// IndexScan bounds and residual expression evaluation.
    pub(crate) fn normalize_oracle_slots(
        &mut self,
        slots: &[(u16, OracleTypeDescriptor)],
        memory: &mut WorkMemory,
    ) -> Result<()> {
        if slots.is_empty() {
            return Ok(());
        }
        let scratch_bytes = normalization_scratch_bytes(&self.values, slots)?;
        memory.reserve(scratch_bytes)?;
        let normalized = (|| {
            let mut values = Vec::new();
            values
                .try_reserve(slots.len())
                .map_err(|_| resource_limit())?;
            for (slot, descriptor) in slots {
                let target = self.values.get(usize::from(*slot)).ok_or_else(|| {
                    DbError::new(
                        ErrorCode::Internal,
                        "parameter slot is absent from its frame",
                    )
                })?;
                values.push(normalize_for_type(*descriptor, target.clone())?);
            }
            Ok::<_, DbError>(values)
        })();
        let normalized = match normalized {
            Ok(values) => values,
            Err(error) => {
                memory.release(scratch_bytes)?;
                return Err(error);
            }
        };

        let old_bytes = owned_values_bytes(self.values.iter())?;
        let mut new_bytes = old_bytes;
        for ((slot, _), normalized) in slots.iter().zip(&normalized) {
            let target = self.values.get(usize::from(*slot)).ok_or_else(|| {
                DbError::new(
                    ErrorCode::Internal,
                    "parameter slot is absent from its frame",
                )
            })?;
            new_bytes = new_bytes
                .checked_sub(owned_value_payload_bytes(target)?)
                .ok_or_else(|| {
                    DbError::new(ErrorCode::Internal, "parameter memory accounting underflow")
                })?;
            new_bytes = checked_add_bytes(new_bytes, owned_value_payload_bytes(normalized)?)?;
        }

        for ((slot, _), normalized) in slots.iter().zip(normalized) {
            let target = self.values.get_mut(usize::from(*slot)).ok_or_else(|| {
                DbError::new(
                    ErrorCode::Internal,
                    "parameter slot is absent from its frame",
                )
            })?;
            *target = normalized;
        }
        memory.release(scratch_bytes)?;
        if new_bytes > old_bytes {
            // Scratch included each normalized payload, so converting this
            // amount from transient to retained accounting cannot exceed it.
            memory.reserve(new_bytes - old_bytes)?;
        } else if old_bytes > new_bytes {
            memory.release(old_bytes - new_bytes)?;
        }
        Ok(())
    }
}

fn normalization_scratch_bytes(
    values: &[Value],
    slots: &[(u16, OracleTypeDescriptor)],
) -> Result<u64> {
    let mut bytes = 32_u64;
    let value_bytes = std::mem::size_of::<Value>() as u64;
    let mut previous = None;
    for (slot, descriptor) in slots {
        if previous.is_some_and(|prior| prior >= *slot) {
            return Err(DbError::new(
                ErrorCode::Internal,
                "Oracle parameter slots are not strictly ordered",
            ));
        }
        previous = Some(*slot);
        let value = values.get(usize::from(*slot)).ok_or_else(|| {
            DbError::new(
                ErrorCode::Internal,
                "parameter slot is absent from its frame",
            )
        })?;
        bytes = checked_add_bytes(bytes, value_bytes)?;
        bytes = checked_add_bytes(bytes, normalization_scratch_payload(*descriptor, value)?)?;
    }
    Ok(bytes)
}

fn normalization_scratch_payload(descriptor: OracleTypeDescriptor, value: &Value) -> Result<u64> {
    let source_bytes = owned_value_payload_bytes(value)?;
    let (OracleTypeDescriptor::Char { length_chars }, Value::Text(text)) = (descriptor, value)
    else {
        return Ok(source_bytes);
    };
    if text.is_empty() {
        return Ok(source_bytes);
    }
    let character_count = text.chars().count();
    let target_chars = usize::try_from(length_chars).map_err(|_| resource_limit())?;
    if character_count > target_chars {
        return Ok(source_bytes);
    }
    checked_add_bytes(
        source_bytes,
        u64::try_from(target_chars - character_count).map_err(|_| resource_limit())?,
    )
}

fn normalize_name(name: &str) -> Result<String> {
    // Avoid allocating an unbounded caller string just to reject it.
    let body = name.strip_prefix(':').unwrap_or(name);
    if body.len() > 128 {
        return Err(bind_error());
    }
    let ParameterKey::Name(name) = ParameterKey::parse(&format!(":{body}"), SqlSpan::new(0, 0)?)?
    else {
        return Err(bind_error());
    };
    Ok(name)
}

fn validate_parameter(
    expected: Option<DataType>,
    allow_oracle_text_source: bool,
    supplied: &ParameterValue,
) -> Result<()> {
    let actual = match supplied {
        ParameterValue::TypedNull(DataType::Null | DataType::Unknown) => {
            return Err(parameter_type_error());
        }
        ParameterValue::TypedNull(concrete) => Some(*concrete),
        ParameterValue::Value(value) => {
            validate_value(value)?;
            match value {
                Value::Null => None,
                Value::Int64(_) => Some(DataType::Int64),
                Value::Decimal(_) => Some(DataType::Decimal),
                Value::Float64(_) => Some(DataType::Float64),
                Value::Text(_) => Some(DataType::Text),
                Value::Blob(_) => Some(DataType::Blob),
                Value::Date(_) => Some(DataType::Date),
                Value::Timestamp(_) => Some(DataType::Timestamp),
                Value::Bool(_) => Some(DataType::Bool),
            }
        }
    };
    match (expected, actual) {
        (Some(DataType::Null | DataType::Unknown), _) | (None, None) => Err(parameter_type_error()),
        (Some(target), Some(source))
            if target != source
                && !(target == DataType::Decimal && source == DataType::Int64)
                && !(allow_oracle_text_source && source == DataType::Text) =>
        {
            Err(parameter_type_error())
        }
        _ => Ok(()),
    }
}

fn bind_error() -> DbError {
    DbError::new(
        ErrorCode::Bind,
        "runtime parameter set does not match prepared metadata",
    )
}
fn parameter_type_error() -> DbError {
    DbError::new(
        ErrorCode::Type,
        "runtime parameter requires a compatible concrete type",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::exec::{memory::owned_values_bytes, tests::positional_metadata};
    use crate::{Decimal, ErrorCode};

    fn bind(metadata: &[ParameterMetadata], parameters: Parameters<'_>) -> Result<ParameterFrame> {
        ParameterFrame::bind(
            metadata,
            parameters,
            &mut WorkMemory::new(1_000_000).unwrap(),
        )
    }

    #[test]
    fn named_parameters_normalize_once_and_follow_metadata_slots() {
        let metadata = [
            ParameterMetadata {
                identifier: ParameterIdentifier::Name("B".into()),
                data_type: None,
                allow_oracle_text_source: false,
            },
            ParameterMetadata {
                identifier: ParameterIdentifier::Name("A".into()),
                data_type: None,
                allow_oracle_text_source: false,
            },
        ];
        let mut values = [
            (":a", Value::Text("owned".into()).into()),
            ("b", Value::Int64(2).into()),
        ];
        let frame = bind(&metadata, Parameters::named(&values)).unwrap();
        values[0].1 = Value::Null.into();
        assert_eq!(frame.get(0), &Value::Int64(2));
        assert_eq!(frame.get(1), &Value::Text("owned".into()));
        for names in [
            ["a", ":A"],
            ["a", "extra"],
            ["a", "\"b\""],
            ["a", "1"],
            ["a", "::b"],
            ["a", "b b"],
        ] {
            let values = names.map(|name| (name, Value::Int64(1).into()));
            assert_eq!(
                bind(&metadata, Parameters::named(&values))
                    .unwrap_err()
                    .code(),
                ErrorCode::Bind
            );
        }
        assert_eq!(
            bind(&metadata, Parameters::named(&values[..1]))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn sets_are_exact_and_reject_wrong_style_and_malformed_metadata() {
        let metadata = positional_metadata(&[None]);
        let values = [Value::Int64(1).into(), Value::Int64(2).into()];
        for supplied in [&values[..0], &values[..2]] {
            assert_eq!(
                bind(&metadata, Parameters::positional(supplied))
                    .unwrap_err()
                    .code(),
                ErrorCode::Bind
            );
        }
        assert_eq!(
            bind(&metadata, Parameters::named(&[("a", values[0].clone())]))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert_eq!(
            bind(&[], Parameters::positional(&values))
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert!(bind(&[], Parameters::none()).is_ok());
        assert!(bind(&[], Parameters::positional(&[])).is_ok());
        assert!(bind(&[], Parameters::named(&[])).is_ok());
        for identifiers in [
            vec![ParameterIdentifier::Position(0)],
            vec![ParameterIdentifier::Position(2)],
            vec![
                ParameterIdentifier::Position(1),
                ParameterIdentifier::Position(1),
            ],
            vec![
                ParameterIdentifier::Position(1),
                ParameterIdentifier::Name("A".into()),
            ],
        ] {
            let metadata: Vec<_> = identifiers
                .into_iter()
                .map(|identifier| ParameterMetadata {
                    identifier,
                    data_type: None,
                    allow_oracle_text_source: false,
                })
                .collect();
            assert_eq!(
                bind(&metadata, Parameters::positional(&values[..metadata.len()]))
                    .unwrap_err()
                    .code(),
                ErrorCode::Bind
            );
        }
    }

    #[test]
    fn only_int64_to_decimal_promotes_and_null_must_have_a_concrete_type() {
        let concrete = [
            DataType::Int64,
            DataType::Decimal,
            DataType::Float64,
            DataType::Text,
            DataType::Blob,
            DataType::Date,
            DataType::Timestamp,
            DataType::Bool,
        ];
        let date = crate::OracleDate::new(2026, 1, 1, 0, 0, 0).unwrap();
        let values = [
            Value::Int64(i64::MIN),
            Value::Decimal(Decimal::new(false, 1, 12).unwrap()),
            Value::Float64(1.5),
            Value::Text(String::new()),
            Value::Blob(vec![]),
            Value::Date(date),
            Value::Timestamp(crate::OracleTimestamp::new(date, 0, None).unwrap()),
            Value::Bool(true),
        ];
        for (i, target) in concrete.iter().enumerate() {
            let metadata = positional_metadata(&[Some(*target)]);
            for (j, value) in values.iter().enumerate() {
                let result = bind(&metadata, Parameters::positional(&[value.clone().into()]));
                if i == j || (i == 1 && j == 0) {
                    assert!(result.is_ok(), "{target:?} {value:?}");
                } else {
                    assert_eq!(result.unwrap_err().code(), ErrorCode::Type);
                }
            }
            assert_eq!(
                bind(&metadata, Parameters::positional(&[Value::Null.into()]))
                    .unwrap()
                    .get(0),
                &Value::Null
            );
            for source in concrete {
                let result = bind(
                    &metadata,
                    Parameters::positional(&[ParameterValue::TypedNull(source)]),
                );
                assert_eq!(
                    result.is_ok(),
                    source == *target
                        || (*target == DataType::Decimal && source == DataType::Int64)
                );
            }
        }
        let metadata = positional_metadata(&[None]);
        for invalid in [
            Value::Null.into(),
            ParameterValue::TypedNull(DataType::Null),
            ParameterValue::TypedNull(DataType::Unknown),
        ] {
            assert_eq!(
                bind(&metadata, Parameters::positional(&[invalid]))
                    .unwrap_err()
                    .code(),
                ErrorCode::Type
            );
        }
        for value in values {
            assert!(bind(&metadata, Parameters::positional(&[value.into()])).is_ok());
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                bind(
                    &metadata,
                    Parameters::positional(&[Value::Float64(value).into()])
                )
                .unwrap_err()
                .code(),
                ErrorCode::Type
            );
        }
    }

    #[test]
    fn oracle_target_text_sources_are_retained_but_legacy_decimal_rejects_them() {
        // Production break caught: accepting Text for every DECIMAL parameter
        // would weaken legacy binding, while eagerly converting this Oracle
        // source would bypass the target column's precision/scale contract.
        let identifier = ParameterIdentifier::Position(1);
        let oracle = [ParameterMetadata {
            identifier: identifier.clone(),
            data_type: Some(DataType::Decimal),
            allow_oracle_text_source: true,
        }];
        let text = [Value::Text("12.345".into()).into()];
        assert_eq!(
            bind(&oracle, Parameters::positional(&text)).unwrap().get(0),
            &Value::Text("12.345".into())
        );

        let legacy = [ParameterMetadata {
            identifier,
            data_type: Some(DataType::Decimal),
            allow_oracle_text_source: false,
        }];
        assert_eq!(
            bind(&legacy, Parameters::positional(&text))
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
    }

    #[test]
    fn frames_reserve_before_copying_and_failure_leaves_budget_unchanged() {
        let metadata = positional_metadata(&[None, None]);
        let values = [
            Value::Text("界".into()).into(),
            Value::Blob(vec![0; 513]).into(),
        ];
        let required = owned_values_bytes(values.iter().map(ParameterValue::value)).unwrap();
        let mut memory = WorkMemory::new(required - 1).unwrap();
        assert_eq!(
            ParameterFrame::bind(&metadata, Parameters::positional(&values), &mut memory)
                .unwrap_err()
                .code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(memory.used(), 0);
        let mut memory = WorkMemory::new(required).unwrap();
        let frame =
            ParameterFrame::bind(&metadata, Parameters::positional(&values), &mut memory).unwrap();
        assert_eq!(memory.used(), required);
        assert_eq!(frame.get(1), &Value::Blob(vec![0; 513]));
        let invalid = [Value::Text("large".repeat(100)).into(), Value::Null.into()];
        assert_eq!(
            ParameterFrame::bind(&metadata, Parameters::positional(&invalid), &mut memory)
                .unwrap_err()
                .code(),
            ErrorCode::Type
        );
        assert_eq!(memory.used(), required);
    }

    #[test]
    fn oracle_slot_normalization_enforces_touched_scratch_at_exact_boundary() {
        // Production breaks caught: reserving after normalization lets the
        // touched NUMBER copy bypass the ceiling, while cloning the entire
        // frame makes an unrelated large slot consume scratch budget.
        let untouched = "u".repeat(128 * 1024);
        let number = format!("{}1", "0".repeat(64 * 1024));
        let values = [
            Value::Text(untouched.clone()).into(),
            Value::Text(number.clone()).into(),
        ];
        let metadata = [
            ParameterMetadata {
                identifier: ParameterIdentifier::Position(1),
                data_type: Some(DataType::Text),
                allow_oracle_text_source: false,
            },
            ParameterMetadata {
                identifier: ParameterIdentifier::Position(2),
                data_type: Some(DataType::Decimal),
                allow_oracle_text_source: true,
            },
        ];
        let frame_bytes = owned_values_bytes(values.iter().map(ParameterValue::value)).unwrap();
        let touched_scratch = owned_values_bytes([values[1].value()]).unwrap();
        let descriptor = OracleTypeDescriptor::number(1, 0).unwrap();

        for (limit, accepted) in [
            (frame_bytes + touched_scratch - 1, false),
            (frame_bytes + touched_scratch, true),
        ] {
            let mut memory = WorkMemory::new(limit).unwrap();
            let mut frame =
                ParameterFrame::bind(&metadata, Parameters::positional(&values), &mut memory)
                    .unwrap();
            let result = frame.normalize_oracle_slots(&[(1, descriptor)], &mut memory);
            if accepted {
                result.unwrap();
                assert_eq!(frame.get(0), &Value::Text(untouched.clone()));
                assert_eq!(
                    frame.get(1),
                    &Value::Decimal(Decimal::new(false, 0, 1).unwrap())
                );
                assert_eq!(
                    memory.used(),
                    owned_values_bytes([frame.get(0), frame.get(1)]).unwrap()
                );
            } else {
                assert_eq!(result.unwrap_err().code(), ErrorCode::ResourceLimit);
                assert_eq!(frame.get(1), &Value::Text(number.clone()));
                assert_eq!(memory.used(), frame_bytes);
            }
        }
    }
}
