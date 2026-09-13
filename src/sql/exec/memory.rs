use crate::{DbError, ErrorCode, Result, Value};

/// Deterministic, query-local accounting, not an allocator measurement.
#[derive(Debug)]
pub(crate) struct WorkMemory {
    limit: u64,
    used: u64,
}

impl WorkMemory {
    pub(crate) fn new(limit: u64) -> Result<Self> {
        if limit == 0 {
            return Err(DbError::new(
                ErrorCode::Type,
                "query memory limit must be nonzero",
            ));
        }
        Ok(Self { limit, used: 0 })
    }

    /// Reserve before retaining the candidate. Failure leaves accounting intact.
    pub(crate) fn reserve(&mut self, bytes: u64) -> Result<()> {
        let next = self.used.checked_add(bytes).ok_or_else(resource_limit)?;
        if next > self.limit {
            return Err(resource_limit());
        }
        self.used = next;
        Ok(())
    }

    pub(crate) const fn used(&self) -> u64 {
        self.used
    }

    pub(crate) const fn remaining(&self) -> u64 {
        self.limit - self.used
    }

    pub(crate) fn release(&mut self, bytes: u64) -> Result<()> {
        self.used = self.used.checked_sub(bytes).ok_or_else(|| {
            DbError::new(
                ErrorCode::Internal,
                "query work memory accounting underflow",
            )
        })?;
        Ok(())
    }
}

pub(super) fn resource_limit() -> DbError {
    DbError::new(ErrorCode::ResourceLimit, "query work memory limit exceeded")
}

pub(crate) fn checked_array_bytes(count: u64, element_bytes: u64) -> Result<u64> {
    count.checked_mul(element_bytes).ok_or_else(resource_limit)
}

/// Value bodies plus owned payload bytes and conservative container overhead.
pub(crate) fn owned_values_bytes<'a>(values: impl IntoIterator<Item = &'a Value>) -> Result<u64> {
    let mut count = 0_u64;
    let mut bytes = 32_u64;
    for value in values {
        count = count.checked_add(1).ok_or_else(resource_limit)?;
        let payload = match value {
            Value::Text(text) => text.len(),
            Value::Blob(blob) => blob.len(),
            _ => 0,
        };
        bytes = bytes
            .checked_add(u64::try_from(payload).map_err(|_| resource_limit())?)
            .ok_or_else(resource_limit)?;
    }
    bytes
        .checked_add(checked_array_bytes(
            count,
            std::mem::size_of::<Value>() as u64,
        )?)
        .ok_or_else(resource_limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorCode, Value};

    // A failed reservation must not consume budget, including integer overflow.
    #[test]
    fn reservations_are_checked_and_atomic_at_both_boundaries() {
        assert_eq!(WorkMemory::new(0).unwrap_err().code(), ErrorCode::Type);
        let mut memory = WorkMemory::new(10).unwrap();
        memory.reserve(10).unwrap();
        assert_eq!(
            memory.reserve(1).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(memory.used(), 10);
        memory.release(4).unwrap();
        assert_eq!(memory.used(), 6);
        assert_eq!(memory.remaining(), 4);
        assert_eq!(memory.release(7).unwrap_err().code(), ErrorCode::Internal);
        assert_eq!(memory.used(), 6);
        memory.reserve(0).unwrap();
        let mut memory = WorkMemory::new(u64::MAX).unwrap();
        memory.reserve(u64::MAX).unwrap();
        assert_eq!(
            memory.reserve(1).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
        assert_eq!(memory.used(), u64::MAX);
        assert_eq!(
            checked_array_bytes(u64::MAX, 2).unwrap_err().code(),
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn accounting_includes_value_bodies_utf8_blob_and_container() {
        let values = [
            Value::Text("界".into()),
            Value::Blob(vec![1, 2]),
            Value::Null,
        ];
        let bodies = 3 * std::mem::size_of::<Value>() as u64;
        assert_eq!(owned_values_bytes(values.iter()).unwrap(), bodies + 5 + 32);
        assert_eq!(owned_values_bytes([].iter()).unwrap(), 32);
    }
}
