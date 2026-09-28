use std::path::PathBuf;

use atman_rt::Vm;
use atman_rt::ast::FlowDecl;

use crate::atman_host::AtmanHost;
use crate::error::RuntimeError;
use crate::tool::{ToolCtx, ToolRegistry};
use crate::value::Value;

type StmtOutcome = atman_rt::StatementOutcome<Value, RuntimeError>;

pub async fn exec_flow(
    flow: &FlowDecl,
    args: Vec<(String, Value)>,
    tools: &ToolRegistry,
    tool_ctx: &ToolCtx,
    providers: &crate::provider::ProviderRegistry,
    source_dir: Option<PathBuf>,
) -> Result<Value, RuntimeError> {
    let flows = std::collections::HashMap::new();
    exec_flow_with_siblings(
        flow,
        args,
        tools,
        tool_ctx,
        providers,
        &flows,
        None,
        None,
        None,
        None,
        tokio_util::sync::CancellationToken::new(),
        None,
        source_dir,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn exec_flow_with_siblings(
    flow: &FlowDecl,
    args: Vec<(String, Value)>,
    tools: &ToolRegistry,
    tool_ctx: &ToolCtx,
    providers: &crate::provider::ProviderRegistry,
    flows: &std::collections::HashMap<String, FlowDecl>,
    events: Option<&crate::event::EventSink>,
    turn_id: Option<crate::event::TurnId>,
    flow_run_id: Option<crate::event::FlowRunId>,
    session: Option<std::sync::Arc<crate::session::Session>>,
    flow_cancel: tokio_util::sync::CancellationToken,
    safety: Option<&crate::safety::SafetyConfig>,
    source_dir: Option<PathBuf>,
) -> Result<Value, RuntimeError> {
    exec_flow_with_linked_siblings(
        flow,
        args,
        tools,
        tool_ctx,
        providers,
        flows,
        events,
        turn_id,
        flow_run_id,
        session,
        flow_cancel,
        safety,
        source_dir,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn exec_flow_with_linked_siblings(
    flow: &FlowDecl,
    args: Vec<(String, Value)>,
    tools: &ToolRegistry,
    tool_ctx: &ToolCtx,
    providers: &crate::provider::ProviderRegistry,
    flows: &std::collections::HashMap<String, FlowDecl>,
    events: Option<&crate::event::EventSink>,
    turn_id: Option<crate::event::TurnId>,
    flow_run_id: Option<crate::event::FlowRunId>,
    session: Option<std::sync::Arc<crate::session::Session>>,
    flow_cancel: tokio_util::sync::CancellationToken,
    safety: Option<&crate::safety::SafetyConfig>,
    source_dir: Option<PathBuf>,
    linked_program: Option<&crate::source_program::LinkedProgram>,
    current_module: Option<crate::source_program::ModuleId>,
) -> Result<Value, RuntimeError> {
    let synthesized_program = if linked_program.is_none() {
        let mut all_flows: Vec<_> = flows
            .values()
            .filter(|sibling| sibling.name.name != flow.name.name)
            .cloned()
            .collect();
        all_flows.sort_by(|left, right| left.name.name.cmp(&right.name.name));
        all_flows.push(flow.clone());
        let file = atman_rt::ast::File {
            flows: all_flows,
            ..Default::default()
        };
        Some(
            crate::source_program::link_inline(file)
                .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?,
        )
    } else {
        None
    };
    let linked_program = linked_program
        .or(synthesized_program.as_ref())
        .expect("linked or synthesized program");
    let module = current_module.unwrap_or_else(|| linked_program.entry_module());
    let ctx = AtmanHost {
        tools,
        tool_ctx: std::borrow::Cow::Borrowed(tool_ctx),
        providers,
        flows,
        linked_program: Some(linked_program),
        current_module: Some(module),
        allows_shell: crate::flow_authority::contract_allows_shell(flow.contract.as_ref()),
        events,
        turn_id,
        flow_run_id,
        session_runtime: session,
        flow_cancel,
        safety,
        current_node_id: None,
        source_dir,
    };
    let delegate = crate::vm_delegate::AtmanVmDelegate::from_host(&ctx)?;
    let vm = Vm::from_shared(linked_program.shared_core());
    let raw_outcome = vm
        .run_flow(
            atman_rt::program::FlowId {
                module,
                name: flow.name.name.clone(),
            },
            args,
            delegate,
        )
        .await;
    match raw_outcome {
        StmtOutcome::Return(v) => Ok(v),
        StmtOutcome::Err(e) => Err(e),
        StmtOutcome::Continue => Ok(Value::Unit),
        StmtOutcome::LoopBreak => Err(RuntimeError::ToolFailed("break outside loop".into())),
        StmtOutcome::LoopContinue => Err(RuntimeError::ToolFailed("continue outside loop".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_rt::parse_file;

    async fn run(src: &str, args: Vec<(String, Value)>) -> Result<Value, RuntimeError> {
        let file = parse_file(src).expect("parse test src");
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        exec_flow(&file.flows[0], args, &tools, &tool_ctx, &providers, None).await
    }

    #[tokio::test]
    async fn bind_and_return() {
        let out = run(
            r#"flow t() -> Int {
    x = 1
    y = x + 2
    return y
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(3)));
    }

    #[tokio::test]
    async fn when_true_executes_body() {
        let out = run(
            r#"flow t() -> Int {
    x = 5
    when x > 3 {
        return 42
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(42)));
    }

    #[tokio::test]
    async fn when_false_skips_body() {
        let out = run(
            r#"flow t() -> Int {
    x = 1
    when x > 3 {
        return 42
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(0)));
    }

    #[tokio::test]
    async fn err_in_bind_stops_flow() {
        let err = run(
            r#"flow t() -> Int {
    x = missing
    return 1
}
"#,
            vec![],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RuntimeError::UndefinedVar(n) if n == "missing"));
    }

    #[tokio::test]
    async fn flow_args_bind_before_body() {
        let out = run(
            r#"flow t() -> Int {
    return n + 1
}
"#,
            vec![("n".into(), Value::Int(4))],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(5)));
    }

    #[tokio::test]
    async fn flow_defaults_follow_parameter_order_and_explicit_args_win() {
        let source = r#"flow t(a: int = 7, b: int = a + 2) -> int {
    return b
}
"#;
        assert!(matches!(run(source, vec![]).await.unwrap(), Value::Int(9)));
        assert!(matches!(
            run(source, vec![("a".into(), Value::Int(3))])
                .await
                .unwrap(),
            Value::Int(5)
        ));
        assert!(matches!(
            run(source, vec![("b".into(), Value::Int(20))])
                .await
                .unwrap(),
            Value::Int(20)
        ));
    }

    #[tokio::test]
    async fn when_cond_unit_is_falsy() {
        let out = run(
            r#"flow t() -> Int {
    when 1 {
        return 1
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(1)));
    }

    #[tokio::test]
    async fn when_cond_struct_is_truthy() {
        let out = run(
            r#"flow t() -> Int {
    x = { a: 1 }
    when x {
        return 42
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(42)));
    }

    #[tokio::test]
    async fn flow_falls_through_to_unit_without_return() {
        let out = run(
            r#"flow t() {
    x = 1
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Unit));
    }

    #[tokio::test]
    async fn loop_continues_then_breaks_without_skipping_following_statement() {
        let out = run(
            r#"flow t() -> Int {
    n = 0
    loop {
        n = n + 1
        when n == 2 {
            continue
        }
        when n == 4 {
            break
        }
    }
    return n
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(4)));
    }

    #[tokio::test]
    async fn return_inside_loop_exits_flow() {
        let out = run(
            r#"flow t() -> Int {
    loop {
        return 7
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(7)));
    }
}
