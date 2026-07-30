use atman_tui::mermaid::{charts, diagrams, render, sequence, state};
use atman_tui::width;

fn join_lines(lines: &[String]) -> String {
    lines.join("\n")
}

fn non_empty_widths(lines: &[String]) -> Vec<usize> {
    lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| width::width(l))
        .collect()
}

// Bug 1: Sequence self-message label is rendered vertically.
#[test]
fn seq_self_message_label_horizontal() {
    let sd = sequence::parse_sequence("sequenceDiagram\nA->>A: self call");
    let lines = sequence::render_sequence(&sd);
    let all = join_lines(&lines);

    assert!(
        all.contains("self call"),
        "self-message label should appear horizontally:\n{all}"
    );
    assert!(
        !all.contains("s\ne\nl\nf"),
        "self-message label must not be split vertically:\n{all}"
    );
}

// Bug 2: Gantt milestone is rendered as a full bar instead of a single ◆.
#[test]
fn gantt_milestone_is_single_diamond() {
    let chart = charts::parse_gantt(
        "gantt\ndateFormat YYYY-MM-DD\nsection S\nmilestone :m1, 2024-01-05, 0d",
    );
    let lines = charts::render_gantt(&chart);
    let all = join_lines(&lines);

    assert!(
        all.contains('◆'),
        "milestone marker ◆ should appear:\n{all}"
    );

    let long_bar = "█".repeat(8);
    assert!(
        !all.contains(long_bar.as_str()),
        "milestone must not render as a long █ bar:\n{all}"
    );
}

// Bug 3: Flowchart thick edge loses its thick glyphs.
#[test]
fn flowchart_thick_edge_keeps_thick_glyphs() {
    let lines = render::render_flowchart("flowchart TB\nA ==> B", 80);
    let all = join_lines(&lines);

    assert!(
        all.contains('═') || all.contains('║'),
        "thick edge should use ═/║ glyphs:\n{all}"
    );
}

// Bug 3: Flowchart dotted edge loses its dotted glyphs.
#[test]
fn flowchart_dotted_edge_keeps_dotted_glyphs() {
    let lines = render::render_flowchart("flowchart TB\nA -.-> B", 80);
    let all = join_lines(&lines);

    assert!(
        all.contains('┄') || all.contains('┆'),
        "dotted edge should use ┄/┆ glyphs:\n{all}"
    );
}

// Bug 4: Stadium shape is rendered like a plain rectangle.
#[test]
fn flowchart_stadium_shape_uses_parentheses() {
    let lines = render::render_flowchart("flowchart TB\nA([开始])", 80);
    let all = join_lines(&lines);

    assert!(
        all.contains('(') || all.contains(')'),
        "stadium node should use rounded parentheses:\n{all}"
    );
    assert!(
        !all.contains('╭') && !all.contains('╰'),
        "stadium node should not use plain box corners:\n{all}"
    );
}

// Bug 5: Diamond shape is rendered like a box-like placeholder instead of a true diamond.
#[test]
fn flowchart_diamond_shape_renders_as_diamond() {
    let lines = render::render_flowchart("flowchart TB\nA{判断}", 80);
    let all = join_lines(&lines);

    assert!(all.contains("判断"), "diamond label should appear:\n{all}");
    assert!(
        all.contains('◇'),
        "diamond shape should be visible as diamond glyphs:\n{all}"
    );
    assert!(
        !all.contains('╭') && !all.contains('╰'),
        "diamond node should not fall back to a plain box:\n{all}"
    );
}

// Bug 6: Composite state loses its outer state label/border.
#[test]
fn state_composite_state_label_visible() {
    let lines = state::render_state_diagram(
        "stateDiagram-v2\nstate Active {\n  [*] --> Running\n}\n[*] --> Active",
        80,
    );
    let all = join_lines(&lines);

    assert!(
        all.contains("Active"),
        "composite state label should appear:\n{all}"
    );
    assert!(
        all.contains("Running"),
        "inner state should still be visible:\n{all}"
    );
}

// Bug 7: Sequence loop/alt/opt blocks are ignored.
#[test]
fn sequence_loop_block_is_rendered() {
    let sd = sequence::parse_sequence(
        "sequenceDiagram\nA->>B: start\nloop until done\nB->>A: check\nend",
    );
    let lines = sequence::render_sequence(&sd);
    let all = join_lines(&lines);

    assert!(
        all.contains("loop"),
        "loop block text should appear:\n{all}"
    );
    assert!(
        all.contains("until done"),
        "loop block condition should appear:\n{all}"
    );
}

// Bug 8: ER relationship symbol is not rendered.
#[test]
fn er_relationship_symbol_is_rendered() {
    let er = diagrams::parse_er("erDiagram\nUSER ||--o{ POST : writes");
    let lines = diagrams::render_er(&er);
    let all = join_lines(&lines);

    assert!(
        all.contains("writes"),
        "relationship label should appear:\n{all}"
    );
    assert!(
        all.contains("||--o{") || all.contains("o{"),
        "ER relationship symbol should be visible in output:\n{all}"
    );
}

// Bug 9: Flowchart subgraph border/label is missing.
#[test]
fn flowchart_subgraph_label_rendered() {
    let lines = render::render_flowchart("flowchart TB\nsubgraph G1 [Group1]\nA --> B\nend", 80);
    let all = join_lines(&lines);

    assert!(
        all.contains("Group1"),
        "subgraph label should appear:\n{all}"
    );
}

// Bug 10: Flowchart self-loop is not drawn.
#[test]
fn flowchart_self_loop_draws_a_loop() {
    let lines = render::render_flowchart("flowchart TB\nA --> A", 80);
    let all = join_lines(&lines);

    assert!(
        all.contains('┐') || all.contains('┘'),
        "self-loop should show turn/loop glyphs, not just a lone node:\n{all}"
    );
}

// Bug 11: Gantt label column width is inconsistent across non-empty rows.
#[test]
fn gantt_non_empty_lines_have_consistent_width() {
    let chart =
        charts::parse_gantt("gantt\ndateFormat YYYY-MM-DD\nsection S\nTask : 2024-01-01, 3d");
    let lines = charts::render_gantt(&chart);
    let widths = non_empty_widths(&lines);

    let first = *widths.first().expect("expected non-empty gantt output");
    assert!(
        widths.iter().all(|&w| w == first),
        "all non-empty gantt lines should have the same display width, got {:?}\n{}",
        widths,
        join_lines(&lines)
    );
}

// Bug 12: Gantt legend is wider than the chart body.
#[test]
fn gantt_legend_does_not_exceed_chart_body_width() {
    let chart =
        charts::parse_gantt("gantt\ndateFormat YYYY-MM-DD\nsection S\nTask : 2024-01-01, 3d");
    let lines = charts::render_gantt(&chart);

    let legend = lines
        .iter()
        .find(|l| l.contains("Legend:"))
        .expect("legend line should exist");
    let task_row = lines
        .iter()
        .find(|l| l.contains("Task"))
        .expect("task row should exist");

    let legend_w = width::width(legend);
    let task_w = width::width(task_row);

    assert!(
        legend_w <= task_w,
        "legend width {} should not exceed chart body width {}:\n{}",
        legend_w,
        task_w,
        join_lines(&lines)
    );
}
