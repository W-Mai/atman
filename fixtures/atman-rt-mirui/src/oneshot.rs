use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Mutex::new(State::Pending(None)));
    (
        Sender {
            shared: Some(Arc::clone(&shared)),
        },
        Receiver { shared },
    )
}

enum State<T> {
    Pending(Option<Waker>),
    Ready(Option<T>),
    Closed,
}

pub struct Sender<T> {
    shared: Option<Arc<Mutex<State<T>>>>,
}

impl<T> Sender<T> {
    pub fn send(mut self, value: T) -> Result<(), T> {
        let shared = self.shared.take().expect("oneshot sender used once");
        let mut state = shared.lock().unwrap();
        let State::Pending(waiter) = &mut *state else {
            return Err(value);
        };
        let waiter = waiter.take();
        *state = State::Ready(Some(value));
        drop(state);
        if let Some(waiter) = waiter {
            waiter.wake();
        }
        Ok(())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        let mut state = shared.lock().unwrap();
        let State::Pending(waiter) = &mut *state else {
            return;
        };
        let waiter = waiter.take();
        *state = State::Closed;
        drop(state);
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}

pub struct Receiver<T> {
    shared: Arc<Mutex<State<T>>>,
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.shared.lock().unwrap();
        match &mut *state {
            State::Pending(waiter) => {
                if !waiter
                    .as_ref()
                    .is_some_and(|waiter| waiter.will_wake(context.waker()))
                {
                    *waiter = Some(context.waker().clone());
                }
                Poll::Pending
            }
            State::Ready(value) => {
                Poll::Ready(Ok(value.take().expect("oneshot value consumed once")))
            }
            State::Closed => Poll::Ready(Err(RecvError)),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        *self.shared.lock().unwrap() = State::Closed;
    }
}

#[derive(Debug)]
pub struct RecvError;
