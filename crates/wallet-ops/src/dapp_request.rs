//! Ephemeral authority for a desktop-owned dapp request.
use crate::RpcBrokerError;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio::{sync::watch, time::Instant};

#[derive(Clone)]
pub struct DappRequestControl(Arc<Control>);
struct Control {
    deadline: Instant,
    check: Box<dyn Fn() -> Result<(), RpcBrokerError> + Send + Sync>,
    invalidation: watch::Sender<Option<RpcBrokerError>>,
    state: AtomicU8,
}
impl DappRequestControl {
    pub fn new(
        deadline: Instant,
        check: impl Fn() -> Result<(), RpcBrokerError> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Control {
            deadline,
            check: Box::new(check),
            invalidation: watch::channel(None).0,
            state: AtomicU8::new(0),
        }))
    }
    pub fn ensure_current(&self) -> Result<(), RpcBrokerError> {
        if let Some(error) = self.0.invalidation.borrow().clone() {
            return Err(error);
        }
        if self.0.state.load(Ordering::Acquire) & 2 != 0 {
            return Err(RpcBrokerError::Timeout);
        }
        (self.0.check)()?;
        if Instant::now() >= self.0.deadline {
            return Err(RpcBrokerError::Timeout);
        }
        Ok(())
    }
    pub fn invalidate(&self, error: &RpcBrokerError) {
        self.0.state.fetch_or(2, Ordering::AcqRel);
        self.0.invalidation.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(error.clone());
                true
            }
        });
    }
    pub fn before_broadcast(&self) -> Result<(), RpcBrokerError> {
        self.ensure_current()?;
        self.0
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                self.0
                    .invalidation
                    .borrow()
                    .clone()
                    .unwrap_or(RpcBrokerError::Timeout)
            })?;
        Ok(())
    }
    #[must_use]
    pub fn handed_off(&self) -> bool {
        self.0.state.load(Ordering::Acquire) & 1 != 0
    }
    pub async fn cancelled(&self) {
        let mut invalidation = self.0.invalidation.subscribe();
        loop {
            if self.ensure_current().is_err() {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep_until(self.0.deadline) => return,
                result = invalidation.changed() => if result.is_err() { return; },
            }
        }
    }
}
impl std::fmt::Debug for DappRequestControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DappRequestControl { .. }")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidation_between_authority_check_and_handoff_prevents_commit() {
        let entered = Arc::new(std::sync::Barrier::new(2));
        let released = Arc::new(std::sync::Barrier::new(2));
        let control =
            DappRequestControl::new(Instant::now() + std::time::Duration::from_secs(30), {
                let entered = entered.clone();
                let released = released.clone();
                move || {
                    entered.wait();
                    released.wait();
                    Ok(())
                }
            });
        let handoff = std::thread::spawn({
            let control = control.clone();
            move || control.before_broadcast()
        });
        entered.wait();
        control.invalidate(&RpcBrokerError::OriginRejected);
        released.wait();
        assert_eq!(handoff.join().unwrap(), Err(RpcBrokerError::OriginRejected));
        assert!(!control.handed_off());

        let committed =
            DappRequestControl::new(Instant::now() + std::time::Duration::from_secs(30), || {
                Ok(())
            });
        committed.before_broadcast().unwrap();
        committed.invalidate(&RpcBrokerError::Shutdown);
        assert!(committed.handed_off());
        assert_eq!(committed.before_broadcast(), Err(RpcBrokerError::Shutdown));
    }
}
