use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Default)]
pub struct DataBudget {
    // Amount of bytes we have in the budget to send.
    bytes: AtomicUsize,
    // Last time that we upped the bytes count, used
    // to detect when to up the bytes budget again
    last_timestamp_ms: AtomicU64,
}

impl DataBudget {
    // If there are enough bytes in the budget, consumes from
    // the budget and returns true. Otherwise returns false.
    #[must_use]
    pub fn take(&self, size: usize) -> bool {
        let mut budget = self.bytes.load(Ordering::Acquire);
        loop {
            if budget < size {
                return false;
            }
            match self.bytes.compare_exchange_weak(
                budget,
                budget - size,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(bytes) => budget = bytes,
            }
        }
    }

    // Updates timestamp and returns true, if at least given milliseconds
    // has passed since last update. Otherwise returns false.
    fn can_update(&self, duration_millis: u64) -> bool {
        let now = solana_sdk::timing::timestamp();
        let mut last_timestamp = self.last_timestamp_ms.load(Ordering::Acquire);
        loop {
            if now < last_timestamp + duration_millis {
                return false;
            }
            match self.last_timestamp_ms.compare_exchange_weak(
                last_timestamp,
                now,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(ts) => last_timestamp = ts,
            }
        }
    }

    // Updates the budget if at least given milliseconds has passed since last
    // update. Updater function maps current value of bytes to the new one.
    pub fn update<F>(&self, duration_millis: u64, updater: F)
    where
        F: Fn(usize) -> usize,
    {
        if !self.can_update(duration_millis) {
            return;
        }
        let mut bytes = self.bytes.load(Ordering::Acquire);
        loop {
            match self.bytes.compare_exchange_weak(
                bytes,
                updater(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(b) => bytes = b,
            }
        }
    }

    // Non-atomic clone only for tests and simulations.
    pub fn clone_non_atomic(&self) -> Self {
        Self {
            bytes: AtomicUsize::new(self.bytes.load(Ordering::Acquire)),
            last_timestamp_ms: AtomicU64::new(self.last_timestamp_ms.load(Ordering::Acquire)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_data_budget() {
        let budget = DataBudget::default();
        assert!(!budget.take(1)); // budget = 0.

        budget.update(1000, |bytes| bytes + 5); // budget updates to 5.
        assert!(budget.take(1));
        assert!(budget.take(2));
        assert!(!budget.take(3)); // budget = 2, out of budget.

        budget.update(30, |_| 10); // no update, budget = 2.
        assert!(!budget.take(3)); // budget = 2, out of budget.

        std::thread::sleep(Duration::from_millis(50));
        budget.update(30, |bytes| bytes * 2); // budget updates to 4.

        assert!(budget.take(3));
        assert!(budget.take(1));
        assert!(!budget.take(1)); // budget = 0.
    }
}
