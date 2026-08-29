use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::FutureExt;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CapturedPanic;

std::thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct CaptureScope;

impl CaptureScope {
    fn enter() -> Self {
        DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for CaptureScope {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

struct CapturedFuture<F> {
    inner: Pin<Box<F>>,
}

impl<F> CapturedFuture<F> {
    fn new(inner: F) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl<F: Future> Future for CapturedFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let _scope = CaptureScope::enter();
        self.inner.as_mut().poll(context)
    }
}

/// Reports whether the current thread is inside an active capture boundary.
pub(crate) fn is_active() -> bool {
    DEPTH.with(|depth| depth.get() > 0)
}

fn discard_payload(payload: Box<dyn std::any::Any + Send + 'static>) {
    let _scope = CaptureScope::enter();
    if let Err(nested) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(nested);
    }
}

pub(crate) async fn future<F: Future>(future: F) -> Result<F::Output, CapturedPanic> {
    match std::panic::AssertUnwindSafe(CapturedFuture::new(future))
        .catch_unwind()
        .await
    {
        Ok(output) => Ok(output),
        Err(payload) => {
            discard_payload(payload);
            Err(CapturedPanic)
        }
    }
}

pub(crate) fn blocking<T>(operation: impl FnOnce() -> T) -> Result<T, CapturedPanic> {
    let _scope = CaptureScope::enter();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(output) => Ok(output),
        Err(payload) => {
            discard_payload(payload);
            Err(CapturedPanic)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn blocking_capture_is_scoped_and_nestable() {
        assert!(!is_active());
        let result = blocking(|| {
            assert!(is_active());
            blocking(|| {
                assert!(is_active());
            })
            .unwrap();
            panic!("captured blocking panic");
        });
        assert!(result.is_err());
        assert!(!is_active());
    }

    #[test]
    fn panicking_payload_drop_is_contained() {
        struct PanickingDrop;

        impl Drop for PanickingDrop {
            fn drop(&mut self) {
                panic!("panic payload drop fixture");
            }
        }

        let result = blocking(|| std::panic::panic_any(PanickingDrop));
        assert!(result.is_err());
        assert!(!is_active());
    }

    #[test]
    fn future_capture_is_poll_scoped_across_threads() {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let future = future(std::future::poll_fn(move |_context| {
            assert!(is_active());
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
        let mut future = Box::pin(future);
        let waker = futures::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        assert!(!is_active());

        std::thread::spawn(move || {
            let waker = futures::task::noop_waker_ref();
            let mut context = Context::from_waker(waker);
            assert!(matches!(
                future.as_mut().poll(&mut context),
                Poll::Ready(Ok(()))
            ));
            assert!(!is_active());
        })
        .join()
        .unwrap();

        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert!(!is_active());
    }

    #[tokio::test]
    async fn future_capture_resets_after_panic() {
        let result = future(async {
            assert!(is_active());
            panic!("captured async panic");
        })
        .await;

        assert!(result.is_err());
        assert!(!is_active());
    }
}
