//! A small cancellation token (the C# `CancellationToken` the engines take).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Cloneable; cancelling any clone cancels them all.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Inner>);

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Completes once the token is cancelled (at once when it already is).
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Cancel this token after `delay` (`CancellationTokenSource.CancelAfter`).
    pub fn cancel_after(&self, delay: std::time::Duration) {
        let token = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            token.cancel();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_completes_for_earlier_and_later_cancels() {
        let t = CancelToken::new();
        let c = t.clone();
        let waiter = tokio::spawn(async move { c.cancelled().await });
        tokio::task::yield_now().await;
        t.cancel();
        waiter.await.unwrap();
        // Already cancelled: returns at once.
        t.cancelled().await;
        assert!(t.is_cancelled());
    }
}
