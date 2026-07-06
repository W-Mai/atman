use std::path::Path;

use atman_dsl::ast::{File, LifecycleDecl, LifecycleEvent};

use crate::executor::Executor;
use crate::value::Value;

pub struct LifecycleRunner {
    decls: Vec<LifecycleDecl>,
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
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(file) = atman_dsl::parse::parse_file(&source) else {
                continue;
            };
            runner.absorb(&file);
        }
        runner
    }

    pub fn absorb(&mut self, file: &File) {
        for decl in &file.lifecycles {
            self.decls.push(decl.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.decls.iter().all(|d| d.body.is_empty())
    }

    pub fn has(&self, event: LifecycleEvent) -> bool {
        self.decls.iter().any(|d| d.event == event)
    }

    pub async fn fire(&self, executor: &Executor, event: LifecycleEvent) {
        for (idx, decl) in self.decls.iter().enumerate() {
            if decl.event != event {
                continue;
            }
            let flow_name = format!("__lifecycle_{}_{idx}", lifecycle_event_slug(event));
            let flow = atman_dsl::ast::FlowDecl {
                name: atman_dsl::ast::Ident {
                    name: flow_name.clone(),
                    span: decl.span,
                },
                params: Vec::new(),
                ret: None,
                contract: None,
                body: decl.body.clone(),
            };
            let file = atman_dsl::ast::File {
                flows: vec![flow],
                routes: Vec::new(),
                default_route: None,
                lifecycles: Vec::new(),
            };
            match executor.run(&file, &flow_name, Vec::new()).await {
                Ok(Value::Err(e)) => {
                    eprintln!("[atman] on {} body error: {e}", lifecycle_event_slug(event));
                }
                Err(e) => {
                    eprintln!("[atman] on {} body error: {e}", lifecycle_event_slug(event));
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

fn lifecycle_event_slug(event: LifecycleEvent) -> &'static str {
    match event {
        LifecycleEvent::SessionStart => "session.start",
        LifecycleEvent::SessionEnd => "session.end",
        LifecycleEvent::TurnStart => "turn.start",
        LifecycleEvent::TurnEnd => "turn.end",
        LifecycleEvent::ContextCompact => "session.context_compact",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

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
        let mut ex = Executor::new();
        crate::tools::register_tier_zero(&mut ex.tools);
        let todo = std::sync::Arc::new(crate::memory::TodoStore::at(dir));
        let confession = std::sync::Arc::new(crate::memory::ConfessionStore::at(dir));
        let goal = std::sync::Arc::new(crate::memory::GoalStore::at(dir));
        crate::tools::register_memory(&mut ex.tools, todo, confession, goal);
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
}
