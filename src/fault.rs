use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FaultPoint {
    BeforeFileWrite,
    AfterFileWrite,
    BeforeFileSync,
    AfterFileSync,
    BeforeCheckpointPublish,
    AfterCheckpointPublish,
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
            FaultPoint::BeforeFileWrite,
            FaultPoint::AfterFileWrite,
            FaultPoint::BeforeFileSync,
            FaultPoint::AfterFileSync,
            FaultPoint::BeforeCheckpointPublish,
            FaultPoint::AfterCheckpointPublish,
        ] {
            injector.check(point).expect("NoFaults must succeed");
        }
    }
}
