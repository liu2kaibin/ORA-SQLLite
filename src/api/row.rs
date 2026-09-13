use crate::Value;

/// One row returned by statement execution.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    values: Box<[Value]>,
}

impl Row {
    #[allow(
        dead_code,
        reason = "result rows are constructed by later Phase 4C1 tasks"
    )]
    pub(crate) fn new(values: Vec<Value>) -> Self {
        Self {
            values: values.into_boxed_slice(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_values_for_test(values: Vec<Value>) -> Self {
        Self::new(values)
    }

    pub const fn len(&self) -> usize {
        self.values.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }
}
