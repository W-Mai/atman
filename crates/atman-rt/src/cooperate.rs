use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub(crate) async fn yield_once() {
    YieldOnce { yielded: false }.await;
}

struct YieldOnce {
    yielded: bool,
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;
    use core::{
        pin::pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };
    use std::task::Wake;

    use super::*;

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn yields_once_and_self_wakes() {
        let wakes = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut context = Context::from_waker(&waker);
        let mut future = pin!(yield_once());

        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            future.as_mut().poll(&mut context),
            Poll::Ready(())
        ));
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    }
}
