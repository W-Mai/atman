use core::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

/// Runs a flow until it finishes or cancellation resolves, preferring cancellation when both are ready.
pub async fn race_cancel<F, C, V, E>(run: F, cancelled: C) -> Result<V, E>
where
    F: Future<Output = Result<V, E>>,
    C: Future<Output = E>,
{
    let mut run = pin!(run);
    let mut cancelled = pin!(cancelled);
    poll_fn(|cx| {
        if let Poll::Ready(error) = cancelled.as_mut().poll(cx) {
            return Poll::Ready(Err(error));
        }
        run.as_mut().poll(cx)
    })
    .await
}

#[cfg(test)]
mod tests {
    use core::{
        future::{Future, pending},
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use super::*;

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test future must complete synchronously"),
        }
    }

    #[test]
    fn cancellation_wins_when_both_futures_are_ready() {
        let result = run_ready(race_cancel(async { Ok::<_, &'static str>(7) }, async {
            "cancelled"
        }));
        assert_eq!(result, Err("cancelled"));
    }

    #[test]
    fn completed_run_wins_while_cancellation_is_pending() {
        let result = run_ready(race_cancel(
            async { Ok::<_, &'static str>(7) },
            pending::<&'static str>(),
        ));
        assert_eq!(result, Ok(7));
    }
}
