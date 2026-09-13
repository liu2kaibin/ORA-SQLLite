/// The non-row result of statement execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionResult {
    rows_affected: u64,
}

impl ExecutionResult {
    #[allow(
        dead_code,
        reason = "execution results are constructed by later Phase 4C1 tasks"
    )]
    pub(crate) const fn new(rows_affected: u64) -> Self {
        Self { rows_affected }
    }

    #[cfg(test)]
    pub(crate) const fn new_for_test(rows_affected: u64) -> Self {
        Self::new(rows_affected)
    }

    pub const fn rows_affected(&self) -> u64 {
        self.rows_affected
    }
}
