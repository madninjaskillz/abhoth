//! A small cancellation token (`CancellationToken` / `CancellationTokenSource.CreateLinkedTokenSource`):
//! cancelling a token cancels every child made from it; a child never cancels its parent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
    children: Mutex<Vec<Weak<Inner>>>,
}

impl Inner {
    fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        self.notify.notify_waiters();
        let children =
            std::mem::take(&mut *self.children.lock().unwrap_or_else(|e| e.into_inner()));
        for child in children.into_iter().filter_map(|w| w.upgrade()) {
            child.cancel();
        }
    }
}

/// Cheap to clone; all clones are the same token.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Inner>);

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CancelToken(cancelled: {})", self.is_cancelled())
    }
}

impl CancelToken {
    /// A token nobody cancels unless [`CancelToken::cancel`] is called (`CancellationToken.None`
    /// when never cancelled).
    pub fn new() -> Self {
        Self::default()
    }

    /// A linked token: cancelled with this one (or on its own).
    pub fn child(&self) -> CancelToken {
        let child = CancelToken::new();
        let mut children = self.0.children.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_cancelled() {
            child.cancel();
        } else {
            children.retain(|w| w.strong_count() > 0);
            children.push(Arc::downgrade(&child.0));
        }
        child
    }

    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Completes once the token is cancelled.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_a_parent_cancels_its_children_only_downwards() {
        let parent = CancelToken::new();
        let child = parent.child();
        let grandchild = child.child();
        grandchild.cancel();
        assert!(!child.is_cancelled() && !parent.is_cancelled());
        let other = parent.child();
        let waiter = tokio::spawn({
            let other = other.clone();
            async move { other.cancelled().await }
        });
        parent.cancel();
        waiter.await.unwrap();
        assert!(child.is_cancelled() && other.is_cancelled());
        assert!(parent.child().is_cancelled());
    }
}
