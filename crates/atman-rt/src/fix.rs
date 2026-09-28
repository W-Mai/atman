//! Language-level retry loop for `fix_until_test_passes`.

use alloc::{boxed::Box, format, string::String, vec};

use crate::{
    Env, HostFuture, Value, ValueError,
    ast::{Expr, Kwargs},
    expr::{ExpressionEffect, ExpressionHost, eval_expr},
};

pub fn eval_fix_until_test_passes<'a, H: ExpressionHost>(
    kwargs: &'a Kwargs,
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> HostFuture<'a, Value<H::Payload, H::Error>> {
    Box::pin(async move {
        let mut edit_flow_expr: Option<&Expr> = None;
        let mut test_expr: Option<&Expr> = None;
        let mut on_giveup_expr: Option<&Expr> = None;
        let mut max_iters: u32 = 5;
        let mut target = None;

        for (name, expr) in kwargs {
            match name.name.as_str() {
                "edit_flow" => edit_flow_expr = Some(expr),
                "test" => test_expr = Some(expr),
                "on_giveup" => on_giveup_expr = Some(expr),
                "max_iters" => match eval_expr(expr, env, host).await {
                    Value::Int(n) if n > 0 && u32::try_from(n).is_ok() => {
                        max_iters = n as u32;
                    }
                    Value::Err(error) => return Value::Err(error),
                    other => {
                        return Value::Err(H::Error::type_mismatch(
                            "positive int (max_iters)",
                            other.kind_name().into(),
                        ));
                    }
                },
                "target" => match eval_expr(expr, env, host).await {
                    Value::Unit => {}
                    value @ (Value::Str(_) | Value::Host(_)) => target = Some(value),
                    Value::Err(error) => return Value::Err(error),
                    other => {
                        return Value::Err(H::Error::type_mismatch(
                            "path (target)",
                            other.kind_name().into(),
                        ));
                    }
                },
                _ => {}
            }
        }

        let Some(edit_flow_expr) = edit_flow_expr else {
            return Value::Err(H::Error::missing_argument(
                "fix_until_test_passes.edit_flow",
            ));
        };
        let Some(test_expr) = test_expr else {
            return Value::Err(H::Error::missing_argument("fix_until_test_passes.test"));
        };

        let pristine = if let Some(target) = target.as_ref() {
            match host
                .eval_external(ExpressionEffect::FixSnapshot {
                    target: target.clone(),
                })
                .await
            {
                Value::Str(text) => Some(text),
                Value::Err(error) => return Value::Err(error),
                other => {
                    return Value::Err(H::Error::type_mismatch(
                        "string (fix snapshot)",
                        other.kind_name().into(),
                    ));
                }
            }
        } else {
            None
        };

        let mut prev_fail = String::new();
        let mut last_test_result = None;
        for iter in 0..max_iters {
            let mut loop_env = env.clone();
            loop_env.bind("iter", Value::Int(iter as i64));
            loop_env.bind("prev_fail", Value::Str(prev_fail.clone()));

            let edit = eval_expr(edit_flow_expr, &loop_env, host).await;
            if edit.is_err() {
                return edit;
            }
            loop_env.bind("last_edit", edit);

            let test = eval_expr(test_expr, &loop_env, host).await;
            if test.is_err() {
                return test;
            }
            let exit = test
                .field("exit_code")
                .or_else(|| test.field("exit"))
                .and_then(|value| match value {
                    Value::Int(number) => Some(*number),
                    _ => None,
                });
            last_test_result = Some(test.clone());
            if exit == Some(0) {
                return Value::Struct(vec![
                    ("status".into(), Value::Str("passed".into())),
                    ("iters".into(), Value::Int(i64::from(iter + 1))),
                    ("test".into(), test),
                ]);
            }
            let stderr_tail = test
                .field("stderr_tail")
                .or_else(|| test.field("output"))
                .and_then(|value| match value {
                    Value::Str(text) => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let stdout_tail = test
                .field("stdout_tail")
                .and_then(|value| match value {
                    Value::Str(text) => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            prev_fail = format!(
                "iter {iter} exit={exit:?}\n--- stderr ---\n{stderr_tail}\n--- stdout ---\n{stdout_tail}"
            );

            if let (Some(target), Some(pristine)) = (&target, &pristine) {
                let restored = host
                    .eval_external(ExpressionEffect::FixRestore {
                        target: target.clone(),
                        pristine: pristine.clone(),
                    })
                    .await;
                if restored.is_err() {
                    return restored;
                }
            }
        }

        if let Some(giveup) = on_giveup_expr {
            let mut giveup_env = env.clone();
            giveup_env.bind("iters", Value::Int(i64::from(max_iters)));
            giveup_env.bind("prev_fail", Value::Str(prev_fail));
            return eval_expr(giveup, &giveup_env, host).await;
        }

        Value::Struct(vec![
            ("status".into(), Value::Str("gave_up".into())),
            ("iters".into(), Value::Int(i64::from(max_iters))),
            ("last_test".into(), last_test_result.unwrap_or(Value::Unit)),
        ])
    })
}
