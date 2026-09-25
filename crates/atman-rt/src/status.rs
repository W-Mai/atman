use crate::{FlowOutcome, StatementOutcome, Value};

/// Portable terminal classification before a host formats or persists it.
pub enum FlowTermination<'a, E> {
    Ok,
    Cancelled,
    Errored(&'a E),
}

pub fn classify_result<'a, P, E>(
    result: &'a Result<Value<P, E>, E>,
    is_cancelled: impl Fn(&E) -> bool,
) -> FlowTermination<'a, E> {
    match result {
        Ok(Value::Err(error)) if is_cancelled(error) => FlowTermination::Cancelled,
        Ok(_) => FlowTermination::Ok,
        Err(error) if is_cancelled(error) => FlowTermination::Cancelled,
        Err(error) => FlowTermination::Errored(error),
    }
}

pub fn classify_outcome<'a, P, E>(
    outcome: &'a FlowOutcome<P, E>,
    is_cancelled: impl Fn(&E) -> bool,
) -> FlowTermination<'a, E> {
    match outcome {
        StatementOutcome::Err(error) if is_cancelled(error) => FlowTermination::Cancelled,
        StatementOutcome::Err(error) => FlowTermination::Errored(error),
        _ => FlowTermination::Ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_classification_preserves_cancelled_value_and_error_paths() {
        let cancellation = Ok::<Value<(), &str>, &str>(Value::Err("cancelled"));
        assert!(matches!(
            classify_result(&cancellation, |error| *error == "cancelled"),
            FlowTermination::Cancelled
        ));

        let failure = Err::<Value<(), &str>, &str>("failed");
        assert!(matches!(
            classify_result(&failure, |error| *error == "cancelled"),
            FlowTermination::Errored(&"failed")
        ));

        let non_cancelled_value = Ok::<Value<(), &str>, &str>(Value::Err("failed"));
        assert!(matches!(
            classify_result(&non_cancelled_value, |error| *error == "cancelled"),
            FlowTermination::Ok
        ));
    }

    #[test]
    fn statement_classification_consumes_loop_control_as_success() {
        let outcome = FlowOutcome::<(), &str>::LoopBreak;
        assert!(matches!(
            classify_outcome(&outcome, |_| false),
            FlowTermination::Ok
        ));
        let failure = FlowOutcome::<(), &str>::Err("cancelled");
        assert!(matches!(
            classify_outcome(&failure, |error| *error == "cancelled"),
            FlowTermination::Cancelled
        ));
    }
}
