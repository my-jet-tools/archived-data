use crate::ArchiveError;

/// Whether a call that changes a writer's state is still unfinished.
///
/// [`Self::begin`] marks a call in flight and [`Self::complete`] clears it, on that call's successful return
/// ONLY. So a call that returned an `Err` after changing state, and a call whose future was dropped at an
/// await, leave the mark behind alike — a dropped future runs no error path, which is why the mark is set
/// on the way in rather than on the way out — and every later call is refused.
#[derive(Debug, Default)]
pub(crate) struct InFlightGuard {
    in_flight: bool,
}

impl InFlightGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses when an earlier call never completed; changes nothing.
    pub fn refuse_if_in_flight(&self) -> Result<(), ArchiveError> {
        if self.in_flight {
            return Err(ArchiveError::ContractViolation(
                "an earlier write or flush failed or was dropped before it completed: the writer is \
                 not usable"
                    .to_string(),
            ));
        }

        Ok(())
    }

    /// Refuses like [`Self::refuse_if_in_flight`], then marks this call in flight.
    pub fn begin(&mut self) -> Result<(), ArchiveError> {
        self.refuse_if_in_flight()?;
        self.in_flight = true;
        Ok(())
    }

    /// The call begun last returned successfully — or was refused before it changed anything.
    pub fn complete(&mut self) {
        self.in_flight = false;
    }

    /// Clears the mark if `result` is `Ok`, and hands `result` back unchanged.
    pub fn complete_if_ok<T>(
        &mut self,
        result: Result<T, ArchiveError>,
    ) -> Result<T, ArchiveError> {
        if result.is_ok() {
            self.complete();
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused<T: std::fmt::Debug>(result: Result<T, ArchiveError>) -> String {
        match result {
            Err(ArchiveError::ContractViolation(message)) => message,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn a_call_that_never_completes_refuses_every_later_one() {
        let mut guard = InFlightGuard::new();
        guard.refuse_if_in_flight().unwrap();

        guard.begin().unwrap();
        refused(guard.refuse_if_in_flight());
        guard.complete();
        guard.begin().unwrap();
        assert_eq!(guard.complete_if_ok(Ok::<_, ArchiveError>(7)).unwrap(), 7);
        guard.refuse_if_in_flight().unwrap();

        guard.begin().unwrap();
        let failed: Result<(), _> = Err(ArchiveError::Corrupted("x".to_string()));
        assert!(matches!(
            guard.complete_if_ok(failed),
            Err(ArchiveError::Corrupted(_))
        ));

        let message = refused(guard.begin());
        assert_eq!(
            message,
            "an earlier write or flush failed or was dropped before it completed: the writer is not usable"
        );
        refused(guard.refuse_if_in_flight());
        refused(guard.begin());
    }

    #[tokio::test]
    async fn a_future_dropped_at_an_await_leaves_the_call_in_flight() {
        let mut guard = InFlightGuard::new();

        let call = async {
            guard.begin()?;
            std::future::pending::<()>().await;
            guard.complete();
            Ok::<_, ArchiveError>(())
        };

        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(1), call).await;
        assert!(timed_out.is_err());

        refused(guard.begin());
    }
}
