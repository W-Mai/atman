use std::path::Path;
use std::sync::Arc;

use atman_rt::ast::{File, LifecycleDecl, LifecycleEvent};
use atman_rt::lifecycle::{lifecycle_event_slug, lifecycle_flow};

use crate::executor::{Executor, RootInvocation};
use crate::source_program::{LinkedProgram, SourceRoots, load_program};
use crate::value::Value;

pub struct LifecycleRunner {
    decls: Vec<LifecycleHook>,
}

struct LifecycleHook {
    decl: LifecycleDecl,
    program: Option<Arc<LinkedProgram>>,
}

impl LifecycleRunner {
    pub fn new() -> Self {
        Self { decls: Vec::new() }
    }

    pub fn from_dir(dir: &Path) -> Self {
        let mut runner = Self::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return runner;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("at") {
                continue;
            }
            let roots = SourceRoots {
                project_root: None,
                config_dir: Some(dir.to_owned()),
            };
            match load_program(&path, &roots) {
                Ok(program) => {
                    let program = Arc::new(program);
                    for decl in &program.entry_file().lifecycles {
                        runner.decls.push(LifecycleHook {
                            decl: decl.clone(),
                            program: Some(Arc::clone(&program)),
                        });
                    }
                }
                Err(error) => crate::notify!(
                    error,
                    location = Inline,
                    "lifecycle source {} failed to load: {error:#}",
                    path.display()
                ),
            }
        }
        runner
    }

    pub fn absorb(&mut self, file: &File) {
        for decl in &file.lifecycles {
            self.decls.push(LifecycleHook {
                decl: decl.clone(),
                program: None,
            });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.decls.iter().all(|hook| hook.decl.body.is_empty())
    }

    pub fn has(&self, event: LifecycleEvent) -> bool {
        self.decls.iter().any(|hook| hook.decl.event == event)
    }

    pub async fn fire(&self, executor: &Executor, event: LifecycleEvent) {
        for (idx, hook) in self.decls.iter().enumerate() {
            let decl = &hook.decl;
            if decl.event != event {
                continue;
            }
            let flow = lifecycle_flow(decl, idx);
            let flow_name = flow.name.name.clone();
            let result = if let Some(program) = hook.program.as_ref() {
                match program.with_entry_flow(flow) {
                    Ok(program) => {
                        executor
                            .run_linked_with_invocation(
                                &program,
                                &flow_name,
                                Vec::new(),
                                RootInvocation::default(),
                            )
                            .await
                    }
                    Err(error) => Err(crate::error::RuntimeError::ToolFailed(error.to_string())),
                }
            } else {
                let file = atman_rt::ast::File {
                    flows: vec![flow],
                    ..atman_rt::ast::File::default()
                };
                executor.run(&file, &flow_name, Vec::new()).await
            };
            match result {
                Ok(Value::Err(e)) => {
                    let key = format!("lifecycle.{}.returned_error", lifecycle_event_slug(event));
                    crate::notify!(
                        error,
                        location = Inline,
                        stack = dedupe(key, 60_000),
                        "lifecycle on {} returned error: {e}",
                        lifecycle_event_slug(event)
                    );
                }
                Err(e) => {
                    let key = format!("lifecycle.{}.run_failed", lifecycle_event_slug(event));
                    crate::notify!(
                        error,
                        location = Inline,
                        stack = dedupe(key, 60_000),
                        "lifecycle on {} failed to run: {e}",
                        lifecycle_event_slug(event)
                    );
                }
                Ok(_) => {}
            }
        }
    }
}

impl Default for LifecycleRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_rt::parse_file;

    fn drain_lifecycle_from(src: &str) -> LifecycleRunner {
        let file = parse_file(src).unwrap();
        let mut r = LifecycleRunner::new();
        r.absorb(&file);
        r
    }

    #[test]
    fn from_dir_picks_up_lifecycles_across_at_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.at"),
            "on session.start { }\non session.end { }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.at"), "on turn.start { }\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "on session.start { }\n").unwrap();

        let runner = LifecycleRunner::from_dir(dir.path());
        assert!(runner.has(LifecycleEvent::SessionStart));
        assert!(runner.has(LifecycleEvent::SessionEnd));
        assert!(runner.has(LifecycleEvent::TurnStart));
        assert!(!runner.has(LifecycleEvent::TurnEnd));
    }

    fn build_executor_with_todos(dir: &Path) -> Executor {
        let ex = Executor::new();
        crate::tools::register_tier_zero(&ex.tools);
        let todo = std::sync::Arc::new(crate::memory::TodoStore::at(dir));
        let confession = std::sync::Arc::new(crate::memory::ConfessionStore::at(dir));
        let goal = std::sync::Arc::new(crate::memory::GoalStore::at(dir));
        let plan = std::sync::Arc::new(crate::memory::PlanStore::at(dir));
        crate::tools::register_memory(&ex.tools, todo, confession, goal, plan);
        ex
    }

    fn set_todo_stmt(where_: &str) -> String {
        format!(
            r#"memory.todo.set(
                where: "{where_}",
                why: "test",
                how: "test",
                expected_result: "test"
            )"#
        )
    }

    #[tokio::test]
    async fn multiple_bodies_for_same_event_fire_in_declaration_order() {
        let src = format!(
            "on session.start {{ {} }}\non session.start {{ {} }}\n",
            set_todo_stmt("first"),
            set_todo_stmt("second"),
        );
        let runner = drain_lifecycle_from(&src);
        let dir = tempfile::tempdir().unwrap();
        let ex = build_executor_with_todos(dir.path());

        runner.fire(&ex, LifecycleEvent::SessionStart).await;

        let todos = std::fs::read_to_string(dir.path().join("todos.jsonl")).unwrap();
        let lines: Vec<&str> = todos.lines().collect();
        assert_eq!(lines.len(), 2, "todos: {todos}");
        let first_idx = lines
            .iter()
            .position(|l| l.contains("\"first\""))
            .expect("first missing");
        let second_idx = lines
            .iter()
            .position(|l| l.contains("\"second\""))
            .expect("second missing");
        assert!(first_idx < second_idx, "wrong order: {lines:?}");
    }

    #[tokio::test]
    async fn body_error_does_not_stop_later_bodies() {
        let src = format!(
            "on session.start {{ x = fs.read(@\"/no/such/path/definitely/not/real\") }}\n\
             on session.start {{ {} }}\n",
            set_todo_stmt("still_ran"),
        );
        let runner = drain_lifecycle_from(&src);
        let dir = tempfile::tempdir().unwrap();
        let ex = build_executor_with_todos(dir.path());

        runner.fire(&ex, LifecycleEvent::SessionStart).await;
        let todos = std::fs::read_to_string(dir.path().join("todos.jsonl")).unwrap();
        assert!(todos.contains("still_ran"), "todos: {todos}");
    }

    #[tokio::test]
    async fn fire_ignores_events_that_dont_match_declaration() {
        let src = format!(
            "on session.end {{ {} }}\n",
            set_todo_stmt("session_end_only")
        );
        let runner = drain_lifecycle_from(&src);
        let dir = tempfile::tempdir().unwrap();
        let ex = build_executor_with_todos(dir.path());

        runner.fire(&ex, LifecycleEvent::SessionStart).await;
        assert!(!dir.path().join("todos.jsonl").exists());

        runner.fire(&ex, LifecycleEvent::SessionEnd).await;
        let todos = std::fs::read_to_string(dir.path().join("todos.jsonl")).unwrap();
        assert!(todos.contains("session_end_only"), "todos: {todos}");
    }

    #[tokio::test]
    async fn discovered_hook_uses_its_loaded_flow_graph() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("lib");
        std::fs::create_dir(&library).unwrap();
        let helper = library.join("helper.at");
        std::fs::write(
            &helper,
            format!("pub flow create() {{ {} }}", set_todo_stmt("original")),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("hooks.at"),
            "use \"user:helper.at\" as helper\nflow local() { x = helper.create().await }\non session.start { x = local().await }",
        )
        .unwrap();
        let runner = LifecycleRunner::from_dir(dir.path());
        assert!(runner.has(LifecycleEvent::SessionStart));
        std::fs::write(
            &helper,
            format!("pub flow create() {{ {} }}", set_todo_stmt("updated")),
        )
        .unwrap();
        let ex = build_executor_with_todos(dir.path());
        runner.fire(&ex, LifecycleEvent::SessionStart).await;
        let todos = std::fs::read_to_string(dir.path().join("todos.jsonl")).unwrap();
        assert!(todos.contains("original"), "todos: {todos}");
        assert!(!todos.contains("updated"), "todos: {todos}");
    }
}
