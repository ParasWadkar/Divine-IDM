//! The async half of the writer's memory budget.
//!
//! `idm_core::writer::ByteBudget` blocks, which is right for the writer threads
//! but wrong on an async fetcher: blocking there parks a runtime worker and
//! stalls every other download sharing it. This is the same budget expressed as
//! a `tokio` semaphore, handed to the writer through its type-erased
//! [`Permit`](idm_core::writer::Permit) so that `idm-core` needs no async
//! runtime of its own.
//!
//! # Why a byte budget is the backpressure mechanism
//!
//! When the budget is exhausted a fetcher simply stops polling its response
//! body. hyper then stops reading the socket, the TCP receive window closes,
//! and the server slows down. That is correct end-to-end flow control obtained
//! for free — no timers, no polling, no rate calculation — and it is why the
//! network can never outrun the disk by more than the budget.

use std::sync::Arc;

use idm_core::writer::{PERMIT_BYTES, Permit};
use tokio::sync::Semaphore;

/// A byte-denominated budget bounding how much downloaded data can be in
/// flight towards the disk.
#[derive(Debug, Clone)]
pub struct AsyncBudget {
    semaphore: Arc<Semaphore>,
    capacity_permits: u32,
}

impl AsyncBudget {
    /// `capacity_bytes` should be `min(256 MiB, 12.5% of available RAM,
    /// 16 MiB per connection)`. It is rounded down to whole permits, with at
    /// least one so the budget can never be zero-capacity.
    pub fn new(capacity_bytes: u64) -> Self {
        let permits = (capacity_bytes / PERMIT_BYTES).max(1);
        // tokio's semaphore counts in u32 and caps well below that.
        let capacity_permits = permits.min(u64::from(u32::MAX >> 4)) as u32;
        Self { semaphore: Arc::new(Semaphore::new(capacity_permits as usize)), capacity_permits }
    }

    /// The default budget for a given connection count.
    pub fn for_connections(connections: u32) -> Self {
        let per_connection = 16u64 << 20;
        let cap = (256u64 << 20).min(per_connection * u64::from(connections.max(1)));
        Self::new(cap)
    }

    pub fn capacity_bytes(&self) -> u64 {
        u64::from(self.capacity_permits) * PERMIT_BYTES
    }

    pub fn available_bytes(&self) -> u64 {
        self.semaphore.available_permits() as u64 * PERMIT_BYTES
    }

    pub fn in_use_bytes(&self) -> u64 {
        self.capacity_bytes().saturating_sub(self.available_bytes())
    }

    fn permits_for(&self, bytes: u64) -> u32 {
        // Round up: a partial permit still occupies memory.
        let wanted = bytes.div_ceil(PERMIT_BYTES).max(1);
        // Clamped so a single oversized chunk can never deadlock by asking for
        // more than the budget will ever hold.
        wanted.min(u64::from(self.capacity_permits)) as u32
    }

    /// Waits for room for `bytes`, yielding rather than blocking the runtime.
    ///
    /// The returned permit travels with the write job and is released once the
    /// write has been *issued*, so the budget reflects bytes still owed to the
    /// disk rather than bytes merely queued.
    pub async fn acquire(&self, bytes: u64) -> Permit {
        let permits = self.permits_for(bytes);
        let owned = Arc::clone(&self.semaphore)
            .acquire_many_owned(permits)
            .await
            // The semaphore is never closed while a budget exists.
            .expect("budget semaphore should not be closed");
        Box::new(owned) as Permit
    }

    /// Takes room only if it is immediately available.
    pub fn try_acquire(&self, bytes: u64) -> Option<Permit> {
        let permits = self.permits_for(bytes);
        Arc::clone(&self.semaphore)
            .try_acquire_many_owned(permits)
            .ok()
            .map(|owned| Box::new(owned) as Permit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_budget_bounds_bytes_in_flight() {
        let budget = AsyncBudget::new(4 * PERMIT_BYTES);
        assert_eq!(budget.capacity_bytes(), 4 * PERMIT_BYTES);

        let first = budget.acquire(2 * PERMIT_BYTES).await;
        let _second = budget.acquire(2 * PERMIT_BYTES).await;
        assert_eq!(budget.in_use_bytes(), 4 * PERMIT_BYTES);

        // Exhausted: a fetcher that cannot get a permit stops reading its
        // socket, which is what closes the TCP window.
        assert!(budget.try_acquire(PERMIT_BYTES).is_none(), "budget overcommitted");

        drop(first);
        assert_eq!(budget.in_use_bytes(), 2 * PERMIT_BYTES);
        assert!(budget.try_acquire(PERMIT_BYTES).is_some());
    }

    #[tokio::test]
    async fn an_oversized_request_is_clamped_rather_than_deadlocking() {
        let budget = AsyncBudget::new(PERMIT_BYTES);
        // Asking for a gigabyte from a 64 KiB budget must still complete.
        let permit = budget.acquire(1 << 30).await;
        assert_eq!(budget.in_use_bytes(), PERMIT_BYTES);
        drop(permit);
        assert_eq!(budget.in_use_bytes(), 0);
    }

    #[tokio::test]
    async fn a_waiting_acquire_resumes_when_room_is_released() {
        // The behaviour blocking would get wrong: this must yield, not park a
        // runtime worker.
        let budget = AsyncBudget::new(2 * PERMIT_BYTES);
        let held = budget.acquire(2 * PERMIT_BYTES).await;

        let waiting = {
            let budget = budget.clone();
            tokio::spawn(async move { budget.acquire(PERMIT_BYTES).await })
        };

        // Give the task a chance to park on the semaphore.
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "should still be waiting for room");

        drop(held);
        let permit = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("should not time out")
            .expect("task should not panic");
        drop(permit);
        assert_eq!(budget.in_use_bytes(), 0);
    }

    #[test]
    fn a_zero_capacity_budget_is_impossible() {
        // Zero capacity would deadlock every fetcher immediately.
        assert!(AsyncBudget::new(0).capacity_bytes() >= PERMIT_BYTES);
    }

    #[test]
    fn per_connection_sizing_is_capped() {
        assert_eq!(AsyncBudget::for_connections(1).capacity_bytes(), 16 << 20);
        assert_eq!(AsyncBudget::for_connections(4).capacity_bytes(), 64 << 20);
        // The 256 MiB ceiling holds however many connections are in play.
        assert_eq!(AsyncBudget::for_connections(64).capacity_bytes(), 256 << 20);
    }
}
