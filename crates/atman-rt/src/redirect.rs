use alloc::string::String;
use core::future::Future;

/// The result of following a bounded chain of flow redirects.
pub enum RedirectOutcome<T, E> {
    Completed(Result<T, E>),
    LimitExceeded,
}

/// Runs each redirect target without forwarding the previous flow's arguments.
pub async fn run_redirects<A, T, E, F, Fut, R>(
    initial_flow: String,
    initial_args: A,
    max_runs: usize,
    mut run: F,
    redirect_target: R,
) -> RedirectOutcome<T, E>
where
    A: Default,
    F: FnMut(String, A) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    R: Fn(&E) -> Option<String>,
{
    let mut flow = initial_flow;
    let mut args = initial_args;
    for _ in 0..max_runs {
        match run(flow, args).await {
            Err(error) => match redirect_target(&error) {
                Some(target) => {
                    flow = target;
                    args = A::default();
                }
                None => return RedirectOutcome::Completed(Err(error)),
            },
            result => return RedirectOutcome::Completed(result),
        }
    }
    RedirectOutcome::LimitExceeded
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec, vec::Vec};
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test flow must complete synchronously"),
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Error {
        Redirect(&'static str),
        Failed,
    }

    #[test]
    fn redirect_clears_args_and_keeps_non_redirect_errors() {
        let mut calls = Vec::new();
        let outcome = run_ready(run_redirects(
            "first".to_string(),
            vec![1],
            5,
            |flow, args| {
                calls.push((flow.clone(), args));
                async move {
                    match flow.as_str() {
                        "first" => Err::<(), _>(Error::Redirect("second")),
                        _ => Err::<(), _>(Error::Failed),
                    }
                }
            },
            |error| match error {
                Error::Redirect(target) => Some((*target).to_string()),
                Error::Failed => None,
            },
        ));
        assert_eq!(
            calls,
            vec![
                ("first".to_string(), vec![1]),
                ("second".to_string(), vec![])
            ]
        );
        assert!(matches!(
            outcome,
            RedirectOutcome::Completed(Err(Error::Failed))
        ));
    }

    #[test]
    fn redirect_stops_after_exact_run_limit() {
        let mut runs = 0;
        let outcome = run_ready(run_redirects(
            "first".to_string(),
            (),
            5,
            |_, _| {
                runs += 1;
                async { Err::<(), _>(Error::Redirect("first")) }
            },
            |error| match error {
                Error::Redirect(target) => Some((*target).to_string()),
                Error::Failed => None,
            },
        ));
        assert_eq!(runs, 5);
        assert!(matches!(outcome, RedirectOutcome::LimitExceeded));
    }
}
