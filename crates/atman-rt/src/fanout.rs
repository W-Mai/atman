use core::future::Future;

use crate::Value;

/// Polls all branches concurrently, reports each result in source order, and returns the first error.
pub async fn join_fanout_all<P, E, F, I, End>(branches: I, mut end: End) -> Value<P, E>
where
    I: IntoIterator<Item = F>,
    F: Future<Output = Value<P, E>>,
    E: Clone,
    End: FnMut(usize, &Value<P, E>),
{
    let results = futures::future::join_all(branches).await;
    for (index, result) in results.iter().enumerate() {
        end(index, result);
    }
    for result in &results {
        if let Value::Err(error) = result {
            return Value::Err(error.clone());
        }
    }
    Value::List(results)
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, rc::Rc, vec, vec::Vec};
    use core::{
        cell::{Cell, RefCell},
        future::{Future, poll_fn},
        pin::{Pin, pin},
        task::{Context, Poll, Waker},
    };

    use super::*;

    type BranchFuture = Pin<Box<dyn Future<Output = Value<(), &'static str>>>>;

    #[test]
    fn fanout_polls_branches_together_then_reports_in_source_order() {
        let second_polled = Rc::new(Cell::new(false));
        let first_flag = Rc::clone(&second_polled);
        let second_flag = Rc::clone(&second_polled);
        let branches: Vec<BranchFuture> = vec![
            Box::pin(poll_fn(move |_| {
                if first_flag.get() {
                    Poll::Ready(Value::Int(1))
                } else {
                    Poll::Pending
                }
            })),
            Box::pin(poll_fn(move |_| {
                second_flag.set(true);
                Poll::Ready(Value::Int(2))
            })),
        ];
        let ended = Rc::new(RefCell::new(Vec::new()));
        let ended_for_callback = Rc::clone(&ended);
        let future = join_fanout_all(branches, move |index, _| {
            ended_for_callback.borrow_mut().push(index);
        });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        let result = (0..4)
            .find_map(|_| match future.as_mut().poll(&mut context) {
                Poll::Ready(result) => Some(result),
                Poll::Pending => None,
            })
            .expect("all branches must finish after the second branch is polled");
        assert!(
            matches!(result, Value::List(items) if matches!(&items[..], [Value::Int(1), Value::Int(2)]))
        );
        assert_eq!(*ended.borrow(), vec![0, 1]);
    }

    #[test]
    fn fanout_reports_every_branch_before_returning_first_error() {
        let mut ended = Vec::new();
        let result = {
            let future = join_fanout_all(
                [
                    core::future::ready(Value::<(), &str>::Err("first")),
                    core::future::ready(Value::Err("second")),
                ],
                |index, _| ended.push(index),
            );
            let mut future = pin!(future);
            match future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                Poll::Ready(result) => result,
                Poll::Pending => panic!("ready branches must finish in one poll"),
            }
        };
        assert!(matches!(result, Value::Err("first")));
        assert_eq!(ended, vec![0, 1]);
    }
}
