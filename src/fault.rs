use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FaultPoint {
    BeforeMainExtend,
    AfterMainExtend,
    BeforeMainPageWrite,
    AfterMainPageWrite,
    BeforeMainSync,
    AfterMainSync,
    BeforeSuperblockWrite,
    AfterSuperblockWrite,
    BeforeWalRecordWrite,
    AfterWalRecordWrite,
    BeforeWalHeaderWrite,
    AfterWalHeaderWrite,
    BeforeWalSync,
    AfterWalSync,
    BeforeWalTruncate,
    AfterWalTruncate,
}

pub(crate) trait FaultInjector: Send + Sync {
    fn check(&self, point: FaultPoint) -> Result<()>;
}

pub(crate) struct NoFaults;

impl FaultInjector for NoFaults {
    fn check(&self, _point: FaultPoint) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FaultInjector, FaultPoint, NoFaults};

    #[test]
    fn production_default_never_injects() {
        let injector = NoFaults;
        for point in [
            FaultPoint::BeforeMainExtend,
            FaultPoint::AfterMainExtend,
            FaultPoint::BeforeMainPageWrite,
            FaultPoint::AfterMainPageWrite,
            FaultPoint::BeforeMainSync,
            FaultPoint::AfterMainSync,
            FaultPoint::BeforeSuperblockWrite,
            FaultPoint::AfterSuperblockWrite,
            FaultPoint::BeforeWalRecordWrite,
            FaultPoint::AfterWalRecordWrite,
            FaultPoint::BeforeWalHeaderWrite,
            FaultPoint::AfterWalHeaderWrite,
            FaultPoint::BeforeWalSync,
            FaultPoint::AfterWalSync,
            FaultPoint::BeforeWalTruncate,
            FaultPoint::AfterWalTruncate,
        ] {
            injector.check(point).expect("NoFaults must succeed");
        }
    }
}
