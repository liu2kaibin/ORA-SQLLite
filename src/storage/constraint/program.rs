use std::collections::BTreeMap;

use crate::storage::catalog::types::{ColumnDef, ColumnId, ColumnType, MAX_CHECK_PROGRAM_BYTES};
use crate::{
    DbError, Decimal, ErrorCode, OracleDate, OracleTimestamp, Result, Value,
    scalar::{self, ScalarBinaryOp, ScalarUnaryOp, TruthValue},
};

const VERSION: u8 = 1;
const HEADER_LEN: usize = 8;
const INSTRUCTION_HEADER_LEN: usize = 8;
const VALUE_BODY_BYTES: u64 = std::mem::size_of::<Value>() as u64;
const STACK_CONTAINER_BYTES: u64 = 32;
const STACK_ENTRY_BYTES: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckType {
    Int64,
    Decimal,
    Float64,
    Text,
    Blob,
    Date,
    Timestamp,
    TimestampWithTimeZone,
    Bool,
}
impl CheckType {
    fn code(self) -> u8 {
        match self {
            Self::Int64 => 1,
            Self::Decimal => 2,
            Self::Float64 => 3,
            Self::Text => 4,
            Self::Blob => 5,
            Self::Date => 6,
            Self::Timestamp => 7,
            Self::TimestampWithTimeZone => 8,
            Self::Bool => 9,
        }
    }
    fn decode(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::Int64),
            2 => Ok(Self::Decimal),
            3 => Ok(Self::Float64),
            4 => Ok(Self::Text),
            5 => Ok(Self::Blob),
            6 => Ok(Self::Date),
            7 => Ok(Self::Timestamp),
            8 => Ok(Self::TimestampWithTimeZone),
            9 => Ok(Self::Bool),
            _ => Err(corruption("check program result type is invalid")),
        }
    }
    fn column_type(self) -> ColumnType {
        match self {
            Self::Int64 => ColumnType::Int64,
            Self::Decimal => ColumnType::Decimal,
            Self::Float64 => ColumnType::Float64,
            Self::Text => ColumnType::Text,
            Self::Blob => ColumnType::Blob,
            Self::Date => ColumnType::Date,
            Self::Timestamp => ColumnType::Timestamp,
            Self::TimestampWithTimeZone => ColumnType::TimestampWithTimeZone,
            Self::Bool => ColumnType::Bool,
        }
    }
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Null => None,
            Value::Int64(_) => Some(Self::Int64),
            Value::Decimal(_) => Some(Self::Decimal),
            Value::Float64(_) => Some(Self::Float64),
            Value::Text(_) => Some(Self::Text),
            Value::Blob(_) => Some(Self::Blob),
            Value::Date(_) => Some(Self::Date),
            Value::Timestamp(value) => Some(if value.offset_minutes().is_some() {
                Self::TimestampWithTimeZone
            } else {
                Self::Timestamp
            }),
            Value::Bool(_) => Some(Self::Bool),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckUnaryOp {
    Plus,
    Minus,
    Not,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Concat,
    And,
    Or,
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CheckTemplateInstruction {
    Column {
        ordinal: u16,
        result: CheckType,
    },
    Literal {
        value: Value,
        result: CheckType,
    },
    Unary {
        op: CheckUnaryOp,
        result: CheckType,
    },
    Binary {
        op: CheckBinaryOp,
        result: CheckType,
    },
    IsNull {
        negated: bool,
        result: CheckType,
    },
    InList {
        value_count: u16,
        negated: bool,
        result: CheckType,
    },
    Between {
        negated: bool,
        result: CheckType,
    },
    Like {
        negated: bool,
        result: CheckType,
    },
}
pub(crate) struct CheckProgramTemplate {
    instructions: Box<[CheckTemplateInstruction]>,
}
#[derive(Debug)]
pub(crate) struct CheckProgram {
    bytes: Box<[u8]>,
}
pub(crate) trait CheckMemory {
    fn reserve_check_bytes(&mut self, bytes: u64) -> Result<()>;
    fn release_check_bytes(&mut self, bytes: u64) -> Result<()>;
}
impl CheckMemory for crate::sql::exec::WorkMemory {
    fn reserve_check_bytes(&mut self, bytes: u64) -> Result<()> {
        self.reserve(bytes)
    }
    fn release_check_bytes(&mut self, bytes: u64) -> Result<()> {
        self.release(bytes)
    }
}

impl CheckProgramTemplate {
    pub(crate) fn new(instructions: Vec<CheckTemplateInstruction>) -> Result<Self> {
        validate_template(&instructions)?;
        Ok(Self {
            instructions: instructions.into_boxed_slice(),
        })
    }
    pub(crate) fn resolve(&self, columns: &[ColumnId]) -> Result<CheckProgram> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[VERSION, 0]);
        bytes.extend_from_slice(
            &u32::try_from(self.instructions.len())
                .map_err(|_| resource_limit())?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        for instruction in self.instructions.iter() {
            let (opcode, result, operands, payload) = encode_instruction(instruction, columns)?;
            push_instruction(&mut bytes, opcode, result, operands, &payload)?;
        }
        if bytes.len() > MAX_CHECK_PROGRAM_BYTES {
            return Err(resource_limit());
        }
        let program = CheckProgram {
            bytes: bytes.into_boxed_slice(),
        };
        program.validate_types(None)?;
        Ok(program)
    }
}
impl CheckProgram {
    pub(crate) fn from_bytes(bytes: Box<[u8]>) -> Result<Self> {
        if bytes.len() > MAX_CHECK_PROGRAM_BYTES {
            return Err(resource_limit());
        }
        let program = Self { bytes };
        program.validate_types(None)?;
        Ok(program)
    }
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn validate(&self, columns: &[ColumnDef]) -> Result<CheckType> {
        self.validate_types(Some(columns))
    }
    fn validate_types(&self, columns: Option<&[ColumnDef]>) -> Result<CheckType> {
        let mut reader = Reader::new(&self.bytes);
        if reader.u8()? != VERSION {
            return if self.bytes.first().copied().is_some_and(|v| v > VERSION) {
                Err(DbError::new(
                    ErrorCode::Unsupported,
                    "check program version is newer than this implementation",
                ))
            } else {
                Err(corruption("check program version is invalid"))
            };
        }
        if reader.u8()? != 0 {
            return Err(corruption("check program reserved byte is nonzero"));
        };
        let count = usize::try_from(reader.u32()?)
            .map_err(|_| corruption("instruction count exceeds platform"))?;
        if reader.u16()? != 0 {
            return Err(corruption("check program reserved bytes are nonzero"));
        };
        let max_count = self
            .bytes
            .len()
            .checked_sub(HEADER_LEN)
            .ok_or_else(|| corruption("check program is truncated"))?
            / INSTRUCTION_HEADER_LEN;
        if count > max_count {
            return Err(corruption("check instruction count exceeds program bytes"));
        }
        let mut stack = Vec::new();
        stack.try_reserve(count).map_err(|_| resource_limit())?;
        for _ in 0..count {
            let instruction = decode_instruction(&mut reader)?;
            validate_instruction(&instruction, &mut stack, columns)?;
        }
        if reader.remaining() != 0 {
            return Err(corruption("check program contains trailing bytes"));
        }
        if stack.len() != 1 {
            return Err(corruption("check program must leave exactly one result"));
        }
        let result = stack
            .pop()
            .ok_or_else(|| corruption("check program result is absent"))?;
        if result != CheckType::Bool {
            return Err(corruption("check program final result must be Bool"));
        }
        Ok(result)
    }
    pub(crate) fn evaluate(
        &self,
        columns: &[ColumnDef],
        row: &[Value],
        memory: &mut impl CheckMemory,
    ) -> Result<TruthValue> {
        self.validate(columns)?;
        if row.len() != columns.len() {
            return Err(DbError::new(
                ErrorCode::Type,
                "check row value count does not match table schema",
            ));
        }
        let mut by_id = BTreeMap::new();
        for (ordinal, column) in columns.iter().enumerate() {
            by_id.insert(column.id(), ordinal);
        }
        let mut account = Account::new(memory);
        let result = (|| {
            let mut reader = Reader::new(&self.bytes);
            reader.skip(HEADER_LEN)?;
            let count = usize::try_from(reader.u32_at(2)?)
                .map_err(|_| corruption("instruction count exceeds platform"))?;
            let stack_bytes = STACK_CONTAINER_BYTES
                .checked_add(
                    u64::try_from(count)
                        .map_err(|_| resource_limit())?
                        .checked_mul(STACK_ENTRY_BYTES)
                        .ok_or_else(resource_limit)?,
                )
                .ok_or_else(resource_limit)?;
            account.reserve(stack_bytes)?;
            let mut values = Vec::new();
            values.try_reserve(count).map_err(|_| resource_limit())?;
            for _ in 0..count {
                let instruction = decode_instruction(&mut reader)?;
                let value = match instruction.kind {
                    Kind::Column(id) => {
                        let ordinal = *by_id
                            .get(&id)
                            .ok_or_else(|| corruption("check program column is missing"))?;
                        row.get(ordinal).cloned().ok_or_else(|| {
                            DbError::new(ErrorCode::Type, "check row column is absent")
                        })?
                    }
                    Kind::Literal(value) => value,
                    Kind::Unary(op) => scalar::eval_unary(map_unary(op), pop(&mut values)?)?,
                    Kind::Binary(op) => {
                        let right = pop(&mut values)?;
                        let left = pop(&mut values)?;
                        scalar::eval_binary(map_binary(op), &left, &right)?
                    }
                    Kind::IsNull(negated) => {
                        Value::Bool(matches!(pop(&mut values)?, Value::Null) != negated)
                    }
                    Kind::In(negated, count) => {
                        let count = usize::from(count);
                        let start = values
                            .len()
                            .checked_sub(count)
                            .ok_or_else(|| corruption("check stack underflow"))?;
                        let operand = start
                            .checked_sub(1)
                            .ok_or_else(|| corruption("check stack underflow"))?;
                        let mut truth = TruthValue::False;
                        for candidate in &values[start..] {
                            truth = truth.or(scalar::sql_equal(&values[operand], candidate)?)
                        }
                        values.truncate(operand);
                        if negated { truth.not() } else { truth }.into_value()
                    }
                    Kind::Between(negated) => {
                        let high = pop(&mut values)?;
                        let low = pop(&mut values)?;
                        let operand = pop(&mut values)?;
                        let lo = TruthValue::from_value(&scalar::eval_binary(
                            ScalarBinaryOp::GreaterThanOrEqual,
                            &operand,
                            &low,
                        )?)?;
                        let hi = TruthValue::from_value(&scalar::eval_binary(
                            ScalarBinaryOp::LessThanOrEqual,
                            &operand,
                            &high,
                        )?)?;
                        if negated {
                            lo.and(hi).not()
                        } else {
                            lo.and(hi)
                        }
                        .into_value()
                    }
                    Kind::Like(negated) => {
                        let pattern = pop(&mut values)?;
                        let operand = pop(&mut values)?;
                        match (&operand, &pattern) {
                            (Value::Null, _) | (_, Value::Null) => Value::Null,
                            (Value::Text(text), Value::Text(pattern)) => {
                                Value::Bool(scalar::like(text, pattern) != negated)
                            }
                            _ => {
                                return Err(DbError::new(
                                    ErrorCode::Type,
                                    "LIKE requires Text operands",
                                ));
                            }
                        }
                    }
                };
                scalar::validate_value(&value)?;
                account.reserve(value_bytes(&value)?)?;
                values.push(value);
            }
            let value = pop(&mut values)?;
            if !values.is_empty() {
                return Err(corruption("check program leaves extra values"));
            }
            TruthValue::from_value(&value)
        })();
        account.finish()?;
        result
    }
}

#[derive(Debug, Clone)]
enum Kind {
    Column(ColumnId),
    Literal(Value),
    Unary(CheckUnaryOp),
    Binary(CheckBinaryOp),
    IsNull(bool),
    In(bool, u16),
    Between(bool),
    Like(bool),
}
struct Instruction {
    result: CheckType,
    operands: u16,
    kind: Kind,
}
fn validate_template(instructions: &[CheckTemplateInstruction]) -> Result<()> {
    let mut stack = Vec::new();
    for instruction in instructions {
        let (kind, result, operands) = template_shape(instruction)?;
        validate_instruction(
            &Instruction {
                result,
                operands,
                kind,
            },
            &mut stack,
            None,
        )
        .map_err(|error| {
            if error.code() == ErrorCode::Corruption {
                DbError::new(ErrorCode::Type, "check template is invalid")
            } else {
                error
            }
        })?;
    }
    if stack.len() != 1 || stack.last() != Some(&CheckType::Bool) {
        return Err(DbError::new(
            ErrorCode::Type,
            "check template must leave one Bool result",
        ));
    }
    Ok(())
}
fn template_shape(instruction: &CheckTemplateInstruction) -> Result<(Kind, CheckType, u16)> {
    Ok(match instruction {
        CheckTemplateInstruction::Column { ordinal, result } => (
            Kind::Column(ColumnId::new(u64::from(*ordinal) + 1)?),
            *result,
            0,
        ),
        CheckTemplateInstruction::Literal { value, result } => {
            (Kind::Literal(value.clone()), *result, 0)
        }
        CheckTemplateInstruction::Unary { op, result } => (Kind::Unary(*op), *result, 1),
        CheckTemplateInstruction::Binary { op, result } => (Kind::Binary(*op), *result, 2),
        CheckTemplateInstruction::IsNull { negated, result } => {
            (Kind::IsNull(*negated), *result, 1)
        }
        CheckTemplateInstruction::InList {
            value_count,
            negated,
            result,
        } => (
            Kind::In(*negated, *value_count),
            *result,
            value_count.checked_add(1).ok_or_else(resource_limit)?,
        ),
        CheckTemplateInstruction::Between { negated, result } => {
            (Kind::Between(*negated), *result, 3)
        }
        CheckTemplateInstruction::Like { negated, result } => (Kind::Like(*negated), *result, 2),
    })
}
fn encode_instruction(
    instruction: &CheckTemplateInstruction,
    columns: &[ColumnId],
) -> Result<(u8, CheckType, u16, Vec<u8>)> {
    match instruction {
        CheckTemplateInstruction::Column { ordinal, result } => {
            let id = *columns.get(usize::from(*ordinal)).ok_or_else(|| {
                DbError::new(ErrorCode::Type, "check template column ordinal is absent")
            })?;
            Ok((1, *result, 0, id.get().to_be_bytes().to_vec()))
        }
        CheckTemplateInstruction::Literal { value, result } => {
            Ok((2, *result, 0, encode_scalar(value)?))
        }
        CheckTemplateInstruction::Unary { op, result } => Ok((
            match op {
                CheckUnaryOp::Plus => 0x10,
                CheckUnaryOp::Minus => 0x11,
                CheckUnaryOp::Not => 0x12,
            },
            *result,
            1,
            Vec::new(),
        )),
        CheckTemplateInstruction::Binary { op, result } => {
            Ok((binary_opcode(*op), *result, 2, Vec::new()))
        }
        CheckTemplateInstruction::IsNull { negated, result } => {
            Ok((if *negated { 0x31 } else { 0x30 }, *result, 1, Vec::new()))
        }
        CheckTemplateInstruction::InList {
            value_count,
            negated,
            result,
        } => Ok((
            if *negated { 0x41 } else { 0x40 },
            *result,
            value_count.checked_add(1).ok_or_else(resource_limit)?,
            Vec::new(),
        )),
        CheckTemplateInstruction::Between { negated, result } => {
            Ok((if *negated { 0x43 } else { 0x42 }, *result, 3, Vec::new()))
        }
        CheckTemplateInstruction::Like { negated, result } => {
            Ok((if *negated { 0x45 } else { 0x44 }, *result, 2, Vec::new()))
        }
    }
}
fn push_instruction(
    bytes: &mut Vec<u8>,
    opcode: u8,
    result: CheckType,
    operands: u16,
    payload: &[u8],
) -> Result<()> {
    let next = bytes
        .len()
        .checked_add(INSTRUCTION_HEADER_LEN)
        .and_then(|n| n.checked_add(payload.len()))
        .ok_or_else(resource_limit)?;
    if next > MAX_CHECK_PROGRAM_BYTES {
        return Err(resource_limit());
    }
    bytes.push(opcode);
    bytes.push(result.code());
    bytes.extend_from_slice(&operands.to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(|_| resource_limit())?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(payload);
    Ok(())
}
fn decode_instruction(reader: &mut Reader<'_>) -> Result<Instruction> {
    let opcode = reader.u8()?;
    let result = CheckType::decode(reader.u8()?)?;
    let operands = reader.u16()?;
    let len = usize::try_from(reader.u32()?)
        .map_err(|_| corruption("check payload length exceeds platform"))?;
    let payload = reader.bytes(len)?;
    let kind = match opcode {
        1 => {
            if operands != 0 || payload.len() != 8 {
                return Err(corruption("column instruction shape is invalid"));
            }
            Kind::Column(
                ColumnId::new(u64::from_be_bytes(
                    payload
                        .try_into()
                        .map_err(|_| corruption("column payload is invalid"))?,
                ))
                .map_err(|_| corruption("column ID is invalid"))?,
            )
        }
        2 => {
            if operands != 0 {
                return Err(corruption("literal operand count is invalid"));
            }
            Kind::Literal(decode_scalar(payload)?)
        }
        0x10 => Kind::Unary(CheckUnaryOp::Plus),
        0x11 => Kind::Unary(CheckUnaryOp::Minus),
        0x12 => Kind::Unary(CheckUnaryOp::Not),
        0x20..=0x26 | 0x28..=0x2d => Kind::Binary(opcode_binary(opcode)?),
        0x30 => Kind::IsNull(false),
        0x31 => Kind::IsNull(true),
        0x40 => Kind::In(
            false,
            operands
                .checked_sub(1)
                .ok_or_else(|| corruption("IN operand count is invalid"))?,
        ),
        0x41 => Kind::In(
            true,
            operands
                .checked_sub(1)
                .ok_or_else(|| corruption("IN operand count is invalid"))?,
        ),
        0x42 => Kind::Between(false),
        0x43 => Kind::Between(true),
        0x44 => Kind::Like(false),
        0x45 => Kind::Like(true),
        _ => return Err(corruption("check program opcode is invalid")),
    };
    if !matches!(kind, Kind::Column(_) | Kind::Literal(_)) && !payload.is_empty() {
        return Err(corruption("operator payload must be empty"));
    }
    Ok(Instruction {
        result,
        operands,
        kind,
    })
}
fn validate_instruction(
    instruction: &Instruction,
    stack: &mut Vec<CheckType>,
    columns: Option<&[ColumnDef]>,
) -> Result<()> {
    let want = match &instruction.kind {
        Kind::Column(id) => {
            if instruction.operands != 0 {
                return Err(corruption("column operand count is invalid"));
            }
            if let Some(columns) = columns {
                let column = columns
                    .iter()
                    .find(|column| column.id() == *id)
                    .ok_or_else(|| {
                        corruption("check program references a foreign or missing column")
                    })?;
                if column.column_type() != instruction.result.column_type() {
                    return Err(corruption(
                        "check program column result type does not match catalog",
                    ));
                }
            }
            Some(instruction.result)
        }
        Kind::Literal(value) => {
            if instruction.operands != 0 {
                return Err(corruption("literal operand count is invalid"));
            }
            if let Some(actual) = CheckType::from_value(value) {
                if actual != instruction.result {
                    return Err(corruption("literal result type does not match scalar"));
                }
            }
            Some(instruction.result)
        }
        Kind::Unary(op) => {
            if instruction.operands != 1 {
                return Err(corruption("unary operand count is invalid"));
            }
            let value = pop_type(stack)?;
            match op {
                CheckUnaryOp::Not => require(value, CheckType::Bool)?,
                CheckUnaryOp::Plus | CheckUnaryOp::Minus => require_numeric(value)?,
            };
            require(
                instruction.result,
                if *op == CheckUnaryOp::Not {
                    CheckType::Bool
                } else {
                    value
                },
            )?;
            Some(instruction.result)
        }
        Kind::Binary(op) => {
            if instruction.operands != 2 {
                return Err(corruption("binary operand count is invalid"));
            }
            let right = pop_type(stack)?;
            let left = pop_type(stack)?;
            match op {
                CheckBinaryOp::And | CheckBinaryOp::Or => {
                    require(left, CheckType::Bool)?;
                    require(right, CheckType::Bool)?
                }
                CheckBinaryOp::Concat => {
                    require(left, CheckType::Text)?;
                    require(right, CheckType::Text)?
                }
                CheckBinaryOp::Add
                | CheckBinaryOp::Subtract
                | CheckBinaryOp::Multiply
                | CheckBinaryOp::Divide => {
                    require_numeric(left)?;
                    require_numeric(right)?;
                    if left != right
                        && !(matches!(left, CheckType::Int64 | CheckType::Decimal)
                            && matches!(right, CheckType::Int64 | CheckType::Decimal))
                    {
                        return Err(corruption("binary numeric operand types are incompatible"));
                    }
                }
                _ => {
                    if left != right
                        && !(matches!(left, CheckType::Int64 | CheckType::Decimal)
                            && matches!(right, CheckType::Int64 | CheckType::Decimal))
                    {
                        return Err(corruption("comparison operand types are incompatible"));
                    }
                }
            };
            let expected = match op {
                CheckBinaryOp::And
                | CheckBinaryOp::Or
                | CheckBinaryOp::Equal
                | CheckBinaryOp::NotEqual
                | CheckBinaryOp::LessThan
                | CheckBinaryOp::LessThanOrEqual
                | CheckBinaryOp::GreaterThan
                | CheckBinaryOp::GreaterThanOrEqual => CheckType::Bool,
                CheckBinaryOp::Concat => CheckType::Text,
                CheckBinaryOp::Add
                | CheckBinaryOp::Subtract
                | CheckBinaryOp::Multiply
                | CheckBinaryOp::Divide => {
                    if matches!(left, CheckType::Int64 | CheckType::Decimal)
                        && matches!(right, CheckType::Int64 | CheckType::Decimal)
                    {
                        if left == CheckType::Decimal || right == CheckType::Decimal {
                            CheckType::Decimal
                        } else {
                            CheckType::Int64
                        }
                    } else {
                        left
                    }
                }
            };
            require(instruction.result, expected)?;
            Some(instruction.result)
        }
        Kind::IsNull(_) => {
            if instruction.operands != 1 {
                return Err(corruption("IS NULL operand count is invalid"));
            }
            pop_type(stack)?;
            require(instruction.result, CheckType::Bool)?;
            Some(instruction.result)
        }
        Kind::In(_, count) => {
            if *count == 0
                || instruction.operands != count.checked_add(1).ok_or_else(resource_limit)?
            {
                return Err(corruption("IN operand count is invalid"));
            }
            let first = pop_type(stack)?;
            for _ in 0..*count {
                let candidate = pop_type(stack)?;
                if candidate != first {
                    return Err(corruption("IN operand types are incompatible"));
                }
            }
            require(instruction.result, CheckType::Bool)?;
            Some(instruction.result)
        }
        Kind::Between(_) => {
            if instruction.operands != 3 {
                return Err(corruption("BETWEEN operand count is invalid"));
            }
            let high = pop_type(stack)?;
            let low = pop_type(stack)?;
            let value = pop_type(stack)?;
            if high != value || low != value {
                return Err(corruption("BETWEEN operand types are incompatible"));
            }
            require(instruction.result, CheckType::Bool)?;
            Some(instruction.result)
        }
        Kind::Like(_) => {
            if instruction.operands != 2 {
                return Err(corruption("LIKE operand count is invalid"));
            }
            require(pop_type(stack)?, CheckType::Text)?;
            require(instruction.result, CheckType::Bool)?;
            require(pop_type(stack)?, CheckType::Text)?;
            Some(instruction.result)
        }
    };
    if let Some(result) = want {
        let expected = match instruction.kind {
            Kind::Unary(CheckUnaryOp::Not)
            | Kind::Binary(
                CheckBinaryOp::And
                | CheckBinaryOp::Or
                | CheckBinaryOp::Equal
                | CheckBinaryOp::NotEqual
                | CheckBinaryOp::LessThan
                | CheckBinaryOp::LessThanOrEqual
                | CheckBinaryOp::GreaterThan
                | CheckBinaryOp::GreaterThanOrEqual,
            )
            | Kind::IsNull(_)
            | Kind::In(_, _)
            | Kind::Between(_)
            | Kind::Like(_) => CheckType::Bool,
            Kind::Unary(CheckUnaryOp::Plus | CheckUnaryOp::Minus) => result,
            Kind::Binary(CheckBinaryOp::Concat) => CheckType::Text,
            Kind::Binary(_) => result,
            _ => result,
        };
        if instruction.result != expected {
            return Err(corruption("check instruction result type is invalid"));
        }
        stack.push(instruction.result);
    }
    Ok(())
}
fn pop_type(stack: &mut Vec<CheckType>) -> Result<CheckType> {
    stack
        .pop()
        .ok_or_else(|| corruption("check stack underflow"))
}
fn require(actual: CheckType, expected: CheckType) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(corruption("check operand type is invalid"))
    }
}
fn require_numeric(value: CheckType) -> Result<()> {
    if matches!(
        value,
        CheckType::Int64 | CheckType::Decimal | CheckType::Float64
    ) {
        Ok(())
    } else {
        Err(corruption("check numeric operand type is invalid"))
    }
}
fn binary_opcode(op: CheckBinaryOp) -> u8 {
    match op {
        CheckBinaryOp::Add => 0x20,
        CheckBinaryOp::Subtract => 0x21,
        CheckBinaryOp::Multiply => 0x22,
        CheckBinaryOp::Divide => 0x23,
        CheckBinaryOp::Concat => 0x24,
        CheckBinaryOp::And => 0x25,
        CheckBinaryOp::Or => 0x26,
        CheckBinaryOp::Equal => 0x28,
        CheckBinaryOp::NotEqual => 0x29,
        CheckBinaryOp::LessThan => 0x2a,
        CheckBinaryOp::LessThanOrEqual => 0x2b,
        CheckBinaryOp::GreaterThan => 0x2c,
        CheckBinaryOp::GreaterThanOrEqual => 0x2d,
    }
}
fn opcode_binary(opcode: u8) -> Result<CheckBinaryOp> {
    match opcode {
        0x20 => Ok(CheckBinaryOp::Add),
        0x21 => Ok(CheckBinaryOp::Subtract),
        0x22 => Ok(CheckBinaryOp::Multiply),
        0x23 => Ok(CheckBinaryOp::Divide),
        0x24 => Ok(CheckBinaryOp::Concat),
        0x25 => Ok(CheckBinaryOp::And),
        0x26 => Ok(CheckBinaryOp::Or),
        0x28 => Ok(CheckBinaryOp::Equal),
        0x29 => Ok(CheckBinaryOp::NotEqual),
        0x2a => Ok(CheckBinaryOp::LessThan),
        0x2b => Ok(CheckBinaryOp::LessThanOrEqual),
        0x2c => Ok(CheckBinaryOp::GreaterThan),
        0x2d => Ok(CheckBinaryOp::GreaterThanOrEqual),
        _ => Err(corruption("binary opcode is invalid")),
    }
}
fn map_unary(op: CheckUnaryOp) -> ScalarUnaryOp {
    match op {
        CheckUnaryOp::Plus => ScalarUnaryOp::Plus,
        CheckUnaryOp::Minus => ScalarUnaryOp::Minus,
        CheckUnaryOp::Not => ScalarUnaryOp::Not,
    }
}
fn map_binary(op: CheckBinaryOp) -> ScalarBinaryOp {
    match op {
        CheckBinaryOp::Add => ScalarBinaryOp::Add,
        CheckBinaryOp::Subtract => ScalarBinaryOp::Subtract,
        CheckBinaryOp::Multiply => ScalarBinaryOp::Multiply,
        CheckBinaryOp::Divide => ScalarBinaryOp::Divide,
        CheckBinaryOp::Concat => ScalarBinaryOp::Concat,
        CheckBinaryOp::And => ScalarBinaryOp::And,
        CheckBinaryOp::Or => ScalarBinaryOp::Or,
        CheckBinaryOp::Equal => ScalarBinaryOp::Equal,
        CheckBinaryOp::NotEqual => ScalarBinaryOp::NotEqual,
        CheckBinaryOp::LessThan => ScalarBinaryOp::LessThan,
        CheckBinaryOp::LessThanOrEqual => ScalarBinaryOp::LessThanOrEqual,
        CheckBinaryOp::GreaterThan => ScalarBinaryOp::GreaterThan,
        CheckBinaryOp::GreaterThanOrEqual => ScalarBinaryOp::GreaterThanOrEqual,
    }
}
fn encode_scalar(value: &Value) -> Result<Vec<u8>> {
    scalar::validate_value(value)?;
    let (tag, body) = match value {
        Value::Null => (0, Vec::new()),
        Value::Int64(value) => (1, value.to_le_bytes().to_vec()),
        Value::Decimal(value) => {
            let mut bytes = vec![u8::from(value.negative()), value.scale() as u8, 0, 0];
            bytes.extend_from_slice(&value.magnitude().to_le_bytes());
            (2, bytes)
        }
        Value::Float64(value) => (3, value.to_bits().to_le_bytes().to_vec()),
        Value::Text(value) => (4, value.as_bytes().to_vec()),
        Value::Blob(value) => (5, value.clone()),
        Value::Date(value) => (6, encode_date(*value)),
        Value::Timestamp(value) => {
            let mut bytes = encode_date(value.date());
            bytes.extend_from_slice(&value.micros().to_le_bytes());
            if let Some(offset) = value.offset_minutes() {
                bytes.extend_from_slice(&offset.to_le_bytes());
                bytes.extend_from_slice(&0_u16.to_le_bytes());
                (8, bytes)
            } else {
                (7, bytes)
            }
        }
        Value::Bool(value) => (9, vec![u8::from(*value)]),
    };
    let mut result = vec![tag];
    result.extend_from_slice(
        &u32::try_from(body.len())
            .map_err(|_| resource_limit())?
            .to_le_bytes(),
    );
    result.extend_from_slice(&body);
    Ok(result)
}
fn decode_scalar(payload: &[u8]) -> Result<Value> {
    let mut reader = Reader::new(payload);
    let tag = reader.u8()?;
    let len =
        usize::try_from(reader.u32()?).map_err(|_| corruption("scalar length exceeds platform"))?;
    let body = reader.bytes(len)?;
    if reader.remaining() != 0 {
        return Err(corruption("scalar contains trailing bytes"));
    }
    let value = match tag {
        0 => {
            if !body.is_empty() {
                return Err(corruption("NULL scalar payload must be empty"));
            }
            Value::Null
        }
        1 => Value::Int64(i64::from_le_bytes(array(body)?)),
        2 => {
            if body.len() != 20 || body[0] & !1 != 0 || body[2] != 0 || body[3] != 0 {
                return Err(corruption("decimal scalar is invalid"));
            }
            let magnitude = u128::from_le_bytes(array(&body[4..])?);
            Value::Decimal(
                Decimal::new(body[0] & 1 != 0, body[1] as i8, magnitude)
                    .map_err(|_| corruption("decimal scalar fields are invalid"))?,
            )
        }
        3 => {
            let value = f64::from_bits(u64::from_le_bytes(array(body)?));
            scalar::validate_sql_float(value)
                .map_err(|_| corruption("Float64 scalar must be finite"))?;
            Value::Float64(value)
        }
        4 => Value::Text(
            std::str::from_utf8(body)
                .map_err(|_| corruption("Text scalar is not UTF-8"))?
                .to_owned(),
        ),
        5 => Value::Blob(body.to_vec()),
        6 => Value::Date(decode_date(body)?),
        7 => Value::Timestamp(decode_timestamp(body, None)?),
        8 => {
            if body.len() != 16 || body[14] != 0 || body[15] != 0 {
                return Err(corruption("timestamp time zone scalar is invalid"));
            }
            let date = decode_date(&body[..8])?;
            let micros = u32::from_le_bytes(array(&body[8..12])?);
            let offset = i16::from_le_bytes(array(&body[12..14])?);
            OracleTimestamp::new(date, micros, Some(offset))
                .map(Value::Timestamp)
                .map_err(|_| corruption("timestamp scalar fields are invalid"))?
        }
        9 => match body {
            [0] => Value::Bool(false),
            [1] => Value::Bool(true),
            _ => return Err(corruption("Bool scalar is invalid")),
        },
        _ => return Err(corruption("scalar tag is invalid")),
    };
    Ok(value)
}
fn encode_date(value: OracleDate) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&value.year().to_le_bytes());
    bytes.extend_from_slice(&[
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        0,
    ]);
    bytes
}
fn decode_date(bytes: &[u8]) -> Result<OracleDate> {
    if bytes.len() != 8 || bytes[7] != 0 {
        return Err(corruption("date scalar is invalid"));
    }
    OracleDate::new(
        u16::from_le_bytes(array(&bytes[..2])?),
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
    )
    .map_err(|_| corruption("date scalar fields are invalid"))
}
fn decode_timestamp(bytes: &[u8], offset: Option<i16>) -> Result<OracleTimestamp> {
    if bytes.len() != 12 {
        return Err(corruption("timestamp scalar is invalid"));
    }
    OracleTimestamp::new(
        decode_date(&bytes[..8])?,
        u32::from_le_bytes(array(&bytes[8..])?),
        offset,
    )
    .map_err(|_| corruption("timestamp scalar fields are invalid"))
}
fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| corruption("scalar fixed width is invalid"))
}
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| corruption("check byte range overflows"))?;
        let result = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| corruption("check program is truncated"))?;
        self.position = end;
        Ok(result)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(*self
            .bytes(1)?
            .first()
            .ok_or_else(|| corruption("check program is truncated"))?)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(array(self.bytes(2)?)?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(array(self.bytes(4)?)?))
    }
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
    fn u32_at(&self, position: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(array(
            self.bytes
                .get(
                    position
                        ..position
                            .checked_add(4)
                            .ok_or_else(|| corruption("check offset overflows"))?,
                )
                .ok_or_else(|| corruption("check program is truncated"))?,
        )?))
    }
}
fn pop(values: &mut Vec<Value>) -> Result<Value> {
    values
        .pop()
        .ok_or_else(|| corruption("check stack underflow"))
}
fn value_bytes(value: &Value) -> Result<u64> {
    let payload = match value {
        Value::Text(value) => u64::try_from(value.len()).map_err(|_| resource_limit())?,
        Value::Blob(value) => u64::try_from(value.len()).map_err(|_| resource_limit())?,
        _ => 0,
    };
    VALUE_BODY_BYTES
        .checked_add(payload)
        .ok_or_else(resource_limit)
}
struct Account<'a, M: CheckMemory> {
    memory: &'a mut M,
    used: u64,
}
impl<'a, M: CheckMemory> Account<'a, M> {
    fn new(memory: &'a mut M) -> Self {
        Self { memory, used: 0 }
    }
    fn reserve(&mut self, bytes: u64) -> Result<()> {
        let next = self.used.checked_add(bytes).ok_or_else(resource_limit)?;
        self.memory.reserve_check_bytes(bytes)?;
        self.used = next;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        let used = std::mem::take(&mut self.used);
        self.memory.release_check_bytes(used)
    }
}
fn corruption(message: &'static str) -> DbError {
    DbError::new(ErrorCode::Corruption, message)
}
fn resource_limit() -> DbError {
    DbError::new(
        ErrorCode::ResourceLimit,
        "check program resource limit exceeded",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::catalog::types::{CatalogName, TableId};

    fn columns() -> Vec<ColumnDef> {
        vec![
            ColumnDef::new(
                ColumnId::new(42).unwrap(),
                TableId::new(1).unwrap(),
                0,
                ColumnType::Int64,
                true,
                CatalogName::new("N").unwrap(),
            )
            .unwrap(),
        ]
    }
    fn program() -> CheckProgram {
        CheckProgramTemplate::new(vec![
            CheckTemplateInstruction::Column {
                ordinal: 0,
                result: CheckType::Int64,
            },
            CheckTemplateInstruction::Literal {
                value: Value::Int64(0),
                result: CheckType::Int64,
            },
            CheckTemplateInstruction::Binary {
                op: CheckBinaryOp::GreaterThan,
                result: CheckType::Bool,
            },
        ])
        .unwrap()
        .resolve(&[ColumnId::new(42).unwrap()])
        .unwrap()
    }
    #[test]
    fn check_program_v1_has_stable_postorder_bytes_and_column_ids() {
        let program = program();
        assert_eq!(&program.as_bytes()[..8], &[1, 0, 3, 0, 0, 0, 0, 0]);
        assert_eq!(program.validate(&columns()).unwrap(), CheckType::Bool);
    }
    #[test]
    fn every_check_program_byte_truncation_is_corruption() {
        let bytes = program().as_bytes().to_vec();
        for end in 0..bytes.len() {
            assert_eq!(
                CheckProgram::from_bytes(bytes[..end].into())
                    .unwrap_err()
                    .code(),
                ErrorCode::Corruption
            )
        }
    }
    struct Memory {
        limit: u64,
        used: u64,
    }
    impl CheckMemory for Memory {
        fn reserve_check_bytes(&mut self, bytes: u64) -> Result<()> {
            let next = self.used.checked_add(bytes).ok_or_else(resource_limit)?;
            if next > self.limit {
                return Err(resource_limit());
            }
            self.used = next;
            Ok(())
        }
        fn release_check_bytes(&mut self, bytes: u64) -> Result<()> {
            self.used = self
                .used
                .checked_sub(bytes)
                .ok_or_else(|| DbError::new(ErrorCode::Internal, "underflow"))?;
            Ok(())
        }
    }
    #[test]
    fn check_false_fails_but_true_and_null_pass() {
        let p = program();
        let mut m = Memory {
            limit: 10_000,
            used: 0,
        };
        assert_eq!(
            p.evaluate(&columns(), &[Value::Int64(-1)], &mut m).unwrap(),
            TruthValue::False
        );
        assert_eq!(m.used, 0);
        assert_eq!(
            p.evaluate(&columns(), &[Value::Int64(1)], &mut m).unwrap(),
            TruthValue::True
        );
        assert_eq!(
            p.evaluate(&columns(), &[Value::Null], &mut m).unwrap(),
            TruthValue::Unknown
        );
    }
}
