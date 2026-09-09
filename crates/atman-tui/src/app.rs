use std::collections::{BTreeSet, HashSet};
use std::time::{Duration, Instant};

use atman_runtime::message::Message;
use atman_runtime::projection::workflow::WorkflowProjection;
use atman_runtime::stream::CompactionPhase;
use atman_runtime::stream::StreamFrame;
use atman_runtime::tools::term::TerminalScreen;

const LAG_COOLDOWN: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TerminalViewMode {
    Stream,
    Capture,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Disclosure {
    #[default]
    Summary,
    Preview,
    Full,
}

impl Disclosure {
    fn next(self) -> Self {
        match self {
            Self::Summary => Self::Preview,
            Self::Preview => Self::Full,
            Self::Full => Self::Summary,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Ok,
    Error,
}

fn tool_result_reports_running(tool: &str, content: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return false;
    };
    match tool {
        "bash.spawn" | "flow.spawn" => {
            value.get("status").and_then(|status| status.as_str()) == Some("running")
        }
        "term.spawn" => {
            value.pointer("/state/kind").and_then(|kind| kind.as_str()) == Some("running")
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolDraftPreview {
    target: Option<&'static str>,
    max_tail: Option<usize>,
    probe: String,
    arguments: String,
    active: bool,
    escaped: bool,
    unicode: Option<(u32, u8)>,
    unicode_high: Option<u16>,
    tail: String,
}

impl ToolDraftPreview {
    fn for_field(target: &'static str, max_tail: Option<usize>) -> Self {
        Self {
            target: Some(target),
            max_tail,
            ..Self::default()
        }
    }

    pub(crate) fn push(&mut self, tool: &str, delta: &str) {
        if self.arguments.len() < 16_384 {
            let mut take = delta.len().min(16_384 - self.arguments.len());
            while !delta.is_char_boundary(take) {
                take -= 1;
            }
            self.arguments.push_str(&delta[..take]);
        }
        if self.target.is_none() {
            let (target, max_tail) = match tool {
                "fs.write" => (Some("content"), Some(8192)),
                "fs.edit" => (Some("new_string"), Some(8192)),
                atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL => (Some("message"), None),
                _ => (None, None),
            };
            self.target = target;
            self.max_tail = max_tail;
        }
        let Some(target) = self.target else {
            return;
        };
        if !self.active {
            self.probe.push_str(delta);
            let marker = format!("\"{target}\"");
            let Some(key_start) = self.probe.find(&marker) else {
                let mut keep_from = self.probe.len().saturating_sub(marker.len() + 8);
                while !self.probe.is_char_boundary(keep_from) {
                    keep_from += 1;
                }
                self.probe.drain(..keep_from);
                return;
            };
            let after_key = key_start + marker.len();
            let Some(colon) = self.probe[after_key..].find(':') else {
                return;
            };
            let after_colon = after_key + colon + 1;
            let Some(quote) = self.probe[after_colon..].find('"') else {
                return;
            };
            let content_start = after_colon + quote + 1;
            let content = self.probe[content_start..].to_string();
            self.probe.clear();
            self.active = true;
            self.decode(&content);
            return;
        }
        self.decode(delta);
    }

    fn decode(&mut self, text: &str) {
        for ch in text.chars() {
            if let Some((mut value, mut digits)) = self.unicode.take() {
                if let Some(digit) = ch.to_digit(16) {
                    value = value.saturating_mul(16).saturating_add(digit);
                    digits += 1;
                    if digits == 4 {
                        self.push_utf16_unit(value as u16);
                    } else {
                        self.unicode = Some((value, digits));
                    }
                }
                continue;
            }
            if self.escaped {
                self.escaped = false;
                match ch {
                    'n' => self.tail.push('\n'),
                    'r' => self.tail.push('\r'),
                    't' => self.tail.push('\t'),
                    'b' => self.tail.push('\u{0008}'),
                    'f' => self.tail.push('\u{000c}'),
                    'u' => self.unicode = Some((0, 0)),
                    other => self.tail.push(other),
                }
            } else if ch == '\\' {
                self.escaped = true;
            } else if ch == '"' {
                if self.unicode_high.take().is_some() {
                    self.tail.push(char::REPLACEMENT_CHARACTER);
                }
                self.active = false;
                break;
            } else {
                self.tail.push(ch);
            }
        }
        if let Some(max_tail) = self.max_tail
            && self.tail.len() > max_tail
        {
            let mut keep_from = self.tail.len() - max_tail;
            while !self.tail.is_char_boundary(keep_from) {
                keep_from += 1;
            }
            self.tail.drain(..keep_from);
        }
    }

    fn push_utf16_unit(&mut self, unit: u16) {
        if let Some(high) = self.unicode_high.take() {
            if (0xDC00..=0xDFFF).contains(&unit) {
                let codepoint =
                    0x10000 + (((u32::from(high) - 0xD800) << 10) | (u32::from(unit) - 0xDC00));
                if let Some(decoded) = char::from_u32(codepoint) {
                    self.tail.push(decoded);
                }
                return;
            }
            self.tail.push(char::REPLACEMENT_CHARACTER);
        }
        if (0xD800..=0xDBFF).contains(&unit) {
            self.unicode_high = Some(unit);
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            self.tail.push(char::REPLACEMENT_CHARACTER);
        } else if let Some(decoded) = char::from_u32(u32::from(unit)) {
            self.tail.push(decoded);
        }
    }

    pub fn last_line(&self) -> Option<&str> {
        self.tail
            .lines()
            .next_back()
            .filter(|line| !line.is_empty())
    }

    pub fn text(&self) -> &str {
        &self.tail
    }

    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

#[derive(Debug, Clone)]
pub struct ToolCallView {
    pub id: String,
    pub tool: String,
    pub intent: String,
    pub input: serde_json::Value,
    pub status: ToolCallStatus,
    pub disclosure: Disclosure,
    pub detail: Option<Box<OutputItem>>,
    pub draft_index: Option<usize>,
    pub draft_preview: ToolDraftPreview,
    pub applied_edit: Option<(String, atman_runtime::activity::EditMetrics)>,
    pub started_at: Instant,
    pub ended_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct SubAgentRoute {
    pub item_index: usize,
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LlmDisclosureState {
    thinking_id: Option<u64>,
    assistant_id: Option<u64>,
}

const ROOT_LLM_DISCLOSURE: &str = "root";

fn llm_disclosure_key(run_id: Option<&str>) -> String {
    run_id.unwrap_or(ROOT_LLM_DISCLOSURE).to_owned()
}

#[derive(Debug, Clone)]
struct WorkFoldState {
    start_id: u64,
    end_id: u64,
    member_count: usize,
    completed_steps: usize,
    total_steps: usize,
    title: String,
    stats: String,
    from_visible: f32,
    target_visible: f32,
    started_at: Instant,
    duration: Duration,
}

impl WorkFoldState {
    fn visibility(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return self.target_visible;
        }
        let progress = (now.saturating_duration_since(self.started_at).as_secs_f32()
            / self.duration.as_secs_f32())
        .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        (self.from_visible + (self.target_visible - self.from_visible) * eased)
            .clamp(0.0, self.member_count as f32)
    }

    fn is_expanded(&self) -> bool {
        self.target_visible > 0.0
    }

    fn is_animating(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < self.duration
            && (self.from_visible - self.target_visible).abs() > f32::EPSILON
    }

    fn toggle(&mut self, now: Instant) {
        let current = self.visibility(now);
        self.from_visible = current;
        self.target_visible = if self.is_expanded() {
            0.0
        } else {
            self.member_count as f32
        };
        self.started_at = now;
    }
}

#[derive(Debug, Clone)]
struct FinalAnswerDraftState {
    preview: ToolDraftPreview,
    summary_preview: ToolDraftPreview,
    assistant_id: Option<u64>,
    work_fold_id: Option<u64>,
}

impl Default for FinalAnswerDraftState {
    fn default() -> Self {
        Self {
            preview: ToolDraftPreview::default(),
            summary_preview: ToolDraftPreview::for_field(
                atman_runtime::message::TOOL_CALL_INTENT_FIELD,
                Some(atman_runtime::message::TOOL_CALL_INTENT_MAX_CHARS),
            ),
            assistant_id: None,
            work_fold_id: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ActivityTotals {
    pub attempted_calls: usize,
    pub completed_calls: usize,
    pub failed_calls: usize,
    pub applied_edits: usize,
    pub hunks: usize,
    pub insertions: usize,
    pub deletions: usize,
    files: HashSet<String>,
    attempted_ids: HashSet<String>,
    completed_ids: HashSet<String>,
}

impl ActivityTotals {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    fn observe_call(&mut self, id: &str) {
        if self.attempted_ids.insert(id.to_string()) {
            self.attempted_calls += 1;
        }
    }

    fn observe_result(&mut self, id: &str, failed: bool) {
        if self.completed_ids.insert(id.to_string()) {
            self.completed_calls += 1;
            self.failed_calls += usize::from(failed);
        }
    }

    fn observe_edit(&mut self, path: &str, metrics: atman_runtime::activity::EditMetrics) {
        self.files.insert(path.to_string());
        self.applied_edits += 1;
        self.hunks += metrics.hunks;
        self.insertions += metrics.insertions;
        self.deletions += metrics.deletions;
    }

    pub(crate) fn from_summary(
        summary: &atman_runtime::activity::ActivitySummary,
        files: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            attempted_calls: summary.attempted_calls,
            completed_calls: summary.completed_calls,
            failed_calls: summary.failed_calls,
            applied_edits: summary.applied_edits,
            hunks: summary.hunks,
            insertions: summary.insertions,
            deletions: summary.deletions,
            files: files.into_iter().collect(),
            attempted_ids: HashSet::new(),
            completed_ids: HashSet::new(),
        }
    }

    pub fn compact_label(&self) -> String {
        format!(
            "{} tools · {} files · +{} −{}",
            self.attempted_calls,
            self.file_count(),
            self.insertions,
            self.deletions
        )
    }
}

#[derive(Debug, Clone)]
pub struct FsSearchHit {
    pub file: String,
    pub line: usize,
    pub before: Vec<String>,
    pub matched: String,
    pub after: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum FsDetail {
    Read {
        path: String,
        content: String,
        start_line: usize,
        total_lines: Option<usize>,
        truncated: bool,
    },
    List {
        path: String,
        entries: Vec<String>,
    },
    Grep {
        path: String,
        pattern: String,
        hits: Vec<FsSearchHit>,
    },
    Raw {
        tool: String,
        path: Option<String>,
        content: String,
        is_error: bool,
    },
}

impl FsDetail {
    pub fn title(&self) -> String {
        match self {
            Self::Read { path, .. } | Self::List { path, .. } => path.clone(),
            Self::Grep { pattern, .. } => format!("Search: {pattern}"),
            Self::Raw { tool, path, .. } => path.clone().unwrap_or_else(|| tool.clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum OutputItem {
    UserTurn {
        text: String,
    },
    Thinking {
        text: String,
        done: bool,
        disclosure: Disclosure,
        retried: bool,
    },
    AssistantMd {
        md: String,
        streaming: bool,
        retried: bool,
    },
    ToolDispatch {
        calls: Vec<ToolCallView>,
    },
    ActivitySummary {
        turn: ActivityTotals,
        session: ActivityTotals,
    },
    SystemNote {
        text: String,
        level: NoteLevel,
    },
    WorkFoldMarker {
        summary: Option<String>,
        start_index: Option<usize>,
    },
    Divider,
    WorkflowPanel {
        turn_index: usize,
        graph: WorkflowProjection,
        expanded_nodes: HashSet<String>,
        panel_expanded: bool,
        started_at: Instant,
        ended_at: Option<Instant>,
        cancelled: bool,
    },
    StartupCard {
        version: String,
        recent: Vec<StartupSessionEntry>,
    },
    Terminal {
        handle: String,
        title: Option<String>,
        command: Option<String>,
        screen: TerminalScreen,
        accumulated_bytes: Vec<u8>,
        mode: TerminalViewMode,
        done: bool,
        expanded: bool,
        scroll_offset: Option<(u16, u16)>,
    },
    Bash {
        handle: String,
        title: Option<String>,
        command: Option<String>,
        output: String,
        done: bool,
        expanded: bool,
    },
    DiffPreview {
        title: String,
        old_content: Option<String>,
        new_content: Option<String>,
        unified_diff: Option<String>,
        expanded: bool,
    },
    FsDetail {
        view: FsDetail,
        expanded: bool,
    },
    CompactionSummary {
        phase: CompactionPhase,
        range_start: usize,
        range_end: usize,
        summary: String,
        before_tokens: u64,
        after_tokens: u64,
        compacted_count: usize,
        disclosure: Disclosure,
    },
    MermaidDiagram {
        source: String,
    },
    SubAgentActivity {
        handle: String,
        goal: String,
        child_run_id: String,
        model: String,
        status: String,
        output: String,
        iteration: u64,
        done: bool,
        expanded: bool,
        messages: Vec<Message>,
        workflow_graph: WorkflowProjection,
        expanded_nodes: HashSet<String>,
        workflow_expanded: bool,
    },
}

impl OutputItem {
    pub fn handle(&self) -> Option<&str> {
        match self {
            OutputItem::Terminal { handle, .. }
            | OutputItem::Bash { handle, .. }
            | OutputItem::SubAgentActivity { handle, .. } => Some(handle),
            _ => None,
        }
    }

    pub(crate) fn has_dynamic_paint(&self) -> bool {
        match self {
            Self::AssistantMd { streaming, .. } => *streaming,
            Self::Thinking { done, .. } => !done,
            Self::Terminal { done, .. }
            | Self::Bash { done, .. }
            | Self::SubAgentActivity { done, .. } => !done,
            Self::WorkflowPanel { ended_at, .. } => ended_at.is_none(),
            Self::CompactionSummary { phase, .. } => {
                matches!(phase, CompactionPhase::Running)
            }
            Self::ToolDispatch { calls } => calls.iter().any(|call| {
                call.status == ToolCallStatus::Running
                    || call
                        .detail
                        .as_deref()
                        .is_some_and(OutputItem::has_dynamic_paint)
            }),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputRevision {
    pub id: u64,
    pub semantic: u64,
    pub interaction: u64,
    pub layout: u64,
    pub paint: u64,
    pub source_generation: u64,
}

#[derive(Debug, Clone)]
pub struct DetachedTaskDetail {
    pub item: OutputItem,
    pub revision: u64,
}

pub fn resolve_task_detail<'a>(
    handle: &str,
    items: &'a [OutputItem],
    handle_index: &std::collections::HashMap<String, usize>,
    detached_task_details: &'a std::collections::HashMap<String, DetachedTaskDetail>,
) -> Option<(usize, &'a OutputItem)> {
    if let Some(detail) = detached_task_details.get(handle) {
        return Some((usize::MAX, &detail.item));
    }
    if let Some(&index) = handle_index.get(handle)
        && let Some(item) = items.get(index)
        && item.handle() == Some(handle)
    {
        return Some((index, item));
    }
    items.iter().enumerate().rev().find_map(|(index, item)| {
        let OutputItem::ToolDispatch { calls } = item else {
            return None;
        };
        calls.iter().find_map(|call| {
            let detail = call.detail.as_deref()?;
            (detail.handle() == Some(handle)).then_some((index, detail))
        })
    })
}

pub fn task_detail_tail_lines(item: &OutputItem, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let text_lines = |text: &str| {
        let mut lines = text
            .lines()
            .rev()
            .filter_map(|line| {
                let trimmed = line.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_owned())
            })
            .take(limit)
            .collect::<Vec<_>>();
        lines.reverse();
        lines
    };
    match item {
        OutputItem::Bash { output, .. } | OutputItem::SubAgentActivity { output, .. } => {
            text_lines(output)
        }
        OutputItem::Terminal {
            screen,
            accumulated_bytes,
            mode,
            ..
        } => {
            let mut lines = Vec::new();
            if *mode == TerminalViewMode::Capture {
                let cols = screen.cols as usize;
                let rows = screen.rows as usize;
                if cols > 0 {
                    for row in (0..rows).rev() {
                        let start = row.saturating_mul(cols);
                        let end = start.saturating_add(cols);
                        let line = screen.cells.get(start..end).map(|cells| {
                            cells
                                .iter()
                                .filter(|cell| !cell.wide_continuation)
                                .map(|cell| cell.chars.as_str())
                                .collect::<String>()
                        });
                        let Some(line) = line else { continue };
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            lines.push(trimmed.to_owned());
                            if lines.len() == limit {
                                break;
                            }
                        }
                    }
                    lines.reverse();
                }
            }
            if lines.is_empty() {
                lines = text_lines(&String::from_utf8_lossy(accumulated_bytes));
            }
            lines
        }
        _ => Vec::new(),
    }
}

const TASK_TEXT_BYTES: usize = 256 * 1024;
const TASK_BYTE_HISTORY: usize = 64 * 1024;
const TASK_TRUNCATION_MARKER: &str = "… earlier task output truncated …\n";

fn append_bounded_text(output: &mut String, text: &str) {
    output.push_str(text);
    if output.len() <= TASK_TEXT_BYTES {
        return;
    }
    let marker_len = TASK_TRUNCATION_MARKER.len();
    let mut keep_from = output
        .len()
        .saturating_sub(TASK_TEXT_BYTES.saturating_sub(marker_len));
    while keep_from < output.len() && !output.is_char_boundary(keep_from) {
        keep_from += 1;
    }
    output.drain(..keep_from);
    if !output.starts_with(TASK_TRUNCATION_MARKER) {
        output.insert_str(0, TASK_TRUNCATION_MARKER);
    }
}

fn append_bounded_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(bytes);
    if output.len() > TASK_BYTE_HISTORY {
        output.drain(..output.len() - TASK_BYTE_HISTORY);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMutation {
    Semantic,
    SemanticPreserveSource,
    Interaction,
    Paint,
}

#[derive(Default)]
pub struct OutputStore {
    values: Vec<OutputItem>,
    revisions: Vec<OutputRevision>,
    animated_ids: BTreeSet<u64>,
    next_id: u64,
    revision_clock: u64,
    structure_revision: u64,
}

impl std::ops::Deref for OutputStore {
    type Target = [OutputItem];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl From<Vec<OutputItem>> for OutputStore {
    fn from(values: Vec<OutputItem>) -> Self {
        let mut store = Self::default();
        store.replace(values);
        store
    }
}

impl OutputStore {
    fn replace(&mut self, values: Vec<OutputItem>) {
        self.values = values;
        self.revisions.clear();
        self.animated_ids.clear();
        self.revisions.reserve(self.values.len());
        for value in &self.values {
            self.next_id = self.next_id.wrapping_add(1);
            self.revision_clock = self.revision_clock.wrapping_add(1);
            if value.has_dynamic_paint() {
                self.animated_ids.insert(self.next_id);
            }
            self.revisions.push(OutputRevision {
                id: self.next_id,
                semantic: self.revision_clock,
                layout: self.revision_clock,
                source_generation: self.revision_clock,
                ..OutputRevision::default()
            });
        }
        self.structure_revision = self.structure_revision.wrapping_add(1);
    }

    fn push(&mut self, value: OutputItem) {
        self.next_id = self.next_id.wrapping_add(1);
        self.revision_clock = self.revision_clock.wrapping_add(1);
        self.values.push(value);
        self.revisions.push(OutputRevision {
            id: self.next_id,
            semantic: self.revision_clock,
            layout: self.revision_clock,
            source_generation: self.revision_clock,
            ..OutputRevision::default()
        });
        if self
            .values
            .last()
            .is_some_and(OutputItem::has_dynamic_paint)
        {
            self.animated_ids.insert(self.next_id);
        }
        self.structure_revision = self.structure_revision.wrapping_add(1);
    }

    fn remove(&mut self, index: usize) -> Option<OutputItem> {
        if index >= self.values.len() {
            return None;
        }
        let revision = self.revisions.remove(index);
        self.animated_ids.remove(&revision.id);
        self.structure_revision = self.structure_revision.wrapping_add(1);
        Some(self.values.remove(index))
    }

    fn mutate(
        &mut self,
        index: usize,
        impact: OutputMutation,
        mutation: impl FnOnce(&mut OutputItem) -> bool,
    ) -> bool {
        let Some(value) = self.values.get_mut(index) else {
            return false;
        };
        let was_animated = value.has_dynamic_paint();
        if !mutation(value) {
            return false;
        }
        let is_animated = value.has_dynamic_paint();
        self.revision_clock = self.revision_clock.wrapping_add(1);
        let revision = &mut self.revisions[index];
        if is_animated != was_animated {
            if is_animated {
                self.animated_ids.insert(revision.id);
            } else {
                self.animated_ids.remove(&revision.id);
            }
        }
        match impact {
            OutputMutation::Semantic => {
                revision.semantic = self.revision_clock;
                revision.layout = self.revision_clock;
                revision.source_generation = self.revision_clock;
            }
            OutputMutation::SemanticPreserveSource => {
                revision.semantic = self.revision_clock;
                revision.layout = self.revision_clock;
            }
            OutputMutation::Interaction => {
                revision.interaction = self.revision_clock;
                revision.layout = self.revision_clock;
            }
            OutputMutation::Paint => {
                revision.interaction = self.revision_clock;
                revision.paint = self.revision_clock;
            }
        }
        true
    }

    fn touch(&mut self, index: usize, impact: OutputMutation) -> bool {
        self.mutate(index, impact, |_| true)
    }

    pub(crate) fn revisions(&self) -> &[OutputRevision] {
        &self.revisions
    }

    pub(crate) fn structure_revision(&self) -> u64 {
        self.structure_revision
    }

    pub(crate) fn revision_clock(&self) -> u64 {
        self.revision_clock
    }

    fn has_active_animation(&self) -> bool {
        !self.animated_ids.is_empty()
    }

    fn index_by_id(&self, id: u64) -> Option<usize> {
        self.revisions.iter().position(|revision| revision.id == id)
    }

    #[cfg(test)]
    fn animated_ids(&self) -> &BTreeSet<u64> {
        &self.animated_ids
    }
}

#[derive(Debug, Clone)]
pub struct StartupIntro {
    pub started_at: Instant,
    pub version: String,
    pub recent: Vec<StartupSessionEntry>,
}

#[derive(Debug, Clone)]
pub struct StartupSessionEntry {
    pub session_id: String,
    pub short_id: String,
    pub goal: Option<String>,
    pub project: Option<String>,
    pub age_label: String,
    pub event_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Err,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteLevel {
    Info,
    Warn,
    Error,
    Success,
    Debug,
}

/// Toast notification displayed in the top-right corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastPosition {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
    TopCenter,
}

#[derive(Debug, Clone)]
pub struct ToastNote {
    pub id: String,
    pub level: NoteLevel,
    pub message: String,
    pub ttl: std::time::Duration,
    pub created: std::time::Instant,
    pub position: ToastPosition,
    pub fading: bool,
    pub fade_started: Option<std::time::Instant>,
}

#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub request_id: atman_runtime::permission::PermissionRequestId,
    pub revision: u64,
    pub payload: atman_runtime::permission_audit::PermissionRequestAudit,
}

#[derive(Debug, Clone)]
pub struct PendingPermissionGroup {
    pub group_id: atman_runtime::permission::PermissionGroupId,
    pub revision: u64,
    pub payload: atman_runtime::permission_audit::PermissionGroupAudit,
    pub expanded: bool,
}

pub struct QueuedSubmissionEdit {
    pub id: atman_runtime::SubmissionId,
    pub revision: u64,
    pub editor: crate::input::InputEditor,
}

#[derive(Default)]
pub struct AppState {
    pub items: OutputStore,
    pub input: String,
    pub input_reasoning: Option<atman_runtime::provider::ReasoningSelection>,
    pub scroll_offset: u32,
    pub follow_tail: bool,
    pub should_quit: bool,
    pub streaming: bool,
    pub was_streaming: bool,
    pub border_fade_at: Option<std::time::Instant>,
    pub waiting_for_llm: bool,
    pub session_activity: ActivityTotals,
    pub turn_activity: ActivityTotals,
    pub goal: Option<String>,
    pub session_id: String,
    pub session_dir: String,
    pub session_name: Option<String>,
    pub project_root: Option<String>,
    pub latest_release: Option<String>,
    pub attach_count: usize,
    pub context: atman_runtime::ContextSnapshot,
    pub todos: Vec<atman_runtime::memory::todo::Todo>,
    pub plans: Vec<atman_runtime::memory::plan::Plan>,
    pub pending_permissions: std::collections::BTreeMap<
        atman_runtime::permission::PermissionRequestId,
        PendingPermission,
    >,
    pub pending_permission_groups: std::collections::BTreeMap<
        atman_runtime::permission::PermissionGroupId,
        PendingPermissionGroup,
    >,
    pub grouped_permission_request_ids:
        std::collections::BTreeSet<atman_runtime::permission::PermissionRequestId>,
    pub pending_injections: Vec<atman_runtime::injection::Injection>,
    pub queued_submissions: Vec<atman_runtime::QueuedSubmissionView>,
    pub submission_focus: bool,
    pub selected_submission: usize,
    pub hovered_submission: Option<usize>,
    pub queued_submission_edit: Option<QueuedSubmissionEdit>,
    pub submission_queue_rect: Option<ratatui::layout::Rect>,
    pub submission_queue_hitmap: crate::submission_queue::QueueHitMap,
    pub yank_mode: bool,
    pub yank_index: usize,
    pub sidebar_mode: crate::sidebar::SidebarMode,
    pub popup: crate::completion::PopupState,
    pub flow_names: Vec<(String, String)>,
    pub expanded_tools: HashSet<String>,
    /// Toast notifications (top-right corner, auto-dismiss).
    pub toasts: Vec<ToastNote>,
    /// Status bar notes keyed by slot id (e.g. "compact", "daemon").
    pub status_notes: std::collections::HashMap<String, String>,
    /// Inline notification slots keyed by replacement id.
    pub inline_note_indices: std::collections::HashMap<String, usize>,
    /// Active modal notification (dismiss with Esc).
    pub modal_notification: Option<String>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub trust: atman_runtime::trust::TrustConfig,
    pub picker_selected: usize,
    pub last_item_ranges: Vec<crate::output::ItemRange>,
    pub last_node_regions: Vec<crate::output::NodeRegion>,
    pub last_transcript_rect: Option<ratatui::layout::Rect>,
    pub last_full_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_rect: Option<ratatui::layout::Rect>,
    pub input_rect: Option<ratatui::layout::Rect>,
    pub hovered_thinking_idx: Option<usize>,
    pub hovered_output_node: Option<(usize, String)>,
    pub hovered_task_id: Option<atman_runtime::TaskId>,
    pub hovered_kill_id: Option<atman_runtime::TaskId>,
    pub hovered_insert_handle: Option<String>,
    pub hovered_activity: Option<(String, String)>,
    pub hovered_history_btn: bool,
    pub hovered_hamburger: bool,
    pub kill_armed_id: Option<atman_runtime::TaskId>,
    pub kill_armed_at: Option<Instant>,
    pub startup_intro: Option<StartupIntro>,
    pub onboarding_skipped: bool,
    pub hints_dismissed: bool,
    pub animation_frame: u32,
    pub deny_arm: Option<std::time::Instant>,
    pub approval_scope_index: u8,
    pub approval_hitmap: crate::approval_bar::ApprovalHitMap,
    pub selected_permission_group: Option<atman_runtime::permission::PermissionGroupId>,
    pub items_version: u64,
    pub task_snapshots_revision: u64,
    pub wm_visual_version: u64,
    pub expanded_version: u64,
    pub terminal_throttle: Option<Instant>,
    pub layout_cache: crate::output::LayoutCache,
    pub last_total_rows: u32,
    pub last_document_visible_rows: u32,
    pub last_input_overlay_rows: u32,
    pub last_items_len: usize,
    pub mouse_captured: bool,
    pub handle_index: std::collections::HashMap<String, usize>,
    pub detached_task_details: std::collections::HashMap<String, DetachedTaskDetail>,
    pub task_handle_index: std::collections::HashMap<String, usize>,
    pub task_id_index: std::collections::HashMap<atman_runtime::TaskId, usize>,
    pub last_workflow_panel_idx: Option<usize>,
    pub workflow_run_to_panel: std::collections::HashMap<String, usize>,
    pub top_level_run_ids: std::collections::HashSet<String>,
    pub sub_agent_run_ids: std::collections::HashMap<String, SubAgentRoute>,
    llm_disclosures: std::collections::HashMap<String, LlmDisclosureState>,
    work_folds: Vec<WorkFoldState>,
    work_fold_scroll_anchor: Option<(u64, u32)>,
    final_answer_drafts: std::collections::HashMap<String, FinalAnswerDraftState>,
    final_answer_drafts_completed: bool,
    pub goal_scroll: u16,
    pub plans_scroll: u16,
    pub todos_scroll: u16,
    pub goal_collapsed: bool,
    pub plan_collapsed: bool,
    pub todo_collapsed: bool,
    pub context_collapsed: bool,
    pub meta_collapsed: bool,
    pub mcp_collapsed: bool,
    pub sidebar_collapsed: bool,
    pub sidebar_upper_collapsed: bool,
    pub sidebar_lower_collapsed: bool,
    pub hovered_sidebar_row: Option<String>,
    pub hovered_sidebar_hamburger: bool,
    pub hovered_sidebar_lower: bool,
    pub hovered_sidebar_more: bool,
    pub sidebar_popup: Option<crate::sidebar::SidebarPopupKind>,
    pub last_sidebar_popup_rect: Option<ratatui::layout::Rect>,
    pub expanded_tasks: std::collections::HashSet<String>,
    pub sidebar_collapse_locked: bool,
    pub sidebar_upper_runtime_collapsed: bool,
    pub sidebar_lower_runtime_collapsed: bool,
    pub task_panel_runtime_collapsed: bool,
    pub task_registry: Option<atman_runtime::TaskRegistry>,
    pub task_snapshots: Vec<atman_runtime::TaskSnapshot>,
    pub activity_nodes: Vec<crate::task_panel::ActivityNode>,
    pub task_panel_collapsed: bool,
    pub task_panel_collapsed_groups: std::collections::HashSet<atman_runtime::TaskKind>,
    pub panel_sizes: std::collections::HashMap<String, (u16, u16)>,
    pub expanded_mcp_servers: HashSet<String>,
    pub mcp_selected: usize,
    pub mcp_remove_armed: Option<String>,
    pub mcp_add_form: Option<crate::mcp_manager::McpAddForm>,
    pub mcp_browser_tab: crate::mcp_manager::McpBrowserTab,
    pub mcp_content_revision: u64,
    pub mcp_resources_cache:
        std::collections::HashMap<String, Vec<atman_runtime::mcp::McpResource>>,
    pub mcp_prompts_cache: std::collections::HashMap<String, Vec<atman_runtime::mcp::McpPrompt>>,
    pub last_task_panel_rect: Option<ratatui::layout::Rect>,
    pub last_task_panel_hitmap: crate::task_panel::TaskPanelHitMap,
    pub tick: u64,
    pub last_goal_rect: Option<ratatui::layout::Rect>,
    pub last_plan_rect: Option<ratatui::layout::Rect>,
    pub last_todo_rect: Option<ratatui::layout::Rect>,
    pub last_goal_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_plan_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_todo_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_ctx_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_meta_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_mcp_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_collapse_btn_rect: Option<ratatui::layout::Rect>,
    pub last_expand_btn_rect: Option<ratatui::layout::Rect>,
    pub last_upper_title_rect: Option<ratatui::layout::Rect>,
    pub last_lower_title_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_more_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_strip_rects: std::collections::HashMap<String, ratatui::layout::Rect>,
    pub select_mode_hinted: bool,
    last_lag_note_idx: Option<usize>,
    last_lag_at: Option<Instant>,
    last_lag_count: u64,
}

enum InputReasoningResolution {
    ModelUnavailable,
    Resolved(Option<atman_runtime::provider::ReasoningSelection>),
    Invalid(atman_runtime::provider::ReasoningSelection),
}

fn mcp_resources_equal(
    left: &[atman_runtime::mcp::McpResource],
    right: &[atman_runtime::mcp::McpResource],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.uri == right.uri
                && left.name == right.name
                && left.description == right.description
                && left.mime_type == right.mime_type
        })
}

fn mcp_prompts_equal(
    left: &[atman_runtime::mcp::McpPrompt],
    right: &[atman_runtime::mcp::McpPrompt],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.name == right.name
                && left.description == right.description
                && left.arguments.len() == right.arguments.len()
                && left
                    .arguments
                    .iter()
                    .zip(&right.arguments)
                    .all(|(left, right)| {
                        left.name == right.name
                            && left.description == right.description
                            && left.required == right.required
                    })
        })
}

pub use atman_runtime::stream::frame_run_id;

impl AppState {
    pub fn new(session_id: String, goal: Option<String>) -> Self {
        Self {
            session_id,
            goal,
            follow_tail: true,
            mouse_captured: true,
            wm_visual_version: 0,
            ..Default::default()
        }
    }

    pub fn input_reasoning_choices(
        &self,
    ) -> Vec<Option<atman_runtime::provider::ReasoningSelection>> {
        if atman_runtime::model_registry::model_info("smart").context_budget == 0 {
            return vec![None];
        }
        let mut choices = atman_runtime::model_registry::reasoning_selections_for_model("smart")
            .into_iter()
            .map(|selection| {
                (!matches!(
                    selection,
                    atman_runtime::provider::ReasoningSelection::ProviderDefault
                ))
                .then_some(selection)
            })
            .collect::<Vec<_>>();
        if choices.is_empty() {
            choices.push(None);
        }
        choices
    }

    pub fn cycle_input_reasoning(&mut self) -> bool {
        let choices = self.input_reasoning_choices();
        let index = choices
            .iter()
            .position(|choice| *choice == self.input_reasoning)
            .unwrap_or(0);
        let next = choices[(index + 1) % choices.len()].clone();
        if next == self.input_reasoning {
            return false;
        }
        self.input_reasoning = next;
        true
    }

    pub fn effective_input_reasoning_badge(&self) -> Option<String> {
        match self.resolve_input_reasoning() {
            InputReasoningResolution::ModelUnavailable => None,
            InputReasoningResolution::Resolved(selection) => {
                selection.map(|selection| selection.to_string())
            }
            InputReasoningResolution::Invalid(selection) => Some(format!("{selection} !")),
        }
    }

    fn resolve_input_reasoning(&self) -> InputReasoningResolution {
        let info = atman_runtime::model_registry::model_info("smart");
        if info.context_budget == 0 {
            return InputReasoningResolution::ModelUnavailable;
        }
        match atman_runtime::model_registry::effective_reasoning_for_model(
            "smart",
            self.input_reasoning.as_ref(),
        ) {
            Ok(selection) => InputReasoningResolution::Resolved(selection),
            Err(_) => InputReasoningResolution::Invalid(
                self.input_reasoning.clone().unwrap_or(info.reasoning),
            ),
        }
    }

    /// Resolve the composer-visible reasoning into a per-submission value.
    pub(crate) fn input_reasoning_for_submission(
        &self,
    ) -> Option<atman_runtime::provider::ReasoningSelection> {
        match self.resolve_input_reasoning() {
            InputReasoningResolution::ModelUnavailable => self.input_reasoning.clone(),
            InputReasoningResolution::Resolved(selection) => selection,
            InputReasoningResolution::Invalid(selection) => Some(selection),
        }
    }

    pub fn reconcile_input_reasoning(&mut self) -> bool {
        let Some(selection) = self.input_reasoning.clone() else {
            return false;
        };
        if atman_runtime::model_registry::reasoning_selection_uses_legacy_capabilities(
            "smart", &selection,
        ) {
            return false;
        }
        let info = atman_runtime::model_registry::model_info("smart");
        if info.context_budget == 0 {
            return false;
        }
        let error =
            atman_runtime::model_registry::resolve_reasoning_for_model("smart", &selection).err();
        let Some(error) = error else {
            return false;
        };
        self.input_reasoning = None;
        self.push_note(
            format!(
                "input reasoning `{selection}` reset to model default for `{}`: {error}",
                info.name
            ),
            NoteLevel::Warn,
        );
        true
    }

    pub fn toggle_mouse_capture(&mut self) -> bool {
        self.mouse_captured = !self.mouse_captured;
        self.mark_visual_dirty();
        self.mouse_captured
    }

    pub fn save_ui_state(&self) {
        let state = crate::states::PersistedUiState::snapshot(self);
        if let Err(e) = state.save() {
            atman_runtime::notify!(warn, "failed to save ui state: {e}");
        }
    }

    /// Background-task-completion open: never steals focus from a focused
    /// floating panel; if a panel is focused, keep its focus and notify via
    /// toast.
    /// Rebuild an `OutputItem::Bash` for a completed bash task whose in-memory
    /// item was evicted (e.g. after a history restore) but whose session log
    /// file still exists on disk at `<session_dir>/bg_<handle>.log`. Returns
    /// `None` when no snapshot/log is available.
    pub(crate) fn reconstruct_bash_item(
        &mut self,
        snap: &Option<atman_runtime::TaskSnapshot>,
    ) -> Option<OutputItem> {
        let snap = snap.as_ref()?;
        if snap.kind != atman_runtime::TaskKind::Bash {
            return None;
        }
        if snap.source_handle.is_empty() {
            return None;
        }
        let log_path =
            std::path::Path::new(&self.session_dir).join(format!("bg_{}.log", snap.source_handle));
        let raw = std::fs::read_to_string(&log_path).ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        // The reader writes `[out] `/`[err] ` prefixes to the log, matching the
        // `[err] `/no-prefix scheme used in the live `OutputItem::Bash.output`.
        let output = raw
            .lines()
            .map(|l| {
                l.strip_prefix("[out] ")
                    .or_else(|| l.strip_prefix("[err] "))
                    .map(str::to_string)
                    .unwrap_or_else(|| l.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(OutputItem::Bash {
            handle: snap.source_handle.clone(),
            title: (!snap.label.trim().is_empty()).then(|| snap.label.clone()),
            command: snap.command.clone(),
            output,
            done: snap.status.is_terminal(),
            expanded: false,
        })
    }

    pub fn with_initial_items(mut self, items: Vec<OutputItem>) -> Self {
        self.llm_disclosures.clear();
        self.work_folds.clear();
        self.work_fold_scroll_anchor = None;
        self.final_answer_drafts.clear();
        self.final_answer_drafts_completed = false;
        let structure_revision = self.items.structure_revision();
        self.items.replace(items);
        let marker_indices = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                OutputItem::WorkFoldMarker {
                    summary,
                    start_index,
                } => Some((index, summary.clone(), *start_index)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (marker_index, summary, persisted_start_index) in marker_indices {
            let start_index = persisted_start_index
                .filter(|start_index| *start_index < marker_index)
                .or_else(|| {
                    self.items[..marker_index]
                        .iter()
                        .rposition(|item| matches!(item, OutputItem::UserTurn { .. }))
                        .map(|index| index + 1)
                });
            let Some(start_index) = start_index else {
                continue;
            };
            let Some(end_index) = marker_index.checked_sub(1) else {
                continue;
            };
            if start_index > end_index {
                continue;
            }
            let Some(start_id) = self.item_id(start_index) else {
                continue;
            };
            let Some(end_id) = self.item_id(end_index) else {
                continue;
            };
            let member_count = end_index - start_index + 1;
            let (completed_steps, total_steps, title, stats) =
                self.work_fold_metadata(start_index, end_index, summary.as_deref(), None);
            self.work_folds.push(WorkFoldState {
                start_id,
                end_id,
                member_count,
                completed_steps,
                total_steps,
                title,
                stats,
                from_visible: 0.0,
                target_visible: 0.0,
                started_at: Instant::now(),
                duration: Duration::ZERO,
            });
        }
        debug_assert_ne!(self.items.structure_revision(), structure_revision);
        self.inline_note_indices.clear();
        self.handle_index.clear();
        self.workflow_run_to_panel.clear();
        self.sub_agent_run_ids.clear();
        self.session_activity = ActivityTotals::default();
        self.turn_activity = ActivityTotals::default();
        self.last_workflow_panel_idx = None;
        for (index, item) in self.items.iter().enumerate() {
            if let Some(handle) = item.handle() {
                self.handle_index.insert(handle.to_owned(), index);
            }
            let (graph, subagent) = match item {
                OutputItem::WorkflowPanel {
                    graph, ended_at, ..
                } => {
                    if ended_at.is_none() {
                        self.last_workflow_panel_idx = Some(index);
                    }
                    (Some(graph), false)
                }
                OutputItem::SubAgentActivity {
                    child_run_id,
                    workflow_graph,
                    ..
                } => {
                    self.sub_agent_run_ids.insert(
                        child_run_id.clone(),
                        SubAgentRoute {
                            item_index: index,
                            tool_use_id: None,
                        },
                    );
                    (Some(workflow_graph), true)
                }
                OutputItem::ToolDispatch { calls } => {
                    for call in calls {
                        self.session_activity.observe_call(&call.id);
                        if call.status != ToolCallStatus::Running {
                            self.session_activity
                                .observe_result(&call.id, call.status == ToolCallStatus::Error);
                        }
                        if let Some((path, metrics)) = &call.applied_edit {
                            self.session_activity.observe_edit(path, *metrics);
                        }
                        if let Some(OutputItem::SubAgentActivity {
                            child_run_id,
                            workflow_graph,
                            ..
                        }) = call.detail.as_deref()
                        {
                            let route = SubAgentRoute {
                                item_index: index,
                                tool_use_id: Some(call.id.clone()),
                            };
                            self.sub_agent_run_ids
                                .insert(child_run_id.clone(), route.clone());
                            let mut nodes = workflow_graph.root.iter().collect::<Vec<_>>();
                            while let Some(node) = nodes.pop() {
                                if let atman_runtime::workflow::WorkflowNodeKind::Flow {
                                    run_id,
                                    ..
                                } = &node.kind
                                {
                                    self.sub_agent_run_ids.insert(run_id.clone(), route.clone());
                                }
                                nodes.extend(node.children.iter());
                            }
                        }
                    }
                    (None, false)
                }
                OutputItem::ActivitySummary { session, .. } => {
                    self.session_activity = session.clone();
                    self.turn_activity = ActivityTotals::default();
                    (None, false)
                }
                _ => (None, false),
            };
            if let Some(graph) = graph {
                let mut nodes = graph.root.iter().collect::<Vec<_>>();
                while let Some(node) = nodes.pop() {
                    if let atman_runtime::workflow::WorkflowNodeKind::Flow { run_id, .. } =
                        &node.kind
                    {
                        if subagent {
                            self.sub_agent_run_ids.insert(
                                run_id.clone(),
                                SubAgentRoute {
                                    item_index: index,
                                    tool_use_id: None,
                                },
                            );
                        } else {
                            self.workflow_run_to_panel.insert(run_id.clone(), index);
                        }
                    }
                    nodes.extend(node.children.iter());
                }
            }
        }
        self.items_version = self.items_version.wrapping_add(1);
        self.layout_cache.invalidate();
        self
    }

    pub fn with_session_dir(mut self, dir: String) -> Self {
        self.session_dir = dir;
        self
    }

    pub fn with_session_identity(
        mut self,
        name: Option<String>,
        project_root: Option<String>,
    ) -> Self {
        self.session_name = name;
        self.project_root = project_root;
        self
    }

    pub fn with_flow_names(mut self, flows: Vec<(String, String)>) -> Self {
        self.flow_names = flows;
        self
    }

    pub fn with_session(mut self, session: Option<std::sync::Arc<atman_runtime::Session>>) -> Self {
        self.session = session;
        self
    }

    pub fn with_trust(mut self, trust: atman_runtime::trust::TrustConfig) -> Self {
        self.trust = trust;
        self
    }

    pub fn with_task_registry(mut self, tr: atman_runtime::TaskRegistry) -> Self {
        self.task_registry = Some(tr);
        self
    }

    pub fn is_tool_expanded(&self, id: &str) -> bool {
        self.expanded_tools.contains(id)
    }

    pub fn toggle_tool_expansion(&mut self, id: &str) {
        if !self.expanded_tools.remove(id) {
            self.expanded_tools.insert(id.to_string());
        }
        if let Some(index) = self.items.iter().position(|item| match item {
            OutputItem::Terminal { handle, .. } | OutputItem::Bash { handle, .. } => handle == id,
            OutputItem::DiffPreview { title, .. } => title == id,
            _ => false,
        }) {
            self.touch_item(index, OutputMutation::Interaction);
        }
    }

    pub fn toggle_last_tool_expansion(&mut self) -> bool {
        self.toggle_last_workflow_tool_node()
    }

    pub fn workflow_panel_task_handle(&self, panel_idx: usize) -> Option<String> {
        let item = self.items.get(panel_idx)?;
        if let crate::app::OutputItem::WorkflowPanel { graph, .. } = item {
            for node in &graph.root {
                if let atman_runtime::workflow::WorkflowNodeKind::Flow { run_id, .. } = &node.kind {
                    return Some(run_id.clone());
                }
            }
        }
        None
    }

    pub fn terminal_item_handle(&self, idx: usize) -> Option<String> {
        let item = self.items.get(idx)?;
        if let crate::app::OutputItem::Terminal { handle, .. } = item {
            return Some(handle.clone());
        }
        None
    }

    pub fn bash_item_handle(&self, idx: usize) -> Option<String> {
        let item = self.items.get(idx)?;
        if let crate::app::OutputItem::Bash { handle, .. } = item {
            return Some(handle.clone());
        }
        None
    }

    pub fn tool_call_detail_handle(&self, idx: usize, tool_use_id: &str) -> Option<String> {
        let OutputItem::ToolDispatch { calls } = self.items.get(idx)? else {
            return None;
        };
        let detail = calls
            .iter()
            .find(|call| call.id == tool_use_id)?
            .detail
            .as_deref()?;
        match detail {
            OutputItem::Terminal { handle, .. }
            | OutputItem::Bash { handle, .. }
            | OutputItem::SubAgentActivity { handle, .. } => Some(handle.clone()),
            _ => None,
        }
    }

    pub fn tool_call_detail(&self, idx: usize, tool_use_id: &str) -> Option<OutputItem> {
        let OutputItem::ToolDispatch { calls } = self.items.get(idx)? else {
            return None;
        };
        calls
            .iter()
            .find(|call| call.id == tool_use_id)?
            .detail
            .as_deref()
            .cloned()
    }

    fn task_command(&self, handle: &str) -> Option<String> {
        self.task_snapshots
            .iter()
            .find(|snapshot| snapshot.source_handle == handle)
            .and_then(|snapshot| snapshot.command.clone())
            .or_else(|| {
                self.task_registry
                    .as_ref()
                    .and_then(|registry| registry.lookup_by_handle(handle))
                    .and_then(|snapshot| snapshot.command)
            })
    }

    pub fn sub_agent_item_handle(&self, idx: usize) -> Option<String> {
        let item = self.items.get(idx)?;
        if let crate::app::OutputItem::SubAgentActivity { handle, .. } = item {
            return Some(handle.clone());
        }
        None
    }

    pub fn maximized_canvas(&self) -> ratatui::layout::Rect {
        let full = self.last_full_rect.unwrap_or_default();
        let transcript = self.last_transcript_rect.unwrap_or(full);
        let bottom_reserved = self
            .input_rect
            .map(|r| {
                full.y
                    .saturating_add(full.height)
                    .saturating_sub(r.y)
                    .saturating_add(1)
            })
            .unwrap_or(0);
        let top = full
            .y
            .saturating_add(full.height)
            .saturating_sub(bottom_reserved);
        let y = transcript.y.max(full.y);
        ratatui::layout::Rect {
            x: full.x,
            y,
            width: full.width,
            height: top.saturating_sub(y),
        }
    }

    pub fn mcp_browser_state(&self) -> crate::mcp_manager::McpBrowserState<'_> {
        crate::mcp_manager::McpBrowserState {
            tab: self.mcp_browser_tab,
            content_revision: self.mcp_content_revision,
            resources: &self.mcp_resources_cache,
            prompts: &self.mcp_prompts_cache,
        }
    }

    pub fn replace_context_snapshot(&mut self, context: atman_runtime::ContextSnapshot) {
        if self.context.mcp_servers != context.mcp_servers {
            self.mcp_content_revision = self.mcp_content_revision.wrapping_add(1);
        }
        self.context = context;
    }

    pub fn replace_mcp_resources(
        &mut self,
        name: String,
        resources: Vec<atman_runtime::mcp::McpResource>,
    ) {
        let unchanged = self
            .mcp_resources_cache
            .get(&name)
            .is_some_and(|current| mcp_resources_equal(current, &resources));
        if unchanged {
            return;
        }
        self.mcp_resources_cache.insert(name, resources);
        self.mcp_content_revision = self.mcp_content_revision.wrapping_add(1);
    }

    pub fn replace_mcp_prompts(
        &mut self,
        name: String,
        prompts: Vec<atman_runtime::mcp::McpPrompt>,
    ) {
        let unchanged = self
            .mcp_prompts_cache
            .get(&name)
            .is_some_and(|current| mcp_prompts_equal(current, &prompts));
        if unchanged {
            return;
        }
        self.mcp_prompts_cache.insert(name, prompts);
        self.mcp_content_revision = self.mcp_content_revision.wrapping_add(1);
    }

    pub fn open_task_panel(
        &mut self,
        wm: &mut crate::wm::WindowManager,
        handle: &str,
        canvas: ratatui::layout::Rect,
        maximized: bool,
        background: bool,
    ) -> bool {
        let item = self.items.iter().rev().find_map(|item| {
            if item.handle() == Some(handle) {
                return Some(item.clone());
            }
            let OutputItem::ToolDispatch { calls } = item else {
                return None;
            };
            calls.iter().find_map(|call| {
                call.detail
                    .as_deref()
                    .filter(|detail| detail.handle() == Some(handle))
                    .cloned()
            })
        });
        let snap = self
            .task_snapshots
            .iter()
            .find(|s| s.source_handle == handle)
            .cloned();

        // A completed bash task may have left the in-memory item list (e.g.
        // after a history restore) while its snapshot and session log file
        // survive. Reconstruct the output so the panel shows real content
        // instead of falling through to the empty placeholder.
        let item = self.reconstruct_bash_item(&snap).or(item);

        let (kind, label, pw, ph) = if let Some(item) = item {
            match item {
                crate::app::OutputItem::Terminal {
                    handle: h, screen, ..
                } => {
                    let (pw, ph) = (screen.cols + 8, screen.rows + 5);
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Terminal, label, pw, ph)
                }
                crate::app::OutputItem::Bash { handle: h, .. } => {
                    let (pw, ph) = self.panel_sizes.get(&h).copied().unwrap_or((0, 0));
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Bash, label, pw, ph)
                }
                crate::app::OutputItem::SubAgentActivity { handle: h, .. } => {
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Flow, label, 0, 0)
                }
                crate::app::OutputItem::WorkflowPanel { .. } => {
                    let kind = snap
                        .as_ref()
                        .map(|s| s.kind)
                        .unwrap_or(atman_runtime::TaskKind::Flow);
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| handle.to_string());
                    (kind, label, 0, 0)
                }
                _ => unreachable!("handle() only returns Some for Terminal/Bash"),
            }
        } else {
            let kind = snap
                .as_ref()
                .map(|s| s.kind)
                .unwrap_or(atman_runtime::TaskKind::Flow);
            let label = snap
                .as_ref()
                .map(|s| s.label.clone())
                .unwrap_or_else(|| handle.to_string());
            (kind, label, 0, 0)
        };

        let content: Box<dyn crate::wm::WindowComponent> = match kind {
            atman_runtime::TaskKind::Bash => Box::new(
                crate::window::bash_panel::BashPanelContent::new(handle.to_string()),
            ),
            atman_runtime::TaskKind::Terminal => {
                Box::new(crate::window::terminal_panel::TerminalPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                })
            }
            atman_runtime::TaskKind::Flow => {
                Box::new(crate::window::flow_panel::FlowPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                    render_cache: None,
                    output_store: (!self.session_dir.is_empty()).then(|| {
                        atman_runtime::tools::tool_output::OutputStore::at(&self.session_dir)
                    }),
                })
            }
        };

        let existing_ids: std::collections::HashSet<crate::wm::WindowId> =
            wm.panels.iter().map(|p| p.id).collect();
        let window_id = if background {
            wm.open_background_with_size(
                handle,
                crate::wm::ContentKey::Task(handle.to_string()),
                crate::wm::OpenPolicy::ReuseExisting,
                crate::wm::WindowContent::Task {
                    handle: handle.to_string(),
                    kind,
                },
                &label,
                canvas,
                pw,
                ph,
                maximized,
            )
        } else {
            wm.open_with_size(
                handle,
                crate::wm::ContentKey::Task(handle.to_string()),
                crate::wm::OpenPolicy::ReuseExisting,
                crate::wm::WindowContent::Task {
                    handle: handle.to_string(),
                    kind,
                },
                &label,
                canvas,
                pw,
                ph,
                maximized,
            )
        };
        let is_new = !existing_ids.contains(&window_id);
        if let Some(panel) = wm.panels.iter_mut().find(|panel| panel.id == window_id) {
            panel.content = Some(content);
        }
        is_new
    }

    pub fn toggle_workflow_node(&mut self, panel_index: usize, node_id: &str) {
        self.mutate_item(panel_index, OutputMutation::Interaction, |item| {
            let expanded_nodes = match item {
                OutputItem::WorkflowPanel { expanded_nodes, .. } => expanded_nodes,
                OutputItem::SubAgentActivity { expanded_nodes, .. } => expanded_nodes,
                _ => return false,
            };
            if !expanded_nodes.remove(node_id) {
                expanded_nodes.insert(node_id.to_string());
            }
            true
        });
    }

    pub fn cycle_thinking_disclosure(&mut self, item_idx: usize) {
        let panel_width = self
            .last_transcript_rect
            .map(|area| area.width)
            .unwrap_or(80);
        self.mutate_item(item_idx, OutputMutation::Interaction, |item| {
            let OutputItem::Thinking {
                text, disclosure, ..
            } = item
            else {
                return false;
            };
            *disclosure = crate::output::next_thinking_disclosure(text, *disclosure, panel_width);
            true
        });
    }

    pub fn set_hovered_thinking(&mut self, idx: Option<usize>) {
        if self.hovered_thinking_idx != idx {
            let previous = self.hovered_thinking_idx;
            self.hovered_thinking_idx = idx;
            if let Some(previous) = previous {
                self.touch_item(previous, OutputMutation::Paint);
            }
            if let Some(idx) = idx {
                self.touch_item(idx, OutputMutation::Paint);
            }
        }
    }

    pub fn set_hovered_output_node(&mut self, node: Option<(usize, String)>) {
        if self.hovered_output_node == node {
            return;
        }
        let previous = std::mem::replace(&mut self.hovered_output_node, node);
        if let Some((item_index, _)) = previous {
            self.touch_item(item_index, OutputMutation::Paint);
        }
        if let Some(item_index) = self
            .hovered_output_node
            .as_ref()
            .map(|(item_index, _)| *item_index)
        {
            self.touch_item(item_index, OutputMutation::Paint);
        }
    }

    pub fn set_hovered_task(&mut self, id: Option<atman_runtime::TaskId>) {
        if self.hovered_task_id != id {
            self.hovered_task_id = id;
        }
    }

    pub fn set_hovered_kill(&mut self, id: Option<atman_runtime::TaskId>) {
        if self.hovered_kill_id != id {
            self.hovered_kill_id = id;
        }
    }

    pub fn set_hovered_insert(&mut self, handle: Option<String>) {
        if self.hovered_insert_handle != handle {
            self.hovered_insert_handle = handle;
        }
    }

    pub fn set_hovered_activity(&mut self, key: Option<(String, String)>) {
        if self.hovered_activity != key {
            self.hovered_activity = key;
        }
    }

    pub fn set_hovered_history_btn(&mut self, hovered: bool) {
        if self.hovered_history_btn != hovered {
            self.hovered_history_btn = hovered;
        }
    }

    pub fn set_hovered_hamburger(&mut self, hovered: bool) {
        if self.hovered_hamburger != hovered {
            self.hovered_hamburger = hovered;
        }
    }

    pub fn arm_kill(&mut self, id: atman_runtime::TaskId) {
        self.kill_armed_id = Some(id);
        self.kill_armed_at = Some(Instant::now());
    }

    pub fn clear_kill_arm(&mut self) {
        if self.kill_armed_id.is_some() {
            self.kill_armed_id = None;
            self.kill_armed_at = None;
        }
    }

    pub fn kill_arm_expired(&self) -> bool {
        match self.kill_armed_at {
            Some(t) => t.elapsed() > std::time::Duration::from_secs(2),
            None => true,
        }
    }

    fn toggle_last_workflow_tool_node(&mut self) -> bool {
        use atman_runtime::workflow::WorkflowNode;
        fn last_tool_path(nodes: &[WorkflowNode], prefix: &str) -> Option<String> {
            for (i, n) in nodes.iter().enumerate().rev() {
                let cur = if prefix.is_empty() {
                    format!("{i}")
                } else {
                    format!("{prefix}/{i}")
                };
                if let Some(hit) = last_tool_path(&n.children, &cur) {
                    return Some(hit);
                }
                if matches!(
                    n.kind,
                    atman_runtime::workflow::WorkflowNodeKind::ToolCall { .. }
                ) {
                    return Some(cur);
                }
            }
            None
        }
        for (idx, item) in self.items.iter().enumerate().rev() {
            if let OutputItem::WorkflowPanel { graph, .. } = item
                && let Some(path) = last_tool_path(&graph.root, "")
            {
                self.toggle_workflow_node(idx, &path);
                return true;
            }
        }
        false
    }

    pub fn toggle_workflow_panel_expansion(&mut self, panel_index: usize) {
        self.mutate_item(panel_index, OutputMutation::Interaction, |item| {
            match item {
                OutputItem::WorkflowPanel { panel_expanded, .. } => {
                    *panel_expanded = !*panel_expanded;
                }
                OutputItem::SubAgentActivity {
                    workflow_expanded, ..
                } => {
                    *workflow_expanded = !*workflow_expanded;
                }
                _ => return false,
            }
            true
        });
    }

    pub fn has_running_workflow(&self) -> bool {
        self.items
            .iter()
            .any(|it| matches!(it, OutputItem::WorkflowPanel { ended_at: None, .. }))
    }

    pub fn has_active_animation(&self) -> bool {
        let now = Instant::now();
        self.items.has_active_animation()
            || self.work_folds.iter().any(|fold| fold.is_animating(now))
    }

    pub fn hit_test(&self, col: u16, row: u16) -> Option<usize> {
        let rect = self.last_transcript_rect?;
        if col < rect.x
            || col >= rect.x.saturating_add(rect.width)
            || row < rect.y
            || row >= rect.y.saturating_add(rect.height)
        {
            return None;
        }
        let rel = u32::from(row.saturating_sub(rect.y)).saturating_add(self.scroll_offset);
        self.last_item_ranges
            .iter()
            .find(|r| rel >= r.start_row && rel < r.end_row)
            .map(|r| r.item_index)
    }

    pub fn scroll_terminal(&mut self, item_index: usize, up: bool, amount: u16) {
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::Terminal {
                screen,
                scroll_offset,
                ..
            } = item
            else {
                return false;
            };
            let max_row = screen.rows;
            let current_row = scroll_offset.map(|(r, _)| r).unwrap_or(0);
            let new_row = if up {
                current_row.saturating_sub(amount)
            } else {
                (current_row + amount).min(max_row.saturating_sub(1))
            };
            let next = if new_row == 0 && !up {
                None
            } else {
                Some((new_row, 0))
            };
            if *scroll_offset == next {
                return false;
            }
            *scroll_offset = next;
            true
        });
    }

    pub fn toggle_terminal_mode(&mut self, item_index: usize) {
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::Terminal { mode, .. } = item else {
                return false;
            };
            *mode = match *mode {
                TerminalViewMode::Capture => TerminalViewMode::Stream,
                TerminalViewMode::Stream => TerminalViewMode::Capture,
            };
            true
        });
    }

    pub fn toggle_terminal_expand(&mut self, item_index: usize) {
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::Terminal { expanded, .. } = item else {
                return false;
            };
            *expanded = !*expanded;
            true
        });
    }

    pub fn toggle_bash_expand(&mut self, item_index: usize) {
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::Bash { expanded, .. } = item else {
                return false;
            };
            *expanded = !*expanded;
            true
        });
    }

    pub fn toggle_sub_agent_expand(&mut self, item_index: usize) {
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::SubAgentActivity { expanded, .. } = item else {
                return false;
            };
            *expanded = !*expanded;
            true
        });
    }

    pub fn toggle_diff_preview_expand(&mut self, item_index: usize) {
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::DiffPreview { expanded, .. } = item else {
                return false;
            };
            *expanded = !*expanded;
            true
        });
    }

    pub fn cycle_compaction_summary_disclosure(&mut self, item_index: usize) {
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::CompactionSummary { disclosure, .. } = item else {
                return false;
            };
            *disclosure = disclosure.next();
            true
        });
    }

    pub fn hit_test_node(&self, col: u16, row: u16) -> Option<(usize, String)> {
        let rect = self.last_transcript_rect?;
        if col < rect.x
            || col >= rect.x.saturating_add(rect.width)
            || row < rect.y
            || row >= rect.y.saturating_add(rect.height)
        {
            return None;
        }
        let rel = u32::from(row.saturating_sub(rect.y)).saturating_add(self.scroll_offset);
        let rel_col = col.saturating_sub(rect.x);
        self.last_node_regions
            .iter()
            .filter(|r| rel >= r.start_row && rel < r.end_row)
            .filter(|r| rel_col >= r.col_start && rel_col < r.col_end)
            .max_by_key(|r| {
                let fullscreen = r
                    .path_key
                    .starts_with(crate::output::TOOL_FULLSCREEN_REGION_PREFIX)
                    || r.path_key
                        .starts_with(crate::output::TOOL_DETAIL_FULLSCREEN_REGION_PREFIX)
                    || matches!(
                        r.path_key.as_str(),
                        crate::output::COLLAPSED_CARD_FULLSCREEN_KEY
                            | crate::output::TERMINAL_FULLSCREEN_KEY
                            | crate::output::BASH_FULLSCREEN_KEY
                            | crate::output::MERMAID_FULLSCREEN_KEY
                            | crate::output::SUB_AGENT_FULLSCREEN_KEY
                    );
                (fullscreen, r.path_key.len())
            })
            .map(|r| (r.panel_item_index, r.path_key.clone()))
    }

    pub fn refresh_popup(&mut self, editor_buf: &str) {
        let builtins = crate::completion::builtins();
        let candidates = crate::completion::compute_candidates(
            editor_buf,
            &self.flow_names,
            &builtins,
            crate::completion::INTERJECTIONS,
            self.streaming,
        );
        self.popup.set(candidates);
    }

    pub fn max_scroll_offset(&self) -> u32 {
        let visible_above = self
            .last_document_visible_rows
            .saturating_sub(self.last_input_overlay_rows)
            .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
            .max(1);
        self.last_total_rows.saturating_sub(visible_above)
    }

    pub fn scroll_up(&mut self, rows: u32) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.follow_tail = false;
    }

    pub fn scroll_down(&mut self, rows: u32) {
        let max = self.max_scroll_offset();
        let next = self.scroll_offset.saturating_add(rows);
        if next >= max {
            self.scroll_offset = max;
            self.follow_tail = true;
        } else {
            self.scroll_offset = next;
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
        self.follow_tail = false;
    }

    pub fn scroll_to_tail(&mut self) {
        self.follow_tail = true;
    }

    pub fn resolve_scroll(
        &mut self,
        total_rows: u32,
        document_visible_rows: u32,
        input_overlay_rows: u32,
        items_len: usize,
    ) {
        let new_items = items_len > self.last_items_len;
        let old_visible_above = self
            .last_document_visible_rows
            .saturating_sub(self.last_input_overlay_rows)
            .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
            .max(1);
        let old_max = self.last_total_rows.saturating_sub(old_visible_above);
        let was_at_bottom = self.scroll_offset >= old_max || self.last_total_rows == 0;
        self.last_total_rows = total_rows;
        self.last_document_visible_rows = document_visible_rows;
        self.last_input_overlay_rows = input_overlay_rows;
        self.last_items_len = items_len;
        let visible_above = document_visible_rows
            .saturating_sub(input_overlay_rows)
            .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
            .max(1);
        let max = total_rows.saturating_sub(visible_above);
        if self.follow_tail && (new_items || was_at_bottom) {
            self.scroll_offset = max;
        } else {
            self.scroll_offset = self.scroll_offset.min(max);
        }
    }

    pub fn pending_below_rows(&self) -> u32 {
        if self.follow_tail {
            0
        } else {
            self.max_scroll_offset().saturating_sub(self.scroll_offset)
        }
    }

    fn find_item_by_handle(&self, handle: &str) -> Option<usize> {
        let idx = *self.handle_index.get(handle)?;
        if idx < self.items.len() {
            Some(idx)
        } else {
            None
        }
    }

    fn mutate_detached_task_detail(
        &mut self,
        handle: String,
        create: impl FnOnce() -> OutputItem,
        update: impl FnOnce(&mut OutputItem),
    ) {
        let entry =
            self.detached_task_details
                .entry(handle)
                .or_insert_with(|| DetachedTaskDetail {
                    item: create(),
                    revision: 0,
                });
        update(&mut entry.item);
        entry.revision = entry.revision.wrapping_add(1);
    }

    fn apply_detached_terminal_chunk(
        &mut self,
        handle: String,
        bytes: Vec<u8>,
        screen: Option<TerminalScreen>,
        title: Option<String>,
        command: Option<String>,
    ) {
        let create_handle = handle.clone();
        let create_title = title.clone();
        let create_command = command.clone();
        self.mutate_detached_task_detail(
            handle,
            move || OutputItem::Terminal {
                handle: create_handle,
                title: create_title,
                command: create_command,
                screen: TerminalScreen {
                    rows: 0,
                    cols: 0,
                    cells: Vec::new(),
                    cursor: None,
                    alt_screen: false,
                },
                accumulated_bytes: Vec::new(),
                mode: TerminalViewMode::Capture,
                done: false,
                expanded: false,
                scroll_offset: None,
            },
            move |item| {
                let OutputItem::Terminal {
                    title: current_title,
                    command: current_command,
                    screen: current_screen,
                    accumulated_bytes,
                    ..
                } = item
                else {
                    return;
                };
                if current_title.is_none() {
                    *current_title = title;
                }
                if current_command.is_none() {
                    *current_command = command;
                }
                if let Some(screen) = screen {
                    *current_screen = screen;
                }
                append_bounded_bytes(accumulated_bytes, &bytes);
            },
        );
    }

    fn apply_detached_bash_chunk(
        &mut self,
        handle: String,
        text: String,
        title: Option<String>,
        command: Option<String>,
    ) {
        let create_handle = handle.clone();
        let create_title = title.clone();
        let create_command = command.clone();
        self.mutate_detached_task_detail(
            handle,
            move || OutputItem::Bash {
                handle: create_handle,
                title: create_title,
                command: create_command,
                output: String::new(),
                done: false,
                expanded: false,
            },
            move |item| {
                let OutputItem::Bash {
                    title: current_title,
                    command: current_command,
                    output,
                    ..
                } = item
                else {
                    return;
                };
                if current_title.is_none() {
                    *current_title = title;
                }
                if current_command.is_none() {
                    *current_command = command;
                }
                append_bounded_text(output, &text);
            },
        );
    }

    fn finish_detached_task_detail(&mut self, handle: &str) {
        let Some(entry) = self.detached_task_details.get_mut(handle) else {
            return;
        };
        match &mut entry.item {
            OutputItem::Terminal { done, .. } | OutputItem::Bash { done, .. } => *done = true,
            _ => return,
        }
        entry.revision = entry.revision.wrapping_add(1);
    }

    pub fn push_item(&mut self, item: OutputItem) {
        let idx = self.items.len();
        match &item {
            OutputItem::Terminal { handle, .. }
            | OutputItem::Bash { handle, .. }
            | OutputItem::SubAgentActivity { handle, .. } => {
                self.handle_index.insert(handle.clone(), idx);
            }
            OutputItem::WorkflowPanel { ended_at: None, .. } => {
                self.last_workflow_panel_idx = Some(idx);
            }
            _ => {}
        }
        let structure_revision = self.items.structure_revision();
        self.items.push(item);
        debug_assert_eq!(self.items.revisions().len(), self.items.len());
        debug_assert_ne!(self.items.structure_revision(), structure_revision);
        self.items_version = self.items_version.wrapping_add(1);
        self.layout_cache.mark_structure_dirty(idx);
        self.reset_lag_state();
    }

    pub fn remove_item(&mut self, index: usize) -> Option<OutputItem> {
        let structure_revision = self.items.structure_revision();
        let removed = self.items.remove(index)?;
        debug_assert_eq!(self.items.revisions().len(), self.items.len());
        debug_assert_ne!(self.items.structure_revision(), structure_revision);
        self.items_version = self.items_version.wrapping_add(1);
        self.layout_cache.mark_structure_dirty(index);
        self.handle_index.retain(|_, item_index| {
            if *item_index == index {
                false
            } else {
                if *item_index > index {
                    *item_index -= 1;
                }
                true
            }
        });
        self.inline_note_indices.retain(|_, item_index| {
            if *item_index == index {
                false
            } else {
                if *item_index > index {
                    *item_index -= 1;
                }
                true
            }
        });
        let removed_run_ids = self
            .workflow_run_to_panel
            .iter()
            .filter(|(_, item_index)| **item_index == index)
            .map(|(run_id, _)| run_id.clone())
            .collect::<Vec<_>>();
        self.workflow_run_to_panel.retain(|_, item_index| {
            if *item_index == index {
                false
            } else {
                if *item_index > index {
                    *item_index -= 1;
                }
                true
            }
        });
        for run_id in removed_run_ids {
            self.top_level_run_ids.remove(&run_id);
        }
        self.sub_agent_run_ids.retain(|_, route| {
            if route.item_index == index {
                false
            } else {
                if route.item_index > index {
                    route.item_index -= 1;
                }
                true
            }
        });
        self.last_lag_note_idx = self.last_lag_note_idx.and_then(|item_index| {
            if item_index == index {
                None
            } else {
                Some(item_index - usize::from(item_index > index))
            }
        });
        self.last_workflow_panel_idx = self
            .items
            .iter()
            .rposition(|item| matches!(item, OutputItem::WorkflowPanel { ended_at: None, .. }));
        Some(removed)
    }

    fn mutate_item(
        &mut self,
        index: usize,
        impact: OutputMutation,
        mutation: impl FnOnce(&mut OutputItem) -> bool,
    ) -> bool {
        let changed = self.items.mutate(index, impact, mutation);
        if changed {
            match impact {
                OutputMutation::Semantic | OutputMutation::SemanticPreserveSource => {
                    self.items_version = self.items_version.wrapping_add(1);
                    self.layout_cache.mark_layout_dirty(index);
                }
                OutputMutation::Interaction => {
                    self.expanded_version = self.expanded_version.wrapping_add(1);
                    self.layout_cache.mark_layout_dirty(index);
                }
                OutputMutation::Paint => self.layout_cache.mark_paint_dirty(index),
            }
        }
        changed
    }

    fn touch_item(&mut self, index: usize, impact: OutputMutation) -> bool {
        let changed = self.items.touch(index, impact);
        if changed && matches!(impact, OutputMutation::Interaction) {
            self.expanded_version = self.expanded_version.wrapping_add(1);
            self.layout_cache.mark_layout_dirty(index);
        } else if changed && matches!(impact, OutputMutation::Paint) {
            self.layout_cache.mark_paint_dirty(index);
        }
        changed
    }

    fn item_id(&self, index: usize) -> Option<u64> {
        self.items
            .revisions()
            .get(index)
            .map(|revision| revision.id)
    }

    fn finish_tracked_thinking(&mut self, key: &str, retried: bool) {
        let thinking_id = self
            .llm_disclosures
            .get(key)
            .and_then(|state| state.thinking_id);
        let Some(index) = thinking_id.and_then(|id| self.items.index_by_id(id)) else {
            return;
        };
        self.mutate_item(index, OutputMutation::Semantic, |item| {
            let OutputItem::Thinking {
                done,
                retried: item_retried,
                ..
            } = item
            else {
                return false;
            };
            let changed = !*done || (retried && !*item_retried);
            *done = true;
            *item_retried |= retried;
            changed
        });
    }

    fn finish_llm_disclosure(&mut self, key: &str, retried: bool) {
        let state = self.llm_disclosures.remove(key).unwrap_or_default();
        for id in [state.thinking_id, state.assistant_id]
            .into_iter()
            .flatten()
        {
            let Some(index) = self.items.index_by_id(id) else {
                continue;
            };
            self.mutate_item(index, OutputMutation::Semantic, |item| match item {
                OutputItem::Thinking {
                    done,
                    retried: item_retried,
                    ..
                } => {
                    let changed = !*done || (retried && !*item_retried);
                    *done = true;
                    *item_retried |= retried;
                    changed
                }
                OutputItem::AssistantMd {
                    streaming,
                    retried: item_retried,
                    ..
                } => {
                    let changed = *streaming || (retried && !*item_retried);
                    *streaming = false;
                    *item_retried |= retried;
                    changed
                }
                _ => false,
            });
        }
    }

    pub fn mark_visual_dirty(&mut self) {
        self.wm_visual_version = self.wm_visual_version.wrapping_add(1);
    }

    pub fn push_note(&mut self, text: impl Into<String>, level: NoteLevel) {
        self.push_item(OutputItem::SystemNote {
            text: text.into(),
            level,
        });
    }

    pub fn push_toast(
        &mut self,
        text: impl Into<String>,
        level: NoteLevel,
        ttl: std::time::Duration,
        position: ToastPosition,
    ) {
        let id = format!("toast-{}", self.toasts.len());
        self.toasts.push(ToastNote {
            id,
            level,
            message: text.into(),
            ttl,
            created: std::time::Instant::now(),
            position,
            fading: false,
            fade_started: None,
        });
        // Keep at most 5 toasts, remove oldest.
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    pub fn push_status(&mut self, key: impl Into<String>, text: impl Into<String>) {
        self.status_notes.insert(key.into(), text.into());
    }

    pub fn tick_toasts(&mut self) {
        let now = std::time::Instant::now();
        let fade_duration = std::time::Duration::from_millis(400);

        // Start fading for expired toasts that aren't already fading
        for toast in &mut self.toasts {
            if !toast.fading && now.duration_since(toast.created) >= toast.ttl {
                toast.fading = true;
                toast.fade_started = Some(now);
            }
        }

        // Remove toasts that have finished fading
        self.toasts.retain(|t| {
            if !t.fading {
                return true;
            }
            let fade_start = t.fade_started.unwrap_or(now);
            now.duration_since(fade_start) < fade_duration
        });
    }

    pub fn apply_task_event(&mut self, event: atman_runtime::TaskEvent) {
        match event {
            atman_runtime::TaskEvent::Registered(snap) => {
                self.task_id_index
                    .insert(snap.id.clone(), self.task_snapshots.len());
                self.task_handle_index
                    .insert(snap.source_handle.clone(), self.task_snapshots.len());
                self.task_snapshots.push(snap);
                self.task_snapshots_revision = self.task_snapshots_revision.wrapping_add(1);
                self.mark_visual_dirty();
            }
            atman_runtime::TaskEvent::StatusChanged {
                id,
                new,
                termination,
                ..
            } => {
                if let Some(s) = self
                    .task_id_index
                    .get(&id)
                    .and_then(|&index| self.task_snapshots.get_mut(index))
                {
                    s.status = new;
                    s.termination = termination;
                    s.ended_at = Some(std::time::Instant::now());
                    self.task_snapshots_revision = self.task_snapshots_revision.wrapping_add(1);
                    self.mark_visual_dirty();
                }
            }
            atman_runtime::TaskEvent::Reaped { id } => {
                if let Some(index) = self.task_id_index.remove(&id) {
                    self.task_snapshots.remove(index);
                    self.task_handle_index.retain(|_, task_index| {
                        if *task_index == index {
                            false
                        } else {
                            *task_index -= usize::from(*task_index > index);
                            true
                        }
                    });
                    self.task_id_index.retain(|_, task_index| {
                        *task_index -= usize::from(*task_index > index);
                        true
                    });
                    self.task_snapshots_revision = self.task_snapshots_revision.wrapping_add(1);
                    self.mark_visual_dirty();
                }
            }
        }
    }

    fn apply_permission_projection(&mut self, frame: &StreamFrame) {
        use atman_runtime::stream::StreamFrame;

        match frame {
            StreamFrame::PermissionRequestCreated { payload, .. }
            | StreamFrame::PermissionRequestTargeted { payload, .. }
            | StreamFrame::PermissionRequestDeferred { payload, .. } => {
                if payload.decision_id.is_some() {
                    return;
                }
                if !matches!(
                    payload.target,
                    atman_runtime::permission_audit::PermissionAuditTarget::User
                ) {
                    return;
                }
                if let Some(request_id) = payload.request_id.clone() {
                    self.pending_permissions.insert(
                        request_id.clone(),
                        PendingPermission {
                            request_id,
                            revision: payload.revision,
                            payload: payload.clone(),
                        },
                    );
                }
            }
            StreamFrame::PermissionRequestApproved { payload, .. }
            | StreamFrame::PermissionRequestDenied { payload, .. }
            | StreamFrame::PermissionRequestCancelled { payload, .. }
            | StreamFrame::UnrestrictedExecution { payload, .. } => {
                if let Some(request_id) = &payload.request_id {
                    self.pending_permissions.remove(request_id);
                }
            }
            StreamFrame::PermissionGroupCreated { payload, .. }
            | StreamFrame::PermissionGroupUpdated { payload, .. } => {
                self.pending_permission_groups.insert(
                    payload.group_id.clone(),
                    PendingPermissionGroup {
                        group_id: payload.group_id.clone(),
                        revision: payload.revision,
                        payload: payload.clone(),
                        expanded: self
                            .pending_permission_groups
                            .get(&payload.group_id)
                            .is_some_and(|group| group.expanded),
                    },
                );
                self.refresh_grouped_permission_request_ids();
            }
            StreamFrame::PermissionGroupResolved { payload, .. } => {
                self.pending_permission_groups.remove(&payload.group_id);
                self.refresh_grouped_permission_request_ids();
                if self.selected_permission_group.as_ref() == Some(&payload.group_id) {
                    self.selected_permission_group = None;
                }
            }
            _ => {}
        }
    }

    fn refresh_grouped_permission_request_ids(&mut self) {
        self.grouped_permission_request_ids.clear();
        self.grouped_permission_request_ids.extend(
            self.pending_permission_groups
                .values()
                .flat_map(|group| group.payload.request_ids.iter().cloned()),
        );
    }

    fn upsert_activity_node(&mut self, node: crate::task_panel::ActivityNode) {
        if let Some(existing) = self
            .activity_nodes
            .iter_mut()
            .rev()
            .find(|existing| existing.run_id == node.run_id && existing.node_id == node.node_id)
        {
            existing.parent_node_id = node.parent_node_id;
            existing.label = node.label;
            existing.kind = node.kind;
            if existing.status != crate::task_panel::ActivityStatus::Running {
                existing.status = node.status;
                existing.started_at = node.started_at;
                existing.ended_at = node.ended_at;
            }
            return;
        }
        self.activity_nodes.push(node);
        if self.activity_nodes.len() > 128 {
            self.activity_nodes.remove(0);
        }
    }

    fn finish_activity_node(
        &mut self,
        run_id: Option<&str>,
        node_id: &str,
        status: crate::task_panel::ActivityStatus,
    ) {
        if let Some(node) = self.activity_nodes.iter_mut().rev().find(|node| {
            node.node_id == node_id && run_id.is_none_or(|run_id| node.run_id == run_id)
        }) {
            node.status = status;
            node.ended_at = Some(std::time::Instant::now());
        }
    }

    fn append_tool_dispatch(&mut self, message: &Message) {
        let calls = message
            .parts
            .iter()
            .filter_map(|part| {
                let atman_runtime::message::MessagePart::ToolUse {
                    id,
                    name,
                    input,
                    intent,
                } = part
                else {
                    return None;
                };
                Some(ToolCallView {
                    id: id.clone(),
                    tool: name.clone(),
                    intent: intent
                        .as_ref()
                        .map(|intent| intent.as_str().to_owned())
                        .unwrap_or_else(|| name.clone()),
                    input: input.clone(),
                    status: ToolCallStatus::Running,
                    disclosure: Disclosure::Summary,
                    detail: None,
                    draft_index: None,
                    draft_preview: ToolDraftPreview::default(),
                    applied_edit: None,
                    started_at: Instant::now(),
                    ended_at: None,
                })
            })
            .collect::<Vec<_>>();
        if calls.is_empty() {
            return;
        }
        let can_reconcile = self.items.last().is_some_and(|item| {
            matches!(item, OutputItem::ToolDispatch { calls } if !calls.is_empty() && calls.iter().all(|call| call.draft_index.is_some()))
        });
        if can_reconcile {
            let index = self.items.len() - 1;
            self.mutate_item(index, OutputMutation::Semantic, |item| {
                let OutputItem::ToolDispatch {
                    calls: current_calls,
                } = item
                else {
                    return false;
                };
                for (index, mut final_call) in calls.into_iter().enumerate() {
                    if let Some(current) = current_calls
                        .iter_mut()
                        .find(|call| call.id == final_call.id || call.draft_index == Some(index))
                    {
                        final_call.disclosure = current.disclosure;
                        final_call.detail = current.detail.take();
                        final_call.started_at = current.started_at;
                        final_call.draft_preview = current.draft_preview.clone();
                        *current = final_call;
                    } else {
                        current_calls.push(final_call);
                    }
                }
                true
            });
        } else {
            self.push_item(OutputItem::ToolDispatch { calls });
        }
    }

    fn apply_tool_call_draft(
        &mut self,
        run_id: Option<&str>,
        index: usize,
        call_id: String,
        name: String,
        arguments_delta: String,
    ) {
        if self.apply_final_answer_draft(run_id, index, &call_id, &name, &arguments_delta) {
            return;
        }
        if run_id.is_some_and(|run_id| self.sub_agent_run_ids.contains_key(run_id)) {
            return;
        }
        let fallback_id = format!("draft:{}:{index}", run_id.unwrap_or("root"));
        let key = if call_id.is_empty() {
            fallback_id
        } else {
            call_id
        };
        let existing = self
            .items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(item_index, item)| {
                let OutputItem::ToolDispatch { calls } = item else {
                    return None;
                };
                calls
                    .iter()
                    .position(|call| call.id == key || call.draft_index == Some(index))
                    .map(|call_index| (item_index, call_index))
            });
        if let Some((item_index, call_index)) = existing {
            self.mutate_item(item_index, OutputMutation::SemanticPreserveSource, |item| {
                let OutputItem::ToolDispatch { calls } = item else {
                    return false;
                };
                let call = &mut calls[call_index];
                if !key.starts_with("draft:") {
                    call.id = key;
                }
                if !name.is_empty() {
                    call.tool = name.clone();
                    if call.intent.starts_with("draft:") || call.intent.is_empty() {
                        call.intent = name.clone();
                    }
                }
                call.draft_preview.push(&call.tool, &arguments_delta);
                true
            });
            return;
        }
        let mut draft_preview = ToolDraftPreview::default();
        draft_preview.push(&name, &arguments_delta);
        let call = ToolCallView {
            id: key.clone(),
            tool: name.clone(),
            intent: if name.is_empty() { key } else { name },
            input: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: Some(index),
            draft_preview,
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: None,
        };
        if let Some(last_index) = self.items.len().checked_sub(1)
            && matches!(self.items[last_index], OutputItem::ToolDispatch { ref calls } if calls.iter().all(|call| call.draft_index.is_some()))
        {
            self.mutate_item(last_index, OutputMutation::Semantic, |item| {
                let OutputItem::ToolDispatch { calls } = item else {
                    return false;
                };
                calls.push(call);
                true
            });
        } else {
            self.push_item(OutputItem::ToolDispatch { calls: vec![call] });
        }
    }

    fn apply_final_answer_draft(
        &mut self,
        run_id: Option<&str>,
        index: usize,
        _call_id: &str,
        name: &str,
        arguments_delta: &str,
    ) -> bool {
        if run_id.is_some_and(|run_id| self.sub_agent_run_ids.contains_key(run_id)) {
            return false;
        }
        let key = format!("final-draft:{}:{index}", run_id.unwrap_or("root"));
        let is_final = name == atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL
            || self.final_answer_drafts.contains_key(&key);
        if !is_final {
            return false;
        }
        let disclosure_key = llm_disclosure_key(run_id);
        self.finish_tracked_thinking(&disclosure_key, false);
        if !self.final_answer_drafts.contains_key(&key) {
            let work_fold_id = self.begin_work_fold(None);
            self.push_item(OutputItem::AssistantMd {
                md: String::new(),
                streaming: true,
                retried: false,
            });
            let assistant_id = self
                .items
                .len()
                .checked_sub(1)
                .and_then(|idx| self.item_id(idx));
            self.final_answer_drafts.insert(
                key.clone(),
                FinalAnswerDraftState {
                    preview: ToolDraftPreview::default(),
                    summary_preview: ToolDraftPreview::for_field(
                        atman_runtime::message::TOOL_CALL_INTENT_FIELD,
                        Some(atman_runtime::message::TOOL_CALL_INTENT_MAX_CHARS),
                    ),
                    assistant_id,
                    work_fold_id,
                },
            );
            self.llm_disclosures
                .entry(disclosure_key)
                .or_default()
                .assistant_id = assistant_id;
        }
        let (assistant_id, work_fold_id, suffix, summary) = {
            let draft = self.final_answer_drafts.get_mut(&key).unwrap();
            let before = draft.preview.text().len();
            draft.preview.push(
                atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL,
                arguments_delta,
            );
            draft.summary_preview.push("", arguments_delta);
            let text = draft.preview.text();
            let suffix = text.get(before..).unwrap_or_default().to_owned();
            (
                draft.assistant_id,
                draft.work_fold_id,
                suffix,
                draft.summary_preview.text().to_owned(),
            )
        };
        if let Some(summary) = atman_runtime::message::ToolCallIntent::new(summary)
            && let Some(fold) = work_fold_id
                .and_then(|id| self.work_folds.iter_mut().find(|fold| fold.start_id == id))
        {
            fold.title = summary.as_str().to_owned();
        }
        if !suffix.is_empty()
            && let Some(item_index) = assistant_id.and_then(|id| self.items.index_by_id(id))
        {
            self.mutate_item(item_index, OutputMutation::SemanticPreserveSource, |item| {
                let OutputItem::AssistantMd { md, .. } = item else {
                    return false;
                };
                md.push_str(&suffix);
                true
            });
        }
        self.waiting_for_llm = false;
        self.streaming = true;
        self.reset_lag_state();
        true
    }

    fn discard_completed_final_answer_draft(&mut self) {
        if !self.final_answer_drafts_completed {
            return;
        }
        let assistant_ids = self
            .final_answer_drafts
            .values()
            .filter_map(|draft| draft.assistant_id)
            .collect::<HashSet<_>>();
        let fold_ids = self
            .final_answer_drafts
            .values()
            .filter_map(|draft| draft.work_fold_id)
            .collect::<HashSet<_>>();
        self.final_answer_drafts.clear();
        self.final_answer_drafts_completed = false;
        self.work_folds
            .retain(|fold| !fold_ids.contains(&fold.start_id));
        let mut indices = assistant_ids
            .into_iter()
            .filter_map(|id| self.items.index_by_id(id))
            .collect::<Vec<_>>();
        indices.sort_unstable_by(|left, right| right.cmp(left));
        for index in indices {
            self.remove_item(index);
        }
    }

    fn begin_work_fold(&mut self, summary: Option<&str>) -> Option<u64> {
        let start_index = self
            .items
            .iter()
            .rposition(|item| matches!(item, OutputItem::UserTurn { .. }))
            .map_or(0, |index| index.saturating_add(1));
        let end_index = self.items.len().checked_sub(1)?;
        if start_index > end_index {
            return None;
        }
        let start_id = self.item_id(start_index)?;
        let end_id = self.item_id(end_index)?;
        if self
            .work_folds
            .iter()
            .any(|fold| fold.start_id == start_id && fold.end_id == end_id)
        {
            return Some(start_id);
        }
        let member_count = end_index - start_index + 1;
        let (completed_steps, total_steps, title, stats) =
            self.work_fold_metadata(start_index, end_index, summary, Some(&self.turn_activity));
        let duration_ms = (400 + member_count as u64 * 40).clamp(400, 700);
        self.work_folds.push(WorkFoldState {
            start_id,
            end_id,
            member_count,
            completed_steps,
            total_steps,
            title,
            stats,
            from_visible: member_count as f32,
            target_visible: 0.0,
            started_at: Instant::now(),
            duration: Duration::from_millis(duration_ms),
        });
        Some(start_id)
    }

    fn work_fold_metadata(
        &self,
        start_index: usize,
        end_index: usize,
        summary: Option<&str>,
        activity: Option<&ActivityTotals>,
    ) -> (usize, usize, String, String) {
        let mut call_ids = HashSet::new();
        let mut intents = Vec::new();
        let mut files = HashSet::new();
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        for item in &self.items[start_index..=end_index] {
            let OutputItem::ToolDispatch { calls } = item else {
                continue;
            };
            for call in calls {
                call_ids.insert(call.id.clone());
                if !call.intent.trim().is_empty()
                    && call.intent != call.tool
                    && !intents.iter().any(|intent| intent == &call.intent)
                    && intents.len() < 3
                {
                    intents.push(call.intent.clone());
                }
                if let Some((path, metrics)) = &call.applied_edit {
                    files.insert(path.clone());
                    insertions = insertions.saturating_add(metrics.insertions);
                    deletions = deletions.saturating_add(metrics.deletions);
                }
            }
        }
        let title = atman_runtime::message::ToolCallIntent::new(summary.unwrap_or_default())
            .map(|summary| summary.as_str().to_owned())
            .unwrap_or_else(|| {
                if intents.is_empty() {
                    "Completed internal work.".to_owned()
                } else {
                    intents.join(" · ")
                }
            });
        let total_steps = call_ids.len().max(1);
        let (file_count, insertions, deletions) =
            activity.map_or((files.len(), insertions, deletions), |activity| {
                (
                    activity.file_count(),
                    activity.insertions,
                    activity.deletions,
                )
            });
        let mut stats = Vec::new();
        if file_count > 0 {
            stats.push(format!("{file_count} files"));
        }
        if insertions > 0 || deletions > 0 {
            stats.push(format!("+{insertions} −{deletions}"));
        }
        (total_steps, total_steps, title, stats.join(" · "))
    }

    fn commit_final_answer_message(&mut self, message: &Message) {
        let text = message.text_concat();
        let summary = atman_runtime::tools::final_answer::summary(message);
        if let Some(summary) = summary.as_deref()
            && let Some(fold) = self.work_folds.last_mut()
        {
            fold.title = summary.to_owned();
        }
        let existing = self
            .items
            .last()
            .is_some_and(|item| matches!(item, OutputItem::AssistantMd { .. }))
            .then(|| self.items.len() - 1);
        if let Some(index) = existing {
            self.mutate_item(index, OutputMutation::Semantic, |item| {
                let OutputItem::AssistantMd { md, streaming, .. } = item else {
                    return false;
                };
                let changed = *md != text || *streaming;
                *md = text;
                *streaming = false;
                changed
            });
        } else {
            let _ = self.begin_work_fold(summary.as_deref());
            self.push_item(OutputItem::AssistantMd {
                md: text,
                streaming: false,
                retried: false,
            });
        }
        self.final_answer_drafts.clear();
        self.final_answer_drafts_completed = false;
    }

    fn mutate_tool_call(
        &mut self,
        tool_use_id: &str,
        mutation: OutputMutation,
        update: impl FnOnce(&mut ToolCallView) -> bool,
    ) -> bool {
        let Some((item_index, call_index)) =
            self.items
                .iter()
                .enumerate()
                .rev()
                .find_map(|(item_index, item)| {
                    let OutputItem::ToolDispatch { calls } = item else {
                        return None;
                    };
                    calls
                        .iter()
                        .position(|call| call.id == tool_use_id)
                        .map(|call_index| (item_index, call_index))
                })
        else {
            return false;
        };
        self.mutate_item(item_index, mutation, |item| {
            let OutputItem::ToolDispatch { calls } = item else {
                return false;
            };
            update(&mut calls[call_index])
        });
        true
    }

    fn apply_tool_result_to_dispatch(&mut self, message: &Message) {
        for part in &message.parts {
            let atman_runtime::message::MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = part
            else {
                continue;
            };
            let meta = self.items.iter().rev().find_map(|item| {
                let OutputItem::ToolDispatch { calls } = item else {
                    return None;
                };
                calls
                    .iter()
                    .find(|call| call.id == *tool_use_id)
                    .map(|call| {
                        crate::history::ToolDisplayMeta::from_tool_use(
                            &call.tool,
                            &call.input,
                            None,
                        )
                    })
            });
            let reports_running = !*is_error
                && meta
                    .as_ref()
                    .is_some_and(|meta| tool_result_reports_running(&meta.name, content));
            let output_store = (!self.session_dir.is_empty())
                .then(|| atman_runtime::tools::tool_output::OutputStore::at(&self.session_dir));
            let mut restored = crate::history::restore_tool_item_with_output_store(
                meta.as_ref(),
                content,
                *is_error,
                output_store.as_ref(),
            );
            self.mutate_tool_call(tool_use_id, OutputMutation::Semantic, |call| {
                if call.status == ToolCallStatus::Running {
                    call.status = if reports_running {
                        ToolCallStatus::Running
                    } else if *is_error {
                        ToolCallStatus::Error
                    } else {
                        ToolCallStatus::Ok
                    };
                    if call.status != ToolCallStatus::Running && call.ended_at.is_none() {
                        call.ended_at = Some(Instant::now());
                    }
                }
                let call_done = call.status != ToolCallStatus::Running;
                if let Some(
                    OutputItem::Bash { done, .. }
                    | OutputItem::Terminal { done, .. }
                    | OutputItem::SubAgentActivity { done, .. },
                ) = restored.as_mut()
                {
                    *done = call_done;
                }
                if call.detail.is_none() {
                    call.detail = restored.map(Box::new);
                } else if let Some(
                    OutputItem::Bash { done, .. }
                    | OutputItem::Terminal { done, .. }
                    | OutputItem::SubAgentActivity { done, .. },
                ) = call.detail.as_deref_mut()
                {
                    *done = call_done;
                }
                true
            });
        }
    }

    pub fn toggle_tool_call_content(&mut self, item_index: usize, tool_use_id: &str) {
        let panel_width = self
            .last_transcript_rect
            .map(|area| area.width)
            .unwrap_or(80);
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::ToolDispatch { calls } = item else {
                return false;
            };
            let Some(call) = calls.iter_mut().find(|call| call.id == tool_use_id) else {
                return false;
            };
            call.disclosure = crate::output::toggle_tool_call_content_disclosure(call, panel_width);
            true
        });
    }

    pub fn toggle_tool_call_detail(&mut self, item_index: usize, tool_use_id: &str) {
        let panel_width = self
            .last_transcript_rect
            .map(|area| area.width)
            .unwrap_or(80);
        self.mutate_item(item_index, OutputMutation::Interaction, |item| {
            let OutputItem::ToolDispatch { calls } = item else {
                return false;
            };
            let Some(call) = calls.iter_mut().find(|call| call.id == tool_use_id) else {
                return false;
            };
            call.disclosure = crate::output::toggle_tool_call_detail_disclosure(call, panel_width);
            true
        });
    }

    pub fn toggle_working_group_expansion(&mut self, item_index: usize, group_key: &str) {
        if !group_key.starts_with(crate::output::WORKING_GROUP_REGION_PREFIX)
            || !matches!(
                self.items.get(item_index),
                Some(OutputItem::ToolDispatch { .. })
            )
        {
            return;
        }
        if !self.expanded_tools.remove(group_key) {
            self.expanded_tools.insert(group_key.to_owned());
        }
        self.touch_item(item_index, OutputMutation::Interaction);
    }

    pub fn toggle_work_fold(&mut self, group_key: &str) -> bool {
        let Some(id) = group_key
            .strip_prefix(crate::output::WORK_FOLD_REGION_PREFIX)
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return false;
        };
        let Some(fold) = self.work_folds.iter_mut().find(|fold| fold.start_id == id) else {
            return false;
        };
        fold.toggle(Instant::now());
        true
    }

    pub fn toggle_latest_work_fold(&mut self) -> bool {
        let Some(fold) = self.work_folds.last_mut() else {
            return false;
        };
        fold.toggle(Instant::now());
        true
    }

    pub(crate) fn work_fold_projections(&self) -> Vec<crate::output::WorkFoldProjection> {
        let now = Instant::now();
        self.work_folds
            .iter()
            .filter_map(|fold| {
                let start_index = self.items.index_by_id(fold.start_id)?;
                let end_index = self.items.index_by_id(fold.end_id)?;
                let visibility = fold.visibility(now);
                let visible_members = visibility.ceil() as usize;
                let fraction = visibility.fract();
                Some(crate::output::WorkFoldProjection {
                    key: fold.start_id,
                    start_index,
                    end_index,
                    visible_members,
                    total_members: fold.member_count,
                    boundary_member: (fraction > f32::EPSILON)
                        .then(|| visible_members.saturating_sub(1)),
                    boundary_level: (fraction * 3.0).ceil().clamp(1.0, 3.0) as u8,
                    completed_steps: fold.completed_steps,
                    total_steps: fold.total_steps,
                    expanded: fold.is_expanded(),
                    animating: fold.is_animating(now),
                    hovered: self.hovered_output_node.as_ref().is_some_and(|(_, key)| {
                        key == &format!(
                            "{}{}",
                            crate::output::WORK_FOLD_REGION_PREFIX,
                            fold.start_id
                        )
                    }),
                    title: fold.title.clone(),
                    stats: fold.stats.clone(),
                })
            })
            .collect()
    }

    pub(crate) fn work_fold_scroll_anchor(&self, key: u64) -> Option<u32> {
        self.work_fold_scroll_anchor
            .filter(|(anchor_key, _)| *anchor_key == key)
            .map(|(_, row)| row)
    }

    pub(crate) fn set_work_fold_scroll_anchor(&mut self, key: u64, row: u32) {
        self.work_fold_scroll_anchor = Some((key, row));
    }

    pub(crate) fn clear_work_fold_scroll_anchor(&mut self) {
        self.work_fold_scroll_anchor = None;
    }

    fn apply_terminal_chunk_to_dispatch(
        &mut self,
        tool_use_id: &str,
        handle: String,
        bytes: Vec<u8>,
        screen: Option<TerminalScreen>,
        title: Option<String>,
        command: Option<String>,
    ) -> bool {
        self.mutate_tool_call(
            tool_use_id,
            OutputMutation::SemanticPreserveSource,
            move |call| {
                if let Some(OutputItem::Terminal {
                    title: current_title,
                    command: current_command,
                    screen: current_screen,
                    accumulated_bytes,
                    ..
                }) = call.detail.as_deref_mut()
                {
                    if current_title.is_none() {
                        *current_title = title;
                    }
                    if current_command.is_none() {
                        *current_command = command;
                    }
                    if let Some(screen) = screen {
                        *current_screen = screen;
                    }
                    append_bounded_bytes(accumulated_bytes, &bytes);
                } else {
                    call.detail = Some(Box::new(OutputItem::Terminal {
                        handle,
                        title,
                        command,
                        screen: screen.unwrap_or_else(|| TerminalScreen {
                            rows: 0,
                            cols: 0,
                            cells: Vec::new(),
                            cursor: None,
                            alt_screen: false,
                        }),
                        accumulated_bytes: bytes,
                        mode: TerminalViewMode::Capture,
                        done: false,
                        expanded: false,
                        scroll_offset: None,
                    }));
                }
                true
            },
        )
    }

    fn apply_bash_chunk_to_dispatch(
        &mut self,
        tool_use_id: &str,
        handle: String,
        text: String,
        title: Option<String>,
        command: Option<String>,
    ) -> bool {
        self.mutate_tool_call(
            tool_use_id,
            OutputMutation::SemanticPreserveSource,
            move |call| {
                if let Some(OutputItem::Bash {
                    title: current_title,
                    command: current_command,
                    output,
                    ..
                }) = call.detail.as_deref_mut()
                {
                    if current_title.is_none() {
                        *current_title = title;
                    }
                    if current_command.is_none() {
                        *current_command = command;
                    }
                    append_bounded_text(output, &text);
                } else {
                    call.detail = Some(Box::new(OutputItem::Bash {
                        handle,
                        title,
                        command,
                        output: text,
                        done: false,
                        expanded: false,
                    }));
                }
                true
            },
        )
    }

    fn finish_streaming_tool_detail(&mut self, tool_use_id: &str, status: ToolCallStatus) -> bool {
        self.mutate_tool_call(tool_use_id, OutputMutation::Semantic, |call| {
            call.status = status;
            call.ended_at = Some(Instant::now());
            if let Some(
                OutputItem::Terminal { done, .. }
                | OutputItem::Bash { done, .. }
                | OutputItem::SubAgentActivity { done, .. },
            ) = call.detail.as_deref_mut()
            {
                *done = true;
            }
            true
        })
    }

    fn apply_diff_to_dispatch(
        &mut self,
        tool_use_id: &str,
        title: String,
        old_content: Option<String>,
        new_content: Option<String>,
        unified_diff: Option<String>,
    ) -> bool {
        self.mutate_tool_call(tool_use_id, OutputMutation::Semantic, move |call| {
            call.detail = Some(Box::new(OutputItem::DiffPreview {
                title,
                old_content,
                new_content,
                unified_diff,
                expanded: false,
            }));
            true
        })
    }

    fn mutate_routed_sub_agent(
        &mut self,
        run_id: &str,
        mutation: OutputMutation,
        update: impl FnOnce(&mut OutputItem) -> bool,
    ) -> bool {
        let Some(route) = self.sub_agent_run_ids.get(run_id).cloned() else {
            return false;
        };
        let mut update = Some(update);
        self.mutate_item(route.item_index, mutation, |item| {
            let target = if let Some(tool_use_id) = route.tool_use_id.as_deref() {
                let OutputItem::ToolDispatch { calls } = item else {
                    return false;
                };
                let Some(call) = calls.iter_mut().find(|call| call.id == tool_use_id) else {
                    return false;
                };
                let Some(detail) = call.detail.as_deref_mut() else {
                    return false;
                };
                detail
            } else {
                item
            };
            update.take().is_some_and(|update| update(target))
        });
        true
    }

    pub fn apply_stream_frame(&mut self, frame: StreamFrame) {
        self.apply_permission_projection(&frame);
        match &frame {
            StreamFrame::ToolNode { tool_use_id, .. } => {
                self.session_activity.observe_call(tool_use_id);
                self.turn_activity.observe_call(tool_use_id);
            }
            StreamFrame::ToolResultMsg { message, .. } => {
                for part in &message.parts {
                    if let atman_runtime::message::MessagePart::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } = part
                    {
                        self.session_activity.observe_result(tool_use_id, *is_error);
                        self.turn_activity.observe_result(tool_use_id, *is_error);
                    }
                }
            }
            StreamFrame::FileEditApplied { path, metrics, .. } => {
                self.session_activity.observe_edit(path, *metrics);
                self.turn_activity.observe_edit(path, *metrics);
            }
            _ => {}
        }
        match frame {
            StreamFrame::TurnStarted { .. } => {
                let now = Instant::now();
                for fold in &mut self.work_folds {
                    fold.from_visible = fold.target_visible;
                    fold.started_at = now.checked_sub(fold.duration).unwrap_or(now);
                }
                self.turn_activity = ActivityTotals::default();
            }
            StreamFrame::TurnEnded { .. } => {
                self.discard_completed_final_answer_draft();
                if self.turn_activity.attempted_calls > 0 || self.turn_activity.applied_edits > 0 {
                    self.push_item(OutputItem::ActivitySummary {
                        turn: self.turn_activity.clone(),
                        session: self.session_activity.clone(),
                    });
                }
            }
            StreamFrame::ThinkingChunk { text, run_id, .. } => {
                self.discard_completed_final_answer_draft();
                let disclosure_key = llm_disclosure_key(run_id.as_deref());
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                self.waiting_for_llm = false;
                let last_index = self.items.len().checked_sub(1);
                let continues_thinking = last_index
                    .is_some_and(|index| matches!(self.items[index], OutputItem::Thinking { .. }));
                if let Some(index) = last_index.filter(|_| continues_thinking) {
                    self.mutate_item(index, OutputMutation::Semantic, |item| {
                        let OutputItem::Thinking { text: current, .. } = item else {
                            return false;
                        };
                        if text.is_empty() {
                            return false;
                        }
                        current.push_str(&text);
                        true
                    });
                    self.streaming = true;
                    self.reset_lag_state();
                    if let Some(id) = self.item_id(index) {
                        self.llm_disclosures
                            .entry(disclosure_key)
                            .or_default()
                            .thinking_id = Some(id);
                    }
                } else {
                    self.push_item(OutputItem::Thinking {
                        text,
                        done: false,
                        disclosure: Disclosure::Summary,
                        retried: false,
                    });
                    self.streaming = true;
                    if let Some(index) = self.items.len().checked_sub(1)
                        && let Some(id) = self.item_id(index)
                    {
                        self.llm_disclosures
                            .entry(disclosure_key)
                            .or_default()
                            .thinking_id = Some(id);
                    }
                }
            }
            StreamFrame::LlmChunk {
                text,
                model: chunk_model,
                run_id,
            } => {
                self.discard_completed_final_answer_draft();
                let disclosure_key = llm_disclosure_key(run_id.as_deref());
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    self.mutate_routed_sub_agent(rid, OutputMutation::Semantic, |item| {
                        let OutputItem::SubAgentActivity { model, output, .. } = item else {
                            return false;
                        };
                        let mut changed = false;
                        if model.is_empty() && !chunk_model.is_empty() {
                            *model = chunk_model;
                            changed = true;
                        }
                        if !text.is_empty() {
                            append_bounded_text(output, &text);
                            changed = true;
                        }
                        changed
                    });
                    self.streaming = true;
                    self.reset_lag_state();
                    return;
                }
                self.waiting_for_llm = false;
                self.finish_tracked_thinking(&disclosure_key, false);
                let last_index = self.items.len().checked_sub(1);
                let continues_assistant = last_index.is_some_and(|index| {
                    matches!(
                        self.items[index],
                        OutputItem::AssistantMd {
                            streaming: true,
                            ..
                        }
                    )
                });
                if let Some(index) = last_index.filter(|_| continues_assistant) {
                    self.mutate_item(index, OutputMutation::SemanticPreserveSource, |item| {
                        let OutputItem::AssistantMd { md, streaming, .. } = item else {
                            return false;
                        };
                        if !*streaming || text.is_empty() {
                            return false;
                        }
                        md.push_str(&text);
                        true
                    });
                    self.streaming = true;
                    self.reset_lag_state();
                    if let Some(id) = self.item_id(index) {
                        self.llm_disclosures
                            .entry(disclosure_key)
                            .or_default()
                            .assistant_id = Some(id);
                    }
                } else {
                    self.push_item(OutputItem::AssistantMd {
                        md: text,
                        streaming: true,
                        retried: false,
                    });
                    self.streaming = true;
                    self.terminal_throttle = Some(Instant::now());
                    if let Some(index) = self.items.len().checked_sub(1)
                        && let Some(id) = self.item_id(index)
                    {
                        self.llm_disclosures
                            .entry(disclosure_key)
                            .or_default()
                            .assistant_id = Some(id);
                    }
                }
            }
            StreamFrame::LlmRetry => {
                if !self.final_answer_drafts.is_empty() {
                    let fold_ids = self
                        .final_answer_drafts
                        .values()
                        .filter_map(|draft| draft.work_fold_id)
                        .collect::<HashSet<_>>();
                    self.final_answer_drafts.clear();
                    self.final_answer_drafts_completed = false;
                    self.work_folds
                        .retain(|fold| !fold_ids.contains(&fold.start_id));
                }
                while self.items.last().is_some_and(|item| {
                    matches!(item, OutputItem::ToolDispatch { calls } if calls.iter().all(|call| call.draft_index.is_some()))
                }) {
                    let index = self.items.len() - 1;
                    self.remove_item(index);
                }
                let disclosure_keys = self.llm_disclosures.keys().cloned().collect::<Vec<_>>();
                for key in disclosure_keys {
                    self.finish_llm_disclosure(&key, true);
                }
                let attempt_start = self
                    .items
                    .iter()
                    .rposition(|item| matches!(item, OutputItem::UserTurn { .. }))
                    .map_or(0, |index| index + 1);
                for index in attempt_start..self.items.len() {
                    self.mutate_item(index, OutputMutation::Semantic, |item| match item {
                        OutputItem::Thinking { done, retried, .. } => {
                            let changed = !*done || !*retried;
                            *done = true;
                            *retried = true;
                            changed
                        }
                        OutputItem::AssistantMd {
                            streaming, retried, ..
                        } => {
                            let changed = *streaming || !*retried;
                            *streaming = false;
                            *retried = true;
                            changed
                        }
                        _ => false,
                    });
                }
                self.waiting_for_llm = true;
            }
            StreamFrame::LlmDone { run_id, .. } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                let disclosure_key = llm_disclosure_key(run_id.as_deref());
                self.finish_llm_disclosure(&disclosure_key, false);
                self.final_answer_drafts_completed = !self.final_answer_drafts.is_empty();
                self.streaming = false;
                self.reset_lag_state();
            }
            StreamFrame::ToolCallDraft {
                index,
                call_id,
                name,
                arguments_delta,
                run_id,
            } => {
                self.discard_completed_final_answer_draft();
                self.apply_tool_call_draft(
                    run_id.as_deref(),
                    index,
                    call_id,
                    name,
                    arguments_delta,
                );
                self.streaming = true;
                self.reset_lag_state();
            }
            StreamFrame::FileEditApplied {
                tool_use_id,
                path,
                metrics,
                ..
            } => {
                if let Some(tool_use_id) = tool_use_id {
                    self.mutate_tool_call(&tool_use_id, OutputMutation::Semantic, |call| {
                        call.applied_edit = Some((path, metrics));
                        true
                    });
                }
            }
            StreamFrame::ToolUseStart { .. } => {}
            StreamFrame::ToolUseDone { id, ok, .. } => {
                let status = if ok {
                    crate::task_panel::ActivityStatus::Ok
                } else {
                    crate::task_panel::ActivityStatus::Err
                };
                self.finish_activity_node(None, &id, status);
            }
            StreamFrame::Note(text) => {
                self.push_item(OutputItem::SystemNote {
                    text,
                    level: NoteLevel::Info,
                });
            }
            StreamFrame::Notification(frame) => {
                let text = frame.message;
                let level = match frame.level {
                    atman_runtime::notify::NotifyLevel::Error => NoteLevel::Error,
                    atman_runtime::notify::NotifyLevel::Warn => NoteLevel::Warn,
                    atman_runtime::notify::NotifyLevel::Info => NoteLevel::Info,
                    atman_runtime::notify::NotifyLevel::Success => NoteLevel::Success,
                    atman_runtime::notify::NotifyLevel::Debug => NoteLevel::Debug,
                };
                match frame.location {
                    atman_runtime::notify::NotifyLocation::Toast => {
                        let ttl = match frame.lifecycle {
                            atman_runtime::notify::NotifyLifecycle::Ttl(d) => d,
                            _ => std::time::Duration::from_secs(3),
                        };
                        self.push_toast(text, level, ttl, ToastPosition::TopRight);
                    }
                    atman_runtime::notify::NotifyLocation::Status => {
                        let key = match &frame.stack {
                            atman_runtime::notify::NotifyStack::Replace { key } => key.clone(),
                            atman_runtime::notify::NotifyStack::Coalesce { key } => key.clone(),
                            _ => "default".into(),
                        };
                        self.push_status(key, text);
                    }
                    atman_runtime::notify::NotifyLocation::Modal => {
                        self.modal_notification = Some(text);
                    }
                    _ => {
                        let replace_key = match &frame.stack {
                            atman_runtime::notify::NotifyStack::Replace { key }
                            | atman_runtime::notify::NotifyStack::Coalesce { key } => {
                                Some(key.clone())
                            }
                            _ => None,
                        };
                        let completes_llm_wait = replace_key
                            .as_deref()
                            .is_some_and(|key| key.starts_with("llm-call:"))
                            && matches!(level, NoteLevel::Error | NoteLevel::Success);
                        if let Some(key) = replace_key.as_ref()
                            && let Some(index) = self.inline_note_indices.get(key).copied()
                            && matches!(self.items.get(index), Some(OutputItem::SystemNote { .. }))
                        {
                            self.mutate_item(index, OutputMutation::Semantic, |item| {
                                let OutputItem::SystemNote {
                                    text: current_text,
                                    level: current_level,
                                } = item
                                else {
                                    return false;
                                };
                                if *current_text == text && *current_level == level {
                                    return false;
                                }
                                *current_text = text;
                                *current_level = level;
                                true
                            });
                            self.reset_lag_state();
                        } else {
                            let index = self.items.len();
                            self.push_item(OutputItem::SystemNote { text, level });
                            if let Some(key) = replace_key {
                                self.inline_note_indices.insert(key, index);
                            }
                        }
                        if completes_llm_wait {
                            self.streaming = false;
                            self.waiting_for_llm = false;
                        }
                    }
                }
            }
            frame @ (StreamFrame::FlowGraph { .. }
            | StreamFrame::FlowStart { .. }
            | StreamFrame::FlowNodeStart { .. }
            | StreamFrame::FlowNodeEnd { .. }
            | StreamFrame::FlowDone { .. }
            | StreamFrame::ToolNode { .. }
            | StreamFrame::LlmCallStats { .. }
            | StreamFrame::AssistantMsg { .. }
            | StreamFrame::ToolResultMsg { .. }
            | StreamFrame::ToolPendingApproval { .. }
            | StreamFrame::ToolApproved { .. }
            | StreamFrame::ToolDenied { .. }
            | StreamFrame::PermissionRequestCreated { .. }
            | StreamFrame::PermissionRequestTargeted { .. }
            | StreamFrame::PermissionRequestDeferred { .. }
            | StreamFrame::PermissionRequestApproved { .. }
            | StreamFrame::PermissionRequestDenied { .. }
            | StreamFrame::PermissionRequestCancelled { .. }
            | StreamFrame::PermissionGroupCreated { .. }
            | StreamFrame::PermissionGroupUpdated { .. }
            | StreamFrame::PermissionGroupResolved { .. }
            | StreamFrame::PermissionGrantCreated { .. }
            | StreamFrame::PermissionGrantExpired { .. }
            | StreamFrame::UnrestrictedExecution { .. }) => {
                if let StreamFrame::AssistantMsg {
                    flow_run_id,
                    message,
                } = &frame
                    && flow_run_id
                        .as_ref()
                        .is_none_or(|run_id| !self.sub_agent_run_ids.contains_key(run_id))
                {
                    if message.origin == atman_runtime::message::MessageOrigin::FinalAnswer {
                        self.commit_final_answer_message(message);
                    } else {
                        if atman_runtime::tools::final_answer::attempted(message) {
                            self.discard_completed_final_answer_draft();
                        }
                        self.append_tool_dispatch(message);
                    }
                }
                if let StreamFrame::ToolResultMsg { message, .. } = &frame {
                    self.apply_tool_result_to_dispatch(message);
                }
                match &frame {
                    StreamFrame::FlowNodeStart {
                        run_id,
                        node_id,
                        kind,
                        label,
                        parent_node_id,
                    } => {
                        self.upsert_activity_node(crate::task_panel::ActivityNode {
                            run_id: run_id.clone(),
                            node_id: node_id.clone(),
                            parent_node_id: parent_node_id.clone(),
                            label: label.clone(),
                            kind: kind.clone(),
                            status: crate::task_panel::ActivityStatus::Running,
                            started_at: std::time::Instant::now(),
                            ended_at: None,
                        });
                    }
                    StreamFrame::ToolNode {
                        run_id,
                        parent_node_id,
                        tool_use_id,
                        tool,
                        call_intent,
                        ..
                    } => {
                        let label = call_intent
                            .as_ref()
                            .map(|intent| intent.as_str().to_owned())
                            .unwrap_or_else(|| tool.clone());
                        self.upsert_activity_node(crate::task_panel::ActivityNode {
                            run_id: run_id.clone(),
                            node_id: tool_use_id.clone(),
                            parent_node_id: Some(parent_node_id.clone()),
                            label,
                            kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                                path: tool.clone(),
                            },
                            status: crate::task_panel::ActivityStatus::Running,
                            started_at: std::time::Instant::now(),
                            ended_at: None,
                        });
                    }
                    StreamFrame::FlowNodeEnd {
                        run_id,
                        node_id,
                        status,
                        ..
                    } => {
                        let st = match status {
                            atman_runtime::event::FlowNodeStatus::Ok => {
                                crate::task_panel::ActivityStatus::Ok
                            }
                            atman_runtime::event::FlowNodeStatus::Err => {
                                crate::task_panel::ActivityStatus::Err
                            }
                            atman_runtime::event::FlowNodeStatus::Cancelled => {
                                crate::task_panel::ActivityStatus::Cancelled
                            }
                        };
                        for n in self.activity_nodes.iter_mut().rev() {
                            if n.run_id == *run_id && n.node_id == *node_id {
                                n.status = st;
                                n.ended_at = Some(std::time::Instant::now());
                                break;
                            }
                        }
                    }
                    StreamFrame::ToolResultMsg {
                        flow_run_id: Some(run_id),
                        message,
                    } => {
                        for part in &message.parts {
                            if let atman_runtime::message::MessagePart::ToolResult {
                                tool_use_id,
                                is_error,
                                ..
                            } = part
                            {
                                let status = if *is_error {
                                    crate::task_panel::ActivityStatus::Err
                                } else {
                                    crate::task_panel::ActivityStatus::Ok
                                };
                                self.finish_activity_node(Some(run_id), tool_use_id, status);
                            }
                        }
                    }
                    _ => {}
                }
                let (is_done, cancelled, done_run_id) = match &frame {
                    StreamFrame::FlowDone {
                        cancelled,
                        suicide,
                        run_id,
                        ..
                    } => (true, *cancelled || *suicide, Some(run_id.as_str())),
                    _ => (false, false, None),
                };
                self.ensure_workflow_panel_and_apply(&frame);
                if is_done {
                    if let Some(rid) = done_run_id {
                        let st = if cancelled {
                            crate::task_panel::ActivityStatus::Cancelled
                        } else {
                            crate::task_panel::ActivityStatus::Ok
                        };
                        let now = std::time::Instant::now();
                        for n in self.activity_nodes.iter_mut() {
                            if n.run_id == rid
                                && n.status == crate::task_panel::ActivityStatus::Running
                            {
                                n.status = st;
                                n.ended_at = Some(now);
                            }
                        }
                    }
                    // Only close the panel for top-level flows — subflow
                    // FlowDone must not close the parent panel.
                    if done_run_id.is_some_and(|rid| self.top_level_run_ids.contains(rid)) {
                        let panel_idx = done_run_id
                            .and_then(|rid| self.workflow_run_to_panel.get(rid).copied());
                        self.close_current_workflow_panel(cancelled, panel_idx);
                        if let Some(rid) = done_run_id {
                            self.top_level_run_ids.remove(rid);
                        }
                    }
                    self.streaming = false;
                }
            }
            StreamFrame::TerminalChunk {
                handle,
                tool_use_id,
                bytes,
                screen,
                state: _,
                call_intent,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    let task_command = self.task_command(&handle);
                    self.apply_detached_terminal_chunk(
                        handle,
                        bytes,
                        screen,
                        call_intent.map(|intent| intent.as_str().to_owned()),
                        task_command,
                    );
                    return;
                }
                let task_command = self.task_command(&handle);
                if let Some(tool_use_id) = tool_use_id
                    && self.apply_terminal_chunk_to_dispatch(
                        &tool_use_id,
                        handle.clone(),
                        bytes.clone(),
                        screen.clone(),
                        call_intent
                            .as_ref()
                            .map(|intent| intent.as_str().to_owned()),
                        task_command.clone(),
                    )
                {
                    self.waiting_for_llm = false;
                    self.reset_lag_state();
                    return;
                }
                self.waiting_for_llm = false;
                if self.scroll_offset >= self.max_scroll_offset() {
                    self.follow_tail = true;
                }
                let existing_index = self.find_item_by_handle(&handle).and_then(|idx| match &self
                    .items[idx]
                {
                    OutputItem::Terminal { done: false, .. } => Some(idx),
                    _ => None,
                });
                if let Some(index) = existing_index {
                    let proposed_title = call_intent.map(|intent| intent.as_str().to_owned());
                    self.mutate_item(index, OutputMutation::Semantic, |item| {
                        let OutputItem::Terminal {
                            title,
                            command,
                            screen: current_screen,
                            accumulated_bytes,
                            ..
                        } = item
                        else {
                            return false;
                        };
                        let mut changed = false;
                        if title.is_none() && proposed_title.is_some() {
                            *title = proposed_title;
                            changed = true;
                        }
                        if command.is_none() && task_command.is_some() {
                            *command = task_command;
                            changed = true;
                        }
                        if let Some(new_screen) = screen
                            && *current_screen != new_screen
                        {
                            *current_screen = new_screen;
                            changed = true;
                        }
                        if !bytes.is_empty() {
                            append_bounded_bytes(accumulated_bytes, &bytes);
                            changed = true;
                        }
                        changed
                    });
                    self.reset_lag_state();
                } else {
                    self.push_item(OutputItem::Terminal {
                        handle,
                        title: call_intent.map(|intent| intent.as_str().to_owned()),
                        command: task_command,
                        screen: screen.unwrap_or_else(|| {
                            atman_runtime::tools::term::TerminalScreen {
                                rows: 0,
                                cols: 0,
                                cells: Vec::new(),
                                cursor: None,
                                alt_screen: false,
                            }
                        }),
                        accumulated_bytes: bytes,
                        mode: TerminalViewMode::Capture,
                        done: false,
                        expanded: false,
                        scroll_offset: None,
                    });
                    self.reset_lag_state();
                }
            }
            StreamFrame::TerminalExited {
                handle,
                tool_use_id,
                exit_code,
                call_intent,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    if !self.detached_task_details.contains_key(&handle) {
                        let task_command = self.task_command(&handle);
                        self.apply_detached_terminal_chunk(
                            handle.clone(),
                            Vec::new(),
                            None,
                            call_intent.map(|intent| intent.as_str().to_owned()),
                            task_command,
                        );
                    }
                    self.finish_detached_task_detail(&handle);
                    return;
                }
                let task_command = self.task_command(&handle);
                if let Some(tool_use_id) = tool_use_id
                    && self.finish_streaming_tool_detail(
                        &tool_use_id,
                        if exit_code == Some(0) {
                            ToolCallStatus::Ok
                        } else {
                            ToolCallStatus::Error
                        },
                    )
                {
                    return;
                }
                if let Some(idx) = self.find_item_by_handle(&handle) {
                    let proposed_title = call_intent.map(|intent| intent.as_str().to_owned());
                    self.mutate_item(idx, OutputMutation::Semantic, |item| {
                        let OutputItem::Terminal {
                            title,
                            command,
                            done,
                            ..
                        } = item
                        else {
                            return false;
                        };
                        let mut changed = false;
                        if title.is_none() && proposed_title.is_some() {
                            *title = proposed_title;
                            changed = true;
                        }
                        if command.is_none() && task_command.is_some() {
                            *command = task_command;
                            changed = true;
                        }
                        if !*done {
                            *done = true;
                            changed = true;
                        }
                        changed
                    });
                } else {
                    self.push_item(OutputItem::Terminal {
                        handle,
                        title: call_intent.map(|intent| intent.as_str().to_owned()),
                        command: task_command,
                        screen: TerminalScreen {
                            rows: 0,
                            cols: 0,
                            cells: Vec::new(),
                            cursor: None,
                            alt_screen: false,
                        },
                        accumulated_bytes: Vec::new(),
                        mode: TerminalViewMode::Capture,
                        done: true,
                        expanded: false,
                        scroll_offset: None,
                    });
                }
            }
            StreamFrame::BashChunk {
                handle,
                tool_use_id,
                kind,
                line,
                call_intent,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    let task_command = self.task_command(&handle);
                    let prefix = if kind == "stderr" { "[err] " } else { "" };
                    self.apply_detached_bash_chunk(
                        handle,
                        format!("{prefix}{line}"),
                        call_intent.map(|intent| intent.as_str().to_owned()),
                        task_command,
                    );
                    return;
                }
                let task_command = self.task_command(&handle);
                let prefix = if kind == "stderr" { "[err] " } else { "" };
                if let Some(tool_use_id) = tool_use_id
                    && self.apply_bash_chunk_to_dispatch(
                        &tool_use_id,
                        handle.clone(),
                        format!("{prefix}{line}"),
                        call_intent
                            .as_ref()
                            .map(|intent| intent.as_str().to_owned()),
                        task_command.clone(),
                    )
                {
                    self.waiting_for_llm = false;
                    self.reset_lag_state();
                    return;
                }
                self.waiting_for_llm = false;
                if self.scroll_offset >= self.max_scroll_offset() {
                    self.follow_tail = true;
                }
                let existing_index = self.find_item_by_handle(&handle).and_then(|idx| match &self
                    .items[idx]
                {
                    OutputItem::Bash { done: false, .. } => Some(idx),
                    _ => None,
                });
                if let Some(index) = existing_index {
                    let proposed_title = call_intent.map(|intent| intent.as_str().to_owned());
                    self.mutate_item(index, OutputMutation::SemanticPreserveSource, |item| {
                        let OutputItem::Bash {
                            title,
                            command,
                            output,
                            ..
                        } = item
                        else {
                            return false;
                        };
                        let mut changed = false;
                        if title.is_none() && proposed_title.is_some() {
                            *title = proposed_title;
                            changed = true;
                        }
                        if command.is_none() && task_command.is_some() {
                            *command = task_command;
                            changed = true;
                        }
                        if !prefix.is_empty() || !line.is_empty() {
                            append_bounded_text(output, prefix);
                            append_bounded_text(output, &line);
                            changed = true;
                        }
                        changed
                    });
                    self.reset_lag_state();
                } else {
                    let mut output = String::new();
                    append_bounded_text(&mut output, prefix);
                    append_bounded_text(&mut output, &line);
                    self.push_item(OutputItem::Bash {
                        handle,
                        title: call_intent.map(|intent| intent.as_str().to_owned()),
                        command: task_command,
                        output,
                        done: false,
                        expanded: false,
                    });
                    self.reset_lag_state();
                }
            }
            StreamFrame::BashExited {
                handle,
                tool_use_id,
                exit_code,
                error,
                call_intent,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    if !self.detached_task_details.contains_key(&handle) {
                        let task_command = self.task_command(&handle);
                        self.apply_detached_bash_chunk(
                            handle.clone(),
                            String::new(),
                            call_intent.map(|intent| intent.as_str().to_owned()),
                            task_command,
                        );
                    }
                    self.finish_detached_task_detail(&handle);
                    return;
                }
                let task_command = self.task_command(&handle);
                if let Some(tool_use_id) = tool_use_id
                    && self.finish_streaming_tool_detail(
                        &tool_use_id,
                        if exit_code == Some(0) && error.is_none() {
                            ToolCallStatus::Ok
                        } else {
                            ToolCallStatus::Error
                        },
                    )
                {
                    return;
                }
                if let Some(idx) = self.find_item_by_handle(&handle) {
                    let proposed_title = call_intent.map(|intent| intent.as_str().to_owned());
                    self.mutate_item(idx, OutputMutation::SemanticPreserveSource, |item| {
                        let OutputItem::Bash {
                            title,
                            command,
                            done,
                            ..
                        } = item
                        else {
                            return false;
                        };
                        let mut changed = false;
                        if title.is_none() && proposed_title.is_some() {
                            *title = proposed_title;
                            changed = true;
                        }
                        if command.is_none() && task_command.is_some() {
                            *command = task_command;
                            changed = true;
                        }
                        if !*done {
                            *done = true;
                            changed = true;
                        }
                        changed
                    });
                } else {
                    self.push_item(OutputItem::Bash {
                        handle,
                        title: call_intent.map(|intent| intent.as_str().to_owned()),
                        command: task_command,
                        output: String::new(),
                        done: true,
                        expanded: false,
                    });
                }
            }
            StreamFrame::DiffPreview {
                title,
                tool_use_id,
                old_content,
                new_content,
                unified_diff,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                if let Some(tool_use_id) = tool_use_id
                    && self.apply_diff_to_dispatch(
                        &tool_use_id,
                        title.clone(),
                        old_content.clone(),
                        new_content.clone(),
                        unified_diff.clone(),
                    )
                {
                    return;
                }
                self.push_item(OutputItem::DiffPreview {
                    title,
                    old_content,
                    new_content,
                    unified_diff,
                    expanded: false,
                });
            }
            StreamFrame::CompactionSummary {
                phase,
                range_start,
                range_end,
                summary,
                before_tokens,
                after_tokens,
                compacted_count,
            } => {
                let existing_index = self.items.len().checked_sub(1).filter(|index| {
                    matches!(
                        &self.items[*index],
                        OutputItem::CompactionSummary {
                            range_start: current_start,
                            range_end: current_end,
                            ..
                        } if *current_start == range_start && *current_end == range_end
                    )
                });
                if let Some(index) = existing_index {
                    self.mutate_item(index, OutputMutation::Semantic, |item| {
                        let OutputItem::CompactionSummary {
                            phase: current_phase,
                            summary: current_summary,
                            before_tokens: current_before,
                            after_tokens: current_after,
                            compacted_count: current_count,
                            disclosure: _,
                            ..
                        } = item
                        else {
                            return false;
                        };
                        if *current_phase == phase
                            && *current_summary == summary
                            && *current_before == before_tokens
                            && *current_after == after_tokens
                            && *current_count == compacted_count
                        {
                            return false;
                        }
                        *current_phase = phase;
                        *current_summary = summary;
                        *current_before = before_tokens;
                        *current_after = after_tokens;
                        *current_count = compacted_count;
                        true
                    });
                } else {
                    self.push_item(OutputItem::CompactionSummary {
                        phase,
                        range_start,
                        range_end,
                        summary,
                        before_tokens,
                        after_tokens,
                        compacted_count,
                        disclosure: Disclosure::Summary,
                    });
                }
            }
            StreamFrame::CompactionDelta {
                range_start,
                range_end,
                text,
            } => {
                if let Some(index) = self.items.iter().rposition(|item| {
                    matches!(item, OutputItem::CompactionSummary {
                        phase: CompactionPhase::Running,
                        range_start: current_start,
                        range_end: current_end,
                        ..
                    } if *current_start == range_start && *current_end == range_end)
                }) {
                    self.mutate_item(index, OutputMutation::SemanticPreserveSource, |item| {
                        let OutputItem::CompactionSummary { summary, .. } = item else {
                            return false;
                        };
                        if text.is_empty() {
                            return false;
                        }
                        summary.push_str(&text);
                        true
                    });
                }
            }
            StreamFrame::MermaidDiagram { source } => {
                self.push_item(OutputItem::MermaidDiagram { source });
                self.reset_lag_state();
            }
            StreamFrame::SubAgentStarted {
                handle,
                tool_use_id,
                goal,
                child_run_id,
                model,
            } => {
                let detail = OutputItem::SubAgentActivity {
                    handle: handle.clone(),
                    goal,
                    child_run_id: child_run_id.clone(),
                    model,
                    status: "running".into(),
                    output: String::new(),
                    iteration: 0,
                    done: false,
                    expanded: false,
                    messages: Vec::new(),
                    workflow_graph: WorkflowProjection::new(atman_runtime::event::TurnId::now()),
                    expanded_nodes: HashSet::new(),
                    workflow_expanded: false,
                };
                let route = if let Some(tool_use_id) = tool_use_id
                    && self.mutate_tool_call(&tool_use_id, OutputMutation::Semantic, |call| {
                        call.detail = Some(Box::new(detail.clone()));
                        true
                    }) {
                    let item_index = self.items.iter().rposition(|item| {
                        matches!(item, OutputItem::ToolDispatch { calls } if calls.iter().any(|call| call.id == tool_use_id))
                    }).unwrap_or_else(|| self.items.len().saturating_sub(1));
                    SubAgentRoute {
                        item_index,
                        tool_use_id: Some(tool_use_id),
                    }
                } else {
                    let item_index = self.items.len();
                    self.push_item(detail);
                    SubAgentRoute {
                        item_index,
                        tool_use_id: None,
                    }
                };
                self.sub_agent_run_ids.insert(child_run_id, route);
            }
            StreamFrame::SubAgentDone {
                handle,
                status,
                final_text,
            } => {
                let run_id = self.sub_agent_run_ids.iter().find_map(|(run_id, route)| {
                    let item = self.items.get(route.item_index)?;
                    let matches = if let Some(tool_use_id) = route.tool_use_id.as_deref() {
                        let OutputItem::ToolDispatch { calls } = item else { return None; };
                        calls.iter().any(|call| {
                            call.id == tool_use_id
                                && matches!(call.detail.as_deref(), Some(OutputItem::SubAgentActivity { handle: current, .. }) if current == &handle)
                        })
                    } else {
                        matches!(item, OutputItem::SubAgentActivity { handle: current, .. } if current == &handle)
                    };
                    matches.then(|| run_id.clone())
                });
                if let Some(run_id) = run_id {
                    let call_status = if status == "ok" {
                        ToolCallStatus::Ok
                    } else {
                        ToolCallStatus::Error
                    };
                    let tool_use_id = self
                        .sub_agent_run_ids
                        .get(&run_id)
                        .and_then(|route| route.tool_use_id.clone());
                    self.mutate_routed_sub_agent(&run_id, OutputMutation::Semantic, |item| {
                        let OutputItem::SubAgentActivity {
                            status: current_status,
                            output,
                            done,
                            ..
                        } = item
                        else {
                            return false;
                        };
                        let output_changed = !final_text.is_empty() && *output != final_text;
                        let changed = *current_status != status || output_changed || !*done;
                        if !changed {
                            return false;
                        }
                        *current_status = status;
                        if output_changed {
                            *output = final_text;
                        }
                        *done = true;
                        true
                    });
                    if let Some(tool_use_id) = tool_use_id {
                        self.finish_streaming_tool_detail(&tool_use_id, call_status);
                    }
                }
            }
            StreamFrame::Unknown => {}
        }
    }

    fn route_to_workflow_panel(&mut self, frame: &StreamFrame) {
        let target_idx = match frame_run_id(frame) {
            Some(run_id) => self.workflow_run_to_panel.get(run_id).copied(),
            None => self
                .items
                .iter()
                .rposition(|item| matches!(item, OutputItem::WorkflowPanel { .. })),
        };
        let Some(idx) = target_idx else {
            return;
        };
        self.mutate_item(idx, OutputMutation::Semantic, |item| {
            let OutputItem::WorkflowPanel { graph, .. } = item else {
                return false;
            };
            graph.apply_stream_frame(frame).changed()
        });
    }

    fn ensure_workflow_panel_and_apply(&mut self, frame: &StreamFrame) {
        // Subflow recursion creates a new run_id per iteration. If a FlowStart's
        // parent_run_id is already mapped to a SubAgentActivity, chain the new
        // run_id to the same item so subsequent LlmChunk/ThinkingChunk/etc.
        // frames route to the sub-agent instead of the main document flow.
        if let StreamFrame::FlowStart {
            run_id,
            parent_run_id: Some(parent_rid),
            ..
        } = frame
        {
            if self.sub_agent_run_ids.contains_key(run_id) {
                // already mapped (e.g. subagent flow's own FlowStart)
            } else if let Some(route) = self.sub_agent_run_ids.get(parent_rid).cloned() {
                self.sub_agent_run_ids.insert(run_id.clone(), route);
            }
        }
        if let Some(rid) = frame_run_id(frame)
            && let Some(route) = self.sub_agent_run_ids.get(rid).cloned()
        {
            let routed = self.items.get(route.item_index).is_some();
            self.mutate_routed_sub_agent(rid, OutputMutation::Semantic, |item| {
                let OutputItem::SubAgentActivity {
                    workflow_graph,
                    messages,
                    model,
                    ..
                } = item
                else {
                    return false;
                };
                let mut changed = workflow_graph.apply_stream_frame(frame).changed();
                if model.is_empty()
                    && let StreamFrame::LlmCallStats {
                        model: call_model, ..
                    } = frame
                    && !call_model.is_empty()
                {
                    *model = call_model.clone();
                    changed = true;
                }
                if let StreamFrame::AssistantMsg { message, .. }
                | StreamFrame::ToolResultMsg { message, .. } = frame
                {
                    messages.push(message.clone());
                    if messages.len() > 100 {
                        let start = messages.len() - 100;
                        messages.drain(..start);
                    }
                    changed = true;
                }
                changed
            });
            if routed {
                return;
            }
        }
        let is_panel_creator = matches!(
            frame,
            StreamFrame::FlowStart { .. } | StreamFrame::FlowGraph { .. }
        );

        // For subflows (FlowStart with parent_run_id), find and reuse the parent's panel.
        if let StreamFrame::FlowStart {
            run_id,
            parent_run_id: Some(parent_rid),
            ..
        } = frame
        {
            if let Some(&parent_idx) = self.workflow_run_to_panel.get(parent_rid) {
                self.mutate_item(parent_idx, OutputMutation::Semantic, |item| {
                    let OutputItem::WorkflowPanel {
                        ended_at,
                        cancelled,
                        ..
                    } = item
                    else {
                        return false;
                    };
                    // Don't reopen a cancelled panel — late subflow events
                    // after a hard stop must not resurrect the spinner.
                    if ended_at.is_some() && !*cancelled {
                        *ended_at = None;
                        true
                    } else {
                        false
                    }
                });
                // Insert subflow run_id so nested subflows (e.g. flow.spawn)
                // can find the parent panel. The top_level_run_ids guard in
                // apply_stream_frame prevents subflow FlowDone from closing it.
                self.workflow_run_to_panel
                    .insert(run_id.clone(), parent_idx);
                self.mutate_item(parent_idx, OutputMutation::Semantic, |item| {
                    let OutputItem::WorkflowPanel { graph, .. } = item else {
                        return false;
                    };
                    graph.apply_stream_frame(frame).changed()
                });
                return;
            }
            // Parent not in HashMap (e.g. after session resume).
            // Create a new panel for this subflow instead of falling through.
            let turn_index = self
                .items
                .iter()
                .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
                .count();
            let idx = self.items.len();
            self.push_item(OutputItem::WorkflowPanel {
                turn_index,
                graph: WorkflowProjection::new(atman_runtime::event::TurnId::now()),
                expanded_nodes: HashSet::new(),
                panel_expanded: false,
                started_at: std::time::Instant::now(),
                ended_at: None,
                cancelled: false,
            });
            self.top_level_run_ids.insert(run_id.clone());
            self.workflow_run_to_panel.insert(run_id.clone(), idx);
            self.route_to_workflow_panel(frame);
            return;
        }

        // Non-panel-creator events (FlowNodeStart, FlowNodeEnd, etc.) must not
        // create phantom panels. Just route to the last open panel.
        if !is_panel_creator {
            self.route_to_workflow_panel(frame);
            return;
        }
        let mut panel_after_user_turn = false;
        let mut reopen_idx: Option<usize> = None;
        for (i, it) in self.items.iter().enumerate().rev() {
            match it {
                OutputItem::WorkflowPanel { ended_at: None, .. } => {
                    // FlowGraph reuses an open panel; FlowStart creates a new one
                    // unless its run_id is already mapped (from a prior FlowGraph).
                    if matches!(frame, StreamFrame::FlowGraph { .. })
                        || matches!(frame, StreamFrame::FlowStart { run_id, .. }
                            if self.workflow_run_to_panel.contains_key(run_id))
                    {
                        panel_after_user_turn = true;
                    }
                    break;
                }
                OutputItem::WorkflowPanel {
                    ended_at: Some(_),
                    cancelled: true,
                    ..
                } => {
                    reopen_idx = Some(i);
                    panel_after_user_turn = true;
                    break;
                }
                OutputItem::WorkflowPanel { .. } => {}
                OutputItem::UserTurn { .. } => break,
                _ => {}
            }
        }
        if let Some(idx) = reopen_idx {
            self.mutate_item(idx, OutputMutation::Semantic, |item| {
                let OutputItem::WorkflowPanel {
                    ended_at,
                    cancelled,
                    ..
                } = item
                else {
                    return false;
                };
                let changed = ended_at.is_some() || *cancelled;
                *ended_at = None;
                *cancelled = false;
                changed
            });
            if let StreamFrame::FlowStart { run_id, .. } = frame {
                self.workflow_run_to_panel.insert(run_id.clone(), idx);
                self.top_level_run_ids.insert(run_id.clone());
            }
        }
        if !panel_after_user_turn {
            let turn_index = self
                .items
                .iter()
                .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
                .count();
            let idx = self.items.len();
            self.push_item(OutputItem::WorkflowPanel {
                turn_index,
                graph: WorkflowProjection::new(atman_runtime::event::TurnId::now()),
                expanded_nodes: HashSet::new(),
                panel_expanded: false,
                started_at: std::time::Instant::now(),
                ended_at: None,
                cancelled: false,
            });
            if let StreamFrame::FlowStart { run_id, .. } = frame {
                self.workflow_run_to_panel.insert(run_id.clone(), idx);
                self.top_level_run_ids.insert(run_id.clone());
            }
        }
        self.route_to_workflow_panel(frame);
    }

    pub fn close_current_workflow_panel(&mut self, cancelled: bool, panel_idx: Option<usize>) {
        let idx = panel_idx.or(self.last_workflow_panel_idx);
        if let Some(idx) = idx {
            self.mutate_item(idx, OutputMutation::Semantic, |item| {
                let OutputItem::WorkflowPanel {
                    ended_at,
                    cancelled: cancelled_flag,
                    ..
                } = item
                else {
                    return false;
                };
                let was_open = ended_at.is_none();
                if was_open {
                    *ended_at = Some(Instant::now());
                }
                let changed = was_open || *cancelled_flag != cancelled;
                *cancelled_flag = cancelled;
                changed
            });
        }
    }

    pub fn cancel_running_activities(&mut self) {
        let now = std::time::Instant::now();
        for n in self.activity_nodes.iter_mut() {
            if n.status == crate::task_panel::ActivityStatus::Running {
                n.status = crate::task_panel::ActivityStatus::Cancelled;
                n.ended_at = Some(now);
            }
        }
        self.streaming = false;
        self.waiting_for_llm = false;
        self.reset_lag_state();
    }

    pub fn record_lag(&mut self, dropped: u64, now: Instant) {
        let within_cooldown = self
            .last_lag_at
            .map(|t| now.duration_since(t) < LAG_COOLDOWN)
            .unwrap_or(false);
        if within_cooldown
            && let Some(idx) = self.last_lag_note_idx
            && matches!(self.items.get(idx), Some(OutputItem::SystemNote { .. }))
        {
            self.last_lag_count = self.last_lag_count.saturating_add(dropped);
            let text = format!("dropped {} stream frames", self.last_lag_count);
            self.last_lag_at = Some(now);
            self.mutate_item(idx, OutputMutation::Semantic, |item| {
                let OutputItem::SystemNote { text: current, .. } = item else {
                    return false;
                };
                if *current == text {
                    return false;
                }
                *current = text;
                true
            });
            return;
        }
        self.push_item(OutputItem::SystemNote {
            text: format!("dropped {dropped} stream frames"),
            level: NoteLevel::Warn,
        });
        self.last_lag_count = dropped;
        self.last_lag_note_idx = Some(self.items.len() - 1);
        self.last_lag_at = Some(now);
    }

    fn reset_lag_state(&mut self) {
        self.last_lag_note_idx = None;
        self.last_lag_count = 0;
    }

    pub fn push_user_turn(&mut self, text: String) {
        self.close_current_workflow_panel(false, None);
        self.push_item(OutputItem::UserTurn { text });
        self.waiting_for_llm = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_tool_call(id: impl Into<String>) -> ToolCallView {
        let now = Instant::now();
        ToolCallView {
            id: id.into(),
            tool: "fs.read".into(),
            intent: "Inspect project state".into(),
            input: serde_json::json!({"path": "src/lib.rs"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: now,
            ended_at: Some(now),
        }
    }

    #[test]
    fn final_answer_draft_streams_markdown_and_folds_prior_work() {
        let mut app = AppState::new("session".into(), None);
        app.push_item(OutputItem::UserTurn {
            text: "build it".into(),
        });
        app.push_item(OutputItem::Thinking {
            text: "checking the renderer".into(),
            done: false,
            disclosure: Disclosure::Summary,
            retried: false,
        });

        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: String::new(),
            name: atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL.into(),
            arguments_delta:
                "{\"_atman_intent\":\"Validated the renderer and tests.\",\"message\":\"Hel".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "answer-1".into(),
            name: atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL.into(),
            arguments_delta: "lo \\uD83D".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "answer-1".into(),
            name: String::new(),
            arguments_delta: "\\uDE00\"}".into(),
            run_id: None,
        });

        assert_eq!(app.work_folds.len(), 1);
        assert_eq!(app.work_folds[0].title, "Validated the renderer and tests.");
        assert!(matches!(
            app.items.last(),
            Some(OutputItem::AssistantMd { md, streaming: true, .. }) if md == "Hello 😀"
        ));
        assert!(!app.items.iter().any(|item| matches!(
            item,
            OutputItem::ToolDispatch { calls }
                if calls.iter().any(|call| call.tool == atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL)
        )));

        let duration = app.work_folds[0].duration;
        app.work_folds[0].started_at = Instant::now().checked_sub(duration).unwrap();
        assert_eq!(app.work_fold_projections()[0].visible_members, 0);
        assert!(app.toggle_latest_work_fold());
        app.work_folds[0].started_at = Instant::now().checked_sub(duration).unwrap();
        assert_eq!(app.work_fold_projections()[0].visible_members, 1);
    }

    #[test]
    fn restored_final_answer_defaults_its_work_section_to_collapsed() {
        let app = AppState::new("session".into(), None).with_initial_items(vec![
            OutputItem::UserTurn {
                text: "build it".into(),
            },
            OutputItem::Thinking {
                text: "restored reasoning".into(),
                done: true,
                disclosure: Disclosure::Summary,
                retried: false,
            },
            OutputItem::WorkFoldMarker {
                summary: Some("Checked the restored work.".into()),
                start_index: None,
            },
            OutputItem::AssistantMd {
                md: "Done.".into(),
                streaming: false,
                retried: false,
            },
        ]);

        let projections = app.work_fold_projections();
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].visible_members, 0);
        assert!(!projections[0].expanded);
        assert_eq!(projections[0].title, "Checked the restored work.");
    }

    #[test]
    fn live_and_replayed_work_folds_use_completed_tool_progress() {
        let mut live = AppState::new("live".into(), None);
        live.push_item(OutputItem::UserTurn {
            text: "build it".into(),
        });
        live.push_item(OutputItem::Thinking {
            text: "working".into(),
            done: true,
            disclosure: Disclosure::Summary,
            retried: false,
        });
        live.push_item(OutputItem::ToolDispatch {
            calls: (0..4)
                .map(|index| completed_tool_call(format!("call-{index}")))
                .collect(),
        });
        // Internal agent-loop helpers also emit ToolNode frames, but they are
        // not visible members of the folded work section.
        live.turn_activity.attempted_calls = 69;
        live.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "answer".into(),
            name: atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL.into(),
            arguments_delta:
                "{\"_atman_intent\":\"Completed the requested changes.\",\"message\":\"Done.\"}"
                    .into(),
            run_id: None,
        });
        let live_fold = live.work_fold_projections().remove(0);

        let mut restored_items = vec![OutputItem::UserTurn {
            text: "build it".into(),
        }];
        restored_items.push(OutputItem::ToolDispatch {
            calls: (0..4)
                .map(|index| completed_tool_call(format!("call-{index}")))
                .collect(),
        });
        restored_items.push(OutputItem::WorkFoldMarker {
            summary: Some("Completed the requested changes.".into()),
            start_index: None,
        });
        restored_items.push(OutputItem::AssistantMd {
            md: "Done.".into(),
            streaming: false,
            retried: false,
        });
        let replay = AppState::new("replay".into(), None).with_initial_items(restored_items);
        let replay_fold = replay.work_fold_projections().remove(0);

        assert_eq!(live_fold.completed_steps, 4);
        assert_eq!(live_fold.total_steps, 4);
        assert_eq!(replay_fold.completed_steps, 4);
        assert_eq!(replay_fold.total_steps, 4);
        assert_eq!(live_fold.title, replay_fold.title);
    }

    #[test]
    fn retry_discards_only_the_uncommitted_final_answer_fold() {
        let mut app = AppState::new("session".into(), None).with_initial_items(vec![
            OutputItem::UserTurn {
                text: "older turn".into(),
            },
            OutputItem::Thinking {
                text: "older work".into(),
                done: true,
                disclosure: Disclosure::Summary,
                retried: false,
            },
            OutputItem::WorkFoldMarker {
                summary: None,
                start_index: None,
            },
            OutputItem::AssistantMd {
                md: "Older answer".into(),
                streaming: false,
                retried: false,
            },
        ]);
        app.push_item(OutputItem::UserTurn {
            text: "new turn".into(),
        });
        app.push_item(OutputItem::Thinking {
            text: "new work".into(),
            done: false,
            disclosure: Disclosure::Summary,
            retried: false,
        });
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "answer".into(),
            name: atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL.into(),
            arguments_delta: "{\"message\":\"partial\"}".into(),
            run_id: None,
        });
        assert_eq!(app.work_folds.len(), 2);

        app.apply_stream_frame(StreamFrame::LlmRetry);

        assert_eq!(app.work_folds.len(), 1);
        assert!(app.final_answer_drafts.is_empty());
        assert!(matches!(
            app.items.last(),
            Some(OutputItem::AssistantMd {
                retried: true,
                streaming: false,
                ..
            })
        ));
    }

    #[test]
    fn next_llm_output_discards_a_rejected_final_answer_candidate() {
        let mut app = AppState::new("session".into(), None);
        app.push_item(OutputItem::UserTurn {
            text: "finish it".into(),
        });
        app.push_item(OutputItem::Thinking {
            text: "work".into(),
            done: false,
            disclosure: Disclosure::Summary,
            retried: false,
        });
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "answer".into(),
            name: atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL.into(),
            arguments_delta: "{\"_atman_intent\":\"Finished the work.\",\"message\":\"premature\"}"
                .into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 1,
            run_id: None,
        });
        assert!(app.final_answer_drafts_completed);

        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "more work".into(),
            run_id: None,
        });

        assert!(app.final_answer_drafts.is_empty());
        assert!(!app.final_answer_drafts_completed);
        assert!(app.work_folds.is_empty());
        assert!(
            !app.items.iter().any(
                |item| matches!(item, OutputItem::AssistantMd { md, .. } if md == "premature")
            )
        );
        assert!(matches!(
            app.items.last(),
            Some(OutputItem::Thinking { text, .. }) if text.ends_with("more work")
        ));
    }

    #[test]
    fn accepted_final_answer_commits_the_streamed_candidate_fold() {
        let mut app = AppState::new("session".into(), None);
        app.push_item(OutputItem::UserTurn {
            text: "finish it".into(),
        });
        app.push_item(OutputItem::Thinking {
            text: "work".into(),
            done: false,
            disclosure: Disclosure::Summary,
            retried: false,
        });
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "answer".into(),
            name: atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL.into(),
            arguments_delta: "{\"_atman_intent\":\"Finished the work.\",\"message\":\"Done.\"}"
                .into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 1,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::AssistantMsg {
            flow_run_id: None,
            message: Message {
                role: atman_runtime::message::MessageRole::Assistant,
                parts: vec![
                    atman_runtime::message::MessagePart::FinalAnswerSummary {
                        text: "Finished the work.".into(),
                    },
                    atman_runtime::message::MessagePart::Text {
                        text: "Done.".into(),
                    },
                ],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: atman_runtime::message::MessageOrigin::FinalAnswer,
            },
        });

        assert!(app.final_answer_drafts.is_empty());
        assert!(!app.final_answer_drafts_completed);
        assert_eq!(app.work_folds.len(), 1);
        assert_eq!(app.work_folds[0].title, "Finished the work.");
        assert!(matches!(
            app.items.last(),
            Some(OutputItem::AssistantMd { md, streaming: false, .. }) if md == "Done."
        ));
    }

    struct ModelConfigReset;

    impl Drop for ModelConfigReset {
        fn drop(&mut self) {
            atman_runtime::model_registry::set_provider_config(
                atman_runtime::model_registry::ProviderConfig::default(),
            );
            atman_runtime::model_registry::remove_provider_catalog("test-codex");
            atman_runtime::model_registry::remove_provider_catalog("test-compatible");
        }
    }

    #[test]
    fn mcp_content_revision_tracks_only_canonical_changes() {
        use atman_runtime::mcp::{
            McpPrompt, McpResource, McpServerState, McpServerStatus, TransportKind,
        };

        let mut app = AppState::new("session".into(), None);
        let mut context = atman_runtime::ContextSnapshot::default();
        context.mcp_servers.push(McpServerStatus {
            name: "server".into(),
            transport: TransportKind::Stdio,
            state: McpServerState::Pending,
        });
        app.replace_context_snapshot(context.clone());
        assert_eq!(app.mcp_content_revision, 1);
        app.replace_context_snapshot(context.clone());
        assert_eq!(app.mcp_content_revision, 1);

        context.tokens_in = 42;
        app.replace_context_snapshot(context.clone());
        assert_eq!(app.mcp_content_revision, 1);
        context.mcp_servers[0].state = McpServerState::Connecting;
        app.replace_context_snapshot(context);
        assert_eq!(app.mcp_content_revision, 2);

        let resources = vec![McpResource {
            uri: "resource://one".into(),
            name: "one".into(),
            description: Some("description".into()),
            mime_type: Some("text/plain".into()),
        }];
        app.replace_mcp_resources("server".into(), resources.clone());
        assert_eq!(app.mcp_content_revision, 3);
        app.replace_mcp_resources("server".into(), resources);
        assert_eq!(app.mcp_content_revision, 3);

        let prompts = vec![McpPrompt {
            name: "prompt".into(),
            description: Some("description".into()),
            arguments: Vec::new(),
        }];
        app.replace_mcp_prompts("server".into(), prompts.clone());
        assert_eq!(app.mcp_content_revision, 4);
        app.replace_mcp_prompts("server".into(), prompts);
        assert_eq!(app.mcp_content_revision, 4);
    }

    #[test]
    fn smart_alias_preserves_unknown_effort_and_resets_known_unsupported_effort() {
        use atman_runtime::model_registry::{AliasEntry, ProviderConfig};
        use atman_runtime::provider::{
            CapabilityKnowledge, DiscoveredModelDetails, ReasoningEffort, ReasoningSelection,
            ReasoningWireProfile,
        };

        let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap();
        let _reset = ModelConfigReset;
        let install_legacy_catalog = |provider_key: &str, wire_profile| {
            let prepared = atman_runtime::model_registry::prepare_provider_catalog(
                atman_runtime::model_registry::ProviderDescriptor {
                    provider_key: provider_key.into(),
                    provider_name: provider_key.into(),
                    namespace: provider_key.into(),
                    wire_profile,
                },
                &[DiscoveredModelDetails {
                    slug: "legacy-model".into(),
                    context_budget: Some(128_000),
                    capability_knowledge: CapabilityKnowledge::Legacy { thinking: true },
                }],
            )
            .unwrap();
            atman_runtime::model_registry::commit_prepared_provider_catalog(prepared);
        };
        atman_runtime::model_registry::set_provider_config(ProviderConfig {
            aliases: std::collections::HashMap::from([(
                "smart".into(),
                AliasEntry {
                    model: "test-codex:legacy-model".into(),
                },
            )]),
            ..Default::default()
        });
        install_legacy_catalog("test-codex", ReasoningWireProfile::CodexResponses);

        let mut app = AppState::new("session".into(), None);
        app.input_reasoning = Some(ReasoningSelection::Effort {
            effort: ReasoningEffort::High,
            execution_mode: None,
        });

        assert!(!app.reconcile_input_reasoning());
        assert_eq!(
            app.input_reasoning,
            Some(ReasoningSelection::Effort {
                effort: ReasoningEffort::High,
                execution_mode: None,
            })
        );
        assert!(app.items.is_empty());
        assert_eq!(
            app.input_reasoning_for_submission(),
            Some(ReasoningSelection::Effort {
                effort: ReasoningEffort::High,
                execution_mode: None,
            })
        );

        atman_runtime::model_registry::remove_provider_catalog("test-codex");
        atman_runtime::model_registry::set_provider_config(ProviderConfig {
            aliases: std::collections::HashMap::from([(
                "smart".into(),
                AliasEntry {
                    model: "test-compatible:legacy-model".into(),
                },
            )]),
            ..Default::default()
        });
        install_legacy_catalog("test-compatible", ReasoningWireProfile::CompatibleThinking);

        assert!(app.reconcile_input_reasoning());
        assert_eq!(app.input_reasoning, None);
        assert!(matches!(
            app.items.last(),
            Some(OutputItem::SystemNote {
                level: NoteLevel::Warn,
                ..
            })
        ));
    }

    #[test]
    fn inline_replace_notification_updates_the_existing_note() {
        let mut app = AppState {
            streaming: true,
            waiting_for_llm: true,
            ..Default::default()
        };
        let frame = |level, message: &str| {
            StreamFrame::Notification(atman_runtime::stream::NotificationFrame {
                level,
                location: atman_runtime::notify::NotifyLocation::Inline,
                lifecycle: atman_runtime::notify::NotifyLifecycle::UntilReplaced,
                stack: atman_runtime::notify::NotifyStack::Replace {
                    key: "llm-call:run:node".into(),
                },
                message: message.into(),
            })
        };

        app.apply_stream_frame(frame(atman_runtime::notify::NotifyLevel::Warn, "retrying"));
        app.apply_stream_frame(frame(atman_runtime::notify::NotifyLevel::Error, "failed"));

        assert_eq!(app.items.len(), 1);
        assert!(!app.streaming);
        assert!(!app.waiting_for_llm);
        assert!(matches!(
            &app.items[0],
            OutputItem::SystemNote {
                text,
                level: NoteLevel::Error
            } if text == "failed"
        ));
    }

    #[test]
    fn s8_permission_frames_route_to_root_and_spawned_workflow_owners() {
        use atman_runtime::event::FlowRunId;
        use atman_runtime::permission::PermissionRequestId;
        use atman_runtime::permission_audit::{
            PermissionAuditTarget, PermissionPolicyReference, PermissionProvenanceSummary,
            PermissionRequestAudit,
        };
        use atman_runtime::tool::Tier;
        use chrono::Utc;

        fn payload(run_id: &str, tool_use_id: &str) -> PermissionRequestAudit {
            let run_id = FlowRunId(uuid::Uuid::parse_str(run_id).unwrap());
            PermissionRequestAudit {
                request_id: Some(PermissionRequestId::now()),
                revision: 1,
                session_id: "session".into(),
                requesting_run_id: run_id.clone(),
                parent_run_id: None,
                root_run_id: run_id,
                tool_use_id: tool_use_id.into(),
                tool: "fs.read".into(),
                call_intent: None,
                tier: Tier::Two,
                execution_boundary: Default::default(),
                provenance: PermissionProvenanceSummary::default(),
                target: PermissionAuditTarget::User,
                group_ids: Vec::new(),
                policy: PermissionPolicyReference {
                    snapshot_id: "snapshot".into(),
                    rule_id: "rule".into(),
                },
                escalation_path: Vec::new(),
                decision_id: None,
                actor: None,
                scope: None,
                reason: None,
                at: Utc::now(),
            }
        }

        let root = uuid::Uuid::now_v7().to_string();
        let spawned = uuid::Uuid::now_v7().to_string();
        let descendant = uuid::Uuid::now_v7().to_string();
        let mut app = AppState::new("session".into(), None);
        app.apply_stream_frame(StreamFrame::FlowStart {
            run_id: root.clone(),
            flow_name: "root".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::SubAgentStarted {
            handle: "agent-1".into(),
            goal: "research".into(),
            child_run_id: spawned.clone(),
            model: "model".into(),
            tool_use_id: None,
        });
        app.apply_stream_frame(StreamFrame::FlowStart {
            run_id: descendant.clone(),
            flow_name: "research_loop".into(),
            parent_run_id: Some(spawned),
            parent_node_id: None,
        });
        let root_payload = payload(&root, "root-tool");
        let child_payload = payload(&descendant, "child-tool");
        let mut automatic = payload(&root, "automatic-tool");
        automatic.decision_id = Some("policy-decision".into());
        app.apply_stream_frame(StreamFrame::PermissionRequestCreated {
            run_id: root.clone(),
            payload: automatic,
        });
        assert!(app.pending_permissions.is_empty());
        app.apply_stream_frame(StreamFrame::PermissionRequestCreated {
            run_id: root,
            payload: root_payload.clone(),
        });
        app.apply_stream_frame(StreamFrame::PermissionRequestCreated {
            run_id: descendant,
            payload: child_payload.clone(),
        });

        let root_graph = app
            .items
            .iter()
            .find_map(|item| match item {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .unwrap();
        let child_graph = app
            .items
            .iter()
            .find_map(|item| match item {
                OutputItem::SubAgentActivity { workflow_graph, .. } => Some(workflow_graph),
                _ => None,
            })
            .unwrap();
        assert_eq!(root_graph.permission_requests.len(), 1);
        assert_eq!(child_graph.permission_requests.len(), 1);
        assert!(root_graph.permission_requests.values().any(|request| {
            request.payload == root_payload && request.payload.policy.rule_id == "rule"
        }));
        assert!(child_graph.permission_requests.values().any(|request| {
            request.payload == child_payload
                && request.payload.provenance == PermissionProvenanceSummary::default()
        }));
        assert!(
            root_graph
                .permission_requests
                .values()
                .all(|request| { request.payload.tool_use_id != "automatic-tool" })
        );
        assert!(
            root_graph
                .permission_requests
                .values()
                .all(|request| { request.payload.tool_use_id != "child-tool" })
        );
        assert!(
            child_graph
                .permission_requests
                .values()
                .all(|request| { request.payload.tool_use_id != "root-tool" })
        );
        assert_eq!(app.pending_permissions.len(), 2);
        let root_id = root_payload.request_id.clone().unwrap();
        assert_eq!(app.pending_permissions[&root_id].revision, 1);

        let mut approved = root_payload;
        approved.revision = 2;
        app.apply_stream_frame(StreamFrame::PermissionRequestApproved {
            run_id: approved.requesting_run_id.to_string(),
            payload: approved,
        });
        assert!(!app.pending_permissions.contains_key(&root_id));
        assert_eq!(app.pending_permissions.len(), 1);

        let child_id = child_payload.request_id.clone().unwrap();
        let mut unrestricted = child_payload;
        unrestricted.revision = 2;
        app.apply_stream_frame(StreamFrame::UnrestrictedExecution {
            run_id: unrestricted.requesting_run_id.to_string(),
            payload: unrestricted,
        });
        assert!(!app.pending_permissions.contains_key(&child_id));
        assert!(app.pending_permissions.is_empty());
    }

    #[test]
    fn toggle_mouse_capture_flips_state() {
        let mut app = AppState::new("s".into(), None);
        assert!(app.mouse_captured, "default is captured");
        let now_on = app.toggle_mouse_capture();
        assert!(!now_on);
        assert!(!app.mouse_captured);
        let now_on2 = app.toggle_mouse_capture();
        assert!(now_on2);
        assert!(app.mouse_captured);
    }

    #[test]
    fn chunks_stream_incrementally_into_single_markdown_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hello ".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "world".into(),
            model: "m".into(),
            run_id: None,
        });
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming, .. } => {
                assert_eq!(md, "hello world");
                assert!(*streaming);
            }
            _ => panic!("expected streaming assistant md"),
        }
        assert!(app.streaming);
    }

    #[test]
    fn tool_dispatch_reconciles_drafts_and_routes_output_by_identity() {
        use atman_runtime::message::{MessageOrigin, MessagePart, MessageRole, ToolCallIntent};

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "write".into(),
            name: "fs.write".into(),
            arguments_delta: r#"{"content":"alpha\nbe"#.into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "write".into(),
            name: "fs.write".into(),
            arguments_delta: r#"ta"}"#.into(),
            run_id: None,
        });
        let assistant = Message {
            role: MessageRole::Assistant,
            parts: vec![
                MessagePart::ToolUse {
                    id: "write".into(),
                    name: "fs.write".into(),
                    input: serde_json::json!({"content": "alpha\nbeta"}),
                    intent: ToolCallIntent::new("写入文件"),
                },
                MessagePart::ToolUse {
                    id: "shell".into(),
                    name: "bash.spawn".into(),
                    input: serde_json::json!({"cmd": "printf ok"}),
                    intent: ToolCallIntent::new("检查结果"),
                },
            ],
            turn_id: atman_runtime::event::TurnId::now(),
            origin: MessageOrigin::User,
        };
        app.apply_stream_frame(StreamFrame::AssistantMsg {
            flow_run_id: None,
            message: assistant,
        });
        app.apply_stream_frame(StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            tool_use_id: Some("shell".into()),
            kind: "stdout".into(),
            line: "ok\n".into(),
            call_intent: ToolCallIntent::new("检查结果"),
            run_id: None,
        });

        let OutputItem::ToolDispatch { calls } = &app.items[0] else {
            panic!("expected grouped tool dispatch");
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].draft_preview.last_line(), Some("beta"));
        assert!(calls[0].detail.is_none());
        assert!(
            matches!(calls[1].detail.as_deref(), Some(OutputItem::Bash { output, .. }) if output == "ok\n")
        );
    }

    #[test]
    fn scoped_root_assistant_reconciles_canonical_tool_input() {
        use atman_runtime::message::{MessageOrigin, MessagePart, MessageRole, ToolCallIntent};

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "read".into(),
            name: "fs.read".into(),
            arguments_delta: r#"{"path":"README.md"}"#.into(),
            run_id: Some("root-flow".into()),
        });
        app.apply_stream_frame(StreamFrame::AssistantMsg {
            flow_run_id: Some("root-flow".into()),
            message: Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: "read".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path": "README.md", "offset": 1, "limit": 80}),
                    intent: ToolCallIntent::new("读取项目说明"),
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        let OutputItem::ToolDispatch { calls } = &app.items[0] else {
            panic!("expected grouped tool dispatch");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["path"], "README.md");
        assert_eq!(
            calls[0].draft_preview.arguments(),
            r#"{"path":"README.md"}"#
        );
    }

    #[test]
    fn tool_draft_probe_trims_only_at_utf8_boundaries() {
        let mut preview = ToolDraftPreview::default();
        preview.push("fs.write", &"界".repeat(64));
        preview.push("fs.write", r#"{"content":"完成"}"#);
        assert_eq!(preview.last_line(), Some("完成"));
    }

    #[test]
    fn scoped_tool_result_completes_matching_document_flow_call() {
        use atman_runtime::message::{MessageOrigin, MessagePart, MessageRole, ToolCallIntent};

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::AssistantMsg {
            flow_run_id: None,
            message: Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: "read-1".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path": "README.md"}),
                    intent: ToolCallIntent::new("读取项目说明"),
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        app.apply_stream_frame(StreamFrame::ToolResultMsg {
            flow_run_id: Some("root-flow".into()),
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "read-1".into(),
                    content: "done".into(),
                    is_error: false,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        let OutputItem::ToolDispatch { calls } = &app.items[0] else {
            panic!("expected grouped tool dispatch");
        };
        assert_eq!(calls[0].status, ToolCallStatus::Ok);
        assert!(calls[0].ended_at.is_some());
    }

    #[test]
    fn late_spawn_results_do_not_revive_exited_tool_calls() {
        use atman_runtime::message::{MessageOrigin, MessagePart, MessageRole, ToolCallIntent};

        let run = |tool: &str, tool_use_id: &str, result: &str, terminal: bool| {
            let mut app = AppState::new("s".into(), None);
            app.apply_stream_frame(StreamFrame::AssistantMsg {
                flow_run_id: None,
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: tool.into(),
                        input: serde_json::json!({"cmd": "true"}),
                        intent: ToolCallIntent::new("运行检查"),
                    }],
                    turn_id: atman_runtime::event::TurnId::now(),
                    origin: MessageOrigin::User,
                },
            });
            if terminal {
                app.apply_stream_frame(StreamFrame::TerminalExited {
                    handle: "term_s_1".into(),
                    tool_use_id: Some(tool_use_id.into()),
                    exit_code: Some(0),
                    call_intent: None,
                    run_id: None,
                });
            } else {
                app.apply_stream_frame(StreamFrame::BashExited {
                    handle: "bg_s_1".into(),
                    tool_use_id: Some(tool_use_id.into()),
                    exit_code: Some(0),
                    error: None,
                    call_intent: None,
                    run_id: None,
                });
            }
            let ended_at = match &app.items[0] {
                OutputItem::ToolDispatch { calls } => calls[0].ended_at,
                _ => panic!("expected grouped tool dispatch"),
            };
            app.apply_stream_frame(StreamFrame::ToolResultMsg {
                flow_run_id: None,
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content: result.into(),
                        is_error: false,
                    }],
                    turn_id: atman_runtime::event::TurnId::now(),
                    origin: MessageOrigin::User,
                },
            });
            let OutputItem::ToolDispatch { calls } = &app.items[0] else {
                panic!("expected grouped tool dispatch");
            };
            assert_eq!(calls[0].status, ToolCallStatus::Ok);
            assert_eq!(calls[0].ended_at, ended_at);
            assert!(matches!(
                calls[0].detail.as_deref(),
                Some(OutputItem::Bash { done: true, .. })
                    | Some(OutputItem::Terminal { done: true, .. })
            ));
        };

        run(
            "bash.spawn",
            "bash-1",
            r#"{"handle":"bg_s_1","status":"running","output":""}"#,
            false,
        );
        run(
            "term.spawn",
            "term-1",
            r#"{"handle":"term_s_1","state":{"kind":"running"},"rows":24,"cols":80,"text":""}"#,
            true,
        );
    }

    #[test]
    fn completed_spawn_result_never_enters_running_state() {
        use atman_runtime::message::{MessageOrigin, MessagePart, MessageRole};

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::AssistantMsg {
            flow_run_id: None,
            message: Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: "bash-1".into(),
                    name: "bash.spawn".into(),
                    input: serde_json::json!({"cmd": "true", "block": true}),
                    intent: None,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });
        app.apply_stream_frame(StreamFrame::ToolResultMsg {
            flow_run_id: None,
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "bash-1".into(),
                    content: r#"{"handle":"bg_s_1","status":"exited","exit_code":0,"output":""}"#
                        .into(),
                    is_error: false,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        let OutputItem::ToolDispatch { calls } = &app.items[0] else {
            panic!("expected grouped tool dispatch");
        };
        assert_eq!(calls[0].status, ToolCallStatus::Ok);
        assert!(calls[0].ended_at.is_some());
        assert!(matches!(
            calls[0].detail.as_deref(),
            Some(OutputItem::Bash { done: true, .. })
        ));
    }

    #[test]
    fn sub_agent_completion_finishes_its_dispatch_row() {
        use atman_runtime::message::{MessageOrigin, MessagePart, MessageRole};

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::AssistantMsg {
            flow_run_id: None,
            message: Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: "flow-1".into(),
                    name: "flow.spawn".into(),
                    input: serde_json::json!({"goal": "检查实现"}),
                    intent: None,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });
        app.apply_stream_frame(StreamFrame::SubAgentStarted {
            handle: "agent_1".into(),
            goal: "检查实现".into(),
            child_run_id: "child".into(),
            model: "model".into(),
            tool_use_id: Some("flow-1".into()),
        });
        app.apply_stream_frame(StreamFrame::ToolResultMsg {
            flow_run_id: None,
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "flow-1".into(),
                    content: r#"{"handle":"agent_1","status":"running"}"#.into(),
                    is_error: false,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });
        app.apply_stream_frame(StreamFrame::SubAgentDone {
            handle: "agent_1".into(),
            status: "ok".into(),
            final_text: "完成".into(),
        });

        let OutputItem::ToolDispatch { calls } = &app.items[0] else {
            panic!("expected grouped tool dispatch");
        };
        assert_eq!(calls[0].status, ToolCallStatus::Ok);
        assert!(calls[0].ended_at.is_some());
        assert!(matches!(
            calls[0].detail.as_deref(),
            Some(OutputItem::SubAgentActivity { done: true, .. })
        ));
    }

    #[test]
    fn sub_agent_uses_first_observed_llm_model_when_start_model_is_empty() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::SubAgentStarted {
            handle: "agent_1".into(),
            goal: "research".into(),
            child_run_id: "spawned".into(),
            model: String::new(),
            tool_use_id: None,
        });
        app.apply_stream_frame(StreamFrame::FlowStart {
            run_id: "worker".into(),
            flow_name: "research_loop".into(),
            parent_run_id: Some("spawned".into()),
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "working".into(),
            model: "primary-model".into(),
            run_id: Some("worker".into()),
        });
        app.apply_stream_frame(StreamFrame::LlmCallStats {
            model: "classifier-model".into(),
            provider: String::new(),
            context_call_purpose: Default::default(),
            context_call_scope: Default::default(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read: 0,
            cache_write: 0,
            ttft_ms: 0,
            tokens_per_second: 0.0,
            wallclock_ms: 0,
            run_id: Some("worker".into()),
            node_id: None,
        });

        assert!(matches!(
            &app.items[0],
            OutputItem::SubAgentActivity { model, output, .. }
                if model == "primary-model" && output == "working"
        ));
    }

    #[test]
    fn llm_done_flips_streaming_flag_without_duplicating_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hi".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 3,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1, "no extra markdown item after done");
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming, .. } => {
                assert_eq!(md, "hi");
                assert!(!streaming);
            }
            _ => panic!(),
        }
        assert!(!app.streaming);
    }

    #[test]
    fn llm_done_finalizes_thinking_without_text_chunks() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "hmm".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 5,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::Thinking { done, text, .. } => {
                assert!(*done, "thinking must be finalized by LlmDone");
                assert_eq!(text, "hmm");
            }
            _ => panic!("expected Thinking item"),
        }
    }

    #[test]
    fn llm_done_finalizes_thinking_then_tool_use_no_text() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "thinking...".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "tc1".into(),
            name: "fs.read".into(),
            arguments_delta: "{\"path\":\"x\"}".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 5,
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::Thinking { done, .. } => assert!(*done, "thinking stuck spinning"),
            _ => panic!("expected Thinking"),
        }
    }

    #[test]
    fn llm_done_finalizes_text_then_thinking_both_unfinalized() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "partial".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "rethink".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 9,
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::AssistantMd { streaming, md, .. } => {
                assert!(!*streaming, "AssistantMd must be finalized");
                assert_eq!(md, "partial");
            }
            _ => panic!("expected AssistantMd at items[0]"),
        }
        match &app.items[1] {
            OutputItem::Thinking { done, text, .. } => {
                assert!(*done, "Thinking must be finalized");
                assert_eq!(text, "rethink");
            }
            _ => panic!("expected Thinking at items[1]"),
        }
    }

    #[test]
    fn llm_done_stops_at_first_finalized_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "prev".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 1,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "new turn".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 2,
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming, .. } => {
                assert_eq!(md, "prev");
                assert!(!*streaming, "prev must stay finalized");
            }
            _ => panic!(),
        }
        match &app.items[1] {
            OutputItem::Thinking { done, text, .. } => {
                assert!(*done);
                assert_eq!(text, "new turn");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tool_use_stream_frames_no_longer_push_items() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ToolUseStart {
            tool: "fs.read".into(),
            args_preview: "\"foo\"".into(),
            id: "tc_1".into(),
        });
        app.apply_stream_frame(StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            ok: true,
            preview: "12 bytes".into(),
            id: "tc_1".into(),
        });
        assert!(
            app.items.is_empty(),
            "tool traffic flows through workflow panel now"
        );
    }

    #[test]
    fn activity_projection_tracks_parallel_tool_intents_and_completion() {
        use atman_runtime::event::FlowNodeStatus;
        use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::FlowNodeStart {
            run_id: "run-1".into(),
            node_id: "dispatch".into(),
            kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                path: "dispatch_all".into(),
            },
            label: "dispatch_all".into(),
            parent_node_id: None,
        });
        for (id, intent) in [("tool-a", "读取配置"), ("tool-b", "检查进程")] {
            app.apply_stream_frame(StreamFrame::ToolNode {
                run_id: "run-1".into(),
                parent_node_id: "dispatch".into(),
                tool_use_id: id.into(),
                tool: "fs.read".into(),
                args_preview: String::new(),
                call_intent: atman_runtime::message::ToolCallIntent::new(intent),
            });
        }

        let leaves = crate::task_panel::running_activity_leaves(&app.activity_nodes)
            .into_iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(leaves, ["读取配置", "检查进程"]);

        app.apply_stream_frame(StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            ok: true,
            preview: String::new(),
            id: "tool-a".into(),
        });
        let leaves = crate::task_panel::running_activity_leaves(&app.activity_nodes);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].node_id, "tool-b");

        app.apply_stream_frame(StreamFrame::ToolResultMsg {
            flow_run_id: Some("run-1".into()),
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "tool-b".into(),
                    content: "failed".into(),
                    is_error: true,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });
        let leaves = crate::task_panel::running_activity_leaves(&app.activity_nodes);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].node_id, "dispatch");
        assert_eq!(
            app.activity_nodes
                .iter()
                .find(|node| node.node_id == "tool-b")
                .unwrap()
                .status,
            crate::task_panel::ActivityStatus::Err
        );

        app.apply_stream_frame(StreamFrame::FlowNodeEnd {
            run_id: "run-1".into(),
            node_id: "dispatch".into(),
            status: FlowNodeStatus::Ok,
            output_preview: None,
            parent_node_id: None,
        });
        assert!(crate::task_panel::running_activity_leaves(&app.activity_nodes).is_empty());
    }

    #[test]
    fn llm_retry_marks_items_as_retried() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "let me think".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hello".into(),
            model: "m".into(),
            run_id: None,
        });
        assert_eq!(app.items.len(), 2);
        app.apply_stream_frame(StreamFrame::LlmRetry);
        assert_eq!(app.items.len(), 2, "LlmRetry keeps items but marks them");
        match &app.items[0] {
            OutputItem::Thinking { retried, done, .. } => {
                assert!(*retried, "Thinking should be marked retried");
                assert!(*done, "Thinking should be done");
            }
            _ => panic!("expected Thinking"),
        }
        match &app.items[1] {
            OutputItem::AssistantMd {
                retried, streaming, ..
            } => {
                assert!(*retried, "AssistantMd should be marked retried");
                assert!(!streaming, "AssistantMd should not be streaming");
            }
            _ => panic!("expected AssistantMd"),
        }
        assert!(app.waiting_for_llm);
    }

    #[test]
    fn llm_retry_preserves_non_llm_items() {
        let mut app = AppState::new("s".into(), None);
        app.push_item(OutputItem::Bash {
            handle: "h".into(),
            title: None,
            command: None,
            output: "done".into(),
            done: true,
            expanded: false,
        });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "done thinking".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 5,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "final answer".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 10,
            run_id: None,
        });
        assert_eq!(app.items.len(), 3);
        app.apply_stream_frame(StreamFrame::LlmRetry);
        assert_eq!(
            app.items.len(),
            3,
            "LlmRetry keeps all items, only marks LLM items as retried"
        );
        match &app.items[0] {
            OutputItem::Bash { .. } => {}
            _ => panic!("expected Bash item preserved"),
        }
        match &app.items[1] {
            OutputItem::Thinking { retried, .. } => assert!(*retried),
            _ => panic!("expected Thinking"),
        }
        match &app.items[2] {
            OutputItem::AssistantMd { retried, .. } => assert!(*retried),
            _ => panic!("expected AssistantMd"),
        }
    }

    #[test]
    fn user_turn_pushes_only_item() {
        let mut app = AppState::new("s".into(), None);
        app.push_user_turn("hi".into());
        assert_eq!(app.items.len(), 1);
        assert!(matches!(app.items[0], OutputItem::UserTurn { .. }));
    }

    #[test]
    fn toggle_tool_expansion_flips_membership() {
        let mut app = AppState::new("s".into(), None);
        assert!(!app.is_tool_expanded("x"));
        app.toggle_tool_expansion("x");
        assert!(app.is_tool_expanded("x"));
        app.toggle_tool_expansion("x");
        assert!(!app.is_tool_expanded("x"));
    }

    #[test]
    fn toggle_working_group_expansion_is_scoped_to_its_dispatch() {
        let now = Instant::now();
        let mut app =
            AppState::new("s".into(), None).with_initial_items(vec![OutputItem::ToolDispatch {
                calls: vec![ToolCallView {
                    id: "read-1".into(),
                    tool: "fs.read".into(),
                    intent: "inspect output".into(),
                    input: serde_json::json!({"path": "src/output.rs"}),
                    status: ToolCallStatus::Ok,
                    disclosure: Disclosure::Summary,
                    detail: None,
                    draft_index: None,
                    draft_preview: Default::default(),
                    applied_edit: None,
                    started_at: now,
                    ended_at: Some(now),
                }],
            }]);
        let key = format!("{}read-1", crate::output::WORKING_GROUP_REGION_PREFIX);

        app.toggle_working_group_expansion(0, &key);
        assert!(app.expanded_tools.contains(&key));
        app.toggle_working_group_expansion(0, &key);
        assert!(!app.expanded_tools.contains(&key));
    }

    #[test]
    fn compaction_summary_frames_mutate_same_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::CompactionSummary {
            phase: CompactionPhase::Running,
            range_start: 2,
            range_end: 8,
            summary: String::new(),
            before_tokens: 100,
            after_tokens: 0,
            compacted_count: 7,
        });
        app.apply_stream_frame(StreamFrame::CompactionSummary {
            phase: CompactionPhase::Finished,
            range_start: 2,
            range_end: 8,
            summary: "## Objective\n- keep it short".into(),
            before_tokens: 100,
            after_tokens: 40,
            compacted_count: 7,
        });

        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::CompactionSummary {
                phase,
                range_start,
                range_end,
                summary,
                before_tokens,
                after_tokens,
                compacted_count,
                ..
            } => {
                assert!(matches!(phase, CompactionPhase::Finished));
                assert_eq!(*range_start, 2);
                assert_eq!(*range_end, 8);
                assert!(summary.contains("Objective"));
                assert_eq!(*before_tokens, 100);
                assert_eq!(*after_tokens, 40);
                assert_eq!(*compacted_count, 7);
            }
            _ => panic!("expected compaction summary item"),
        }
    }

    #[test]
    fn hit_test_maps_absolute_row_to_item_index() {
        use crate::output::ItemRange;
        use ratatui::layout::Rect;
        let mut app = AppState::new("s".into(), None);
        app.last_transcript_rect = Some(Rect::new(0, 2, 80, 20));
        app.last_item_ranges = vec![
            ItemRange {
                item_index: 0,
                start_row: 0,
                end_row: 2,
            },
            ItemRange {
                item_index: 1,
                start_row: 2,
                end_row: 5,
            },
        ];
        app.scroll_offset = 0;
        assert_eq!(app.hit_test(10, 2), Some(0));
        assert_eq!(app.hit_test(10, 3), Some(0));
        assert_eq!(app.hit_test(10, 4), Some(1));
        assert_eq!(app.hit_test(10, 6), Some(1));
    }

    #[test]
    fn hit_test_node_maps_row_to_workflow_node_id() {
        use crate::output::NodeRegion;
        use ratatui::layout::Rect;
        let mut app = AppState::new("s".into(), None);
        app.last_transcript_rect = Some(Rect::new(0, 2, 80, 20));
        app.last_node_regions = vec![
            NodeRegion {
                panel_item_index: 3,
                path_key: "0".into(),
                start_row: 1,
                end_row: 2,
                col_start: 0,
                col_end: 80,
            },
            NodeRegion {
                panel_item_index: 3,
                path_key: "0/0".into(),
                start_row: 2,
                end_row: 3,
                col_start: 0,
                col_end: 80,
            },
        ];
        app.scroll_offset = 0;
        assert_eq!(app.hit_test_node(10, 3), Some((3, "0".to_string())));
        assert_eq!(app.hit_test_node(10, 4), Some((3, "0/0".to_string())));
        assert_eq!(app.hit_test_node(10, 5), None);
    }

    #[test]
    fn hit_test_node_prioritizes_fullscreen_and_keeps_tool_detail_clickable() {
        use crate::output::{
            NodeRegion, TOOL_CALL_REGION_PREFIX, TOOL_DETAIL_FULLSCREEN_REGION_PREFIX,
            TOOL_DETAIL_REGION_PREFIX, TOOL_FULLSCREEN_REGION_PREFIX,
        };
        use ratatui::layout::Rect;

        let mut app = AppState::new("s".into(), None);
        app.last_transcript_rect = Some(Rect::new(0, 2, 80, 20));
        app.last_node_regions = vec![
            NodeRegion {
                panel_item_index: 4,
                path_key: format!("{TOOL_CALL_REGION_PREFIX}edit-1"),
                start_row: 1,
                end_row: 4,
                col_start: 0,
                col_end: 80,
            },
            NodeRegion {
                panel_item_index: 4,
                path_key: format!("{TOOL_DETAIL_REGION_PREFIX}edit-1"),
                start_row: 4,
                end_row: 8,
                col_start: 0,
                col_end: 80,
            },
            NodeRegion {
                panel_item_index: 4,
                path_key: format!("{TOOL_FULLSCREEN_REGION_PREFIX}edit-1"),
                start_row: 1,
                end_row: 2,
                col_start: 76,
                col_end: 78,
            },
            NodeRegion {
                panel_item_index: 4,
                path_key: format!("{TOOL_DETAIL_FULLSCREEN_REGION_PREFIX}edit-1"),
                start_row: 4,
                end_row: 5,
                col_start: 74,
                col_end: 78,
            },
        ];

        assert_eq!(
            app.hit_test_node(77, 3),
            Some((4, format!("{TOOL_FULLSCREEN_REGION_PREFIX}edit-1")))
        );
        assert_eq!(
            app.hit_test_node(10, 6),
            Some((4, format!("{TOOL_DETAIL_REGION_PREFIX}edit-1")))
        );
        assert_eq!(
            app.hit_test_node(77, 6),
            Some((4, format!("{TOOL_DETAIL_FULLSCREEN_REGION_PREFIX}edit-1")))
        );
    }

    #[test]
    fn hit_test_returns_none_outside_transcript() {
        use crate::output::ItemRange;
        use ratatui::layout::Rect;
        let mut app = AppState::new("s".into(), None);
        app.last_transcript_rect = Some(Rect::new(5, 2, 80, 20));
        app.last_item_ranges = vec![ItemRange {
            item_index: 0,
            start_row: 0,
            end_row: 2,
        }];
        assert_eq!(app.hit_test(10, 1), None, "row above rect");
        assert_eq!(app.hit_test(10, 30), None, "row below rect");
        assert_eq!(app.hit_test(2, 3), None, "col left of rect");
        assert_eq!(app.hit_test(200, 3), None, "col right of rect");
    }

    #[test]
    fn resolve_scroll_follows_tail_by_default() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 20, 0, 5);
        assert_eq!(app.scroll_offset, 82);
        assert!(app.follow_tail);
    }

    #[test]
    fn anchor_with_input_overlay() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        assert_eq!(app.scroll_offset, 77);
        assert_eq!(app.max_scroll_offset(), 77);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_up_goes_toward_older_content() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_up(20);
        assert_eq!(app.scroll_offset, 57);
        assert!(!app.follow_tail);
    }

    #[test]
    fn scroll_down_reaches_anchor_and_reenables_tail() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_up(30);
        assert_eq!(app.scroll_offset, 47);
        app.scroll_down(30);
        assert_eq!(app.scroll_offset, 77);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_down_past_anchor_clamped_to_anchor() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_down(30);
        assert_eq!(app.scroll_offset, 77);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_up_from_older_content_preserves_offset() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(200, 30, 5, 5);
        app.scroll_up(50);
        assert_eq!(app.scroll_offset, 127);
        app.resolve_scroll(300, 30, 5, 5);
        assert_eq!(app.scroll_offset, 127);
    }

    #[test]
    fn pending_below_rows_with_overlay() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_up(20);
        assert_eq!(app.pending_below_rows(), 20);
        app.scroll_to_tail();
        app.resolve_scroll(100, 30, 5, 5);
        assert_eq!(app.pending_below_rows(), 0);
    }

    #[test]
    fn max_scroll_offset_zero_when_content_shorter_than_viewport() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(10, 30, 5, 5);
        assert_eq!(app.max_scroll_offset(), 0);
    }

    #[test]
    fn record_lag_within_cooldown_merges_into_last_note() {
        let mut app = AppState::new("s".into(), None);
        let t0 = Instant::now();
        app.record_lag(5, t0);
        app.record_lag(10, t0 + Duration::from_millis(100));
        app.record_lag(20, t0 + Duration::from_millis(200));
        let notes: Vec<_> = app
            .items
            .iter()
            .filter_map(|i| match i {
                OutputItem::SystemNote { text, level } => Some((text.clone(), *level)),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].0, "dropped 35 stream frames");
        assert_eq!(notes[0].1, NoteLevel::Warn);
    }

    #[test]
    fn record_lag_after_cooldown_starts_new_note() {
        let mut app = AppState::new("s".into(), None);
        let t0 = Instant::now();
        app.record_lag(5, t0);
        app.record_lag(7, t0 + Duration::from_millis(400));
        let lag_notes: Vec<_> = app
            .items
            .iter()
            .filter_map(|i| match i {
                OutputItem::SystemNote { text, .. } if text.starts_with("dropped ") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            lag_notes,
            vec!["dropped 5 stream frames", "dropped 7 stream frames"]
        );
    }

    #[test]
    fn record_lag_state_resets_when_new_stream_frame_arrives() {
        let mut app = AppState::new("s".into(), None);
        let t0 = Instant::now();
        app.record_lag(5, t0);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hi".into(),
            model: "m".into(),
            run_id: None,
        });
        app.record_lag(3, t0 + Duration::from_millis(50));
        let lag_texts: Vec<_> = app
            .items
            .iter()
            .filter_map(|i| match i {
                OutputItem::SystemNote { text, .. } if text.starts_with("dropped ") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            lag_texts,
            vec!["dropped 5 stream frames", "dropped 3 stream frames"]
        );
    }

    #[test]
    fn flow_start_populates_workflow_panel_with_root() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("look_into", "r1"));
        let graph = atman_runtime::nodegraph::FlowGraph {
            flow_name: "look_into".into(),
            root: Vec::new(),
        };
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph,
        });
        let panel = app
            .items
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .expect("workflow panel present");
        assert_eq!(panel.root.len(), 1);
        assert_eq!(panel.root[0].label, "look_into");
    }

    #[test]
    fn toggle_workflow_node_flips_expanded_membership() {
        let mut app = AppState::new("s".into(), None);
        app.push_item(OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: WorkflowProjection::new(atman_runtime::event::TurnId::now()),
            expanded_nodes: HashSet::new(),
            panel_expanded: true,
            started_at: Instant::now(),
            ended_at: None,
            cancelled: false,
        });
        let idx = app.items.len() - 1;
        app.toggle_workflow_node(idx, "node_x");
        if let OutputItem::WorkflowPanel { expanded_nodes, .. } = &app.items[idx] {
            assert!(expanded_nodes.contains("node_x"));
        }
        app.toggle_workflow_node(idx, "node_x");
        if let OutputItem::WorkflowPanel { expanded_nodes, .. } = &app.items[idx] {
            assert!(!expanded_nodes.contains("node_x"));
        }
    }

    #[test]
    fn output_store_revisions_track_exact_mutation_domains() {
        let mut store = OutputStore::default();
        store.push(OutputItem::SystemNote {
            text: "abcdef".into(),
            level: NoteLevel::Info,
        });
        let initial = store.revisions()[0];

        assert!(store.mutate(0, OutputMutation::Semantic, |item| {
            let OutputItem::SystemNote { text, .. } = item else {
                return false;
            };
            text.replace_range(2..4, "ZZ");
            true
        }));
        let semantic = store.revisions()[0];
        assert_eq!(semantic.id, initial.id);
        assert_ne!(semantic.semantic, initial.semantic);
        assert_ne!(semantic.layout, initial.layout);
        assert_eq!(semantic.interaction, initial.interaction);
        assert_eq!(semantic.paint, initial.paint);

        assert!(store.mutate(0, OutputMutation::Paint, |_| true));
        let paint = store.revisions()[0];
        assert_ne!(paint.interaction, semantic.interaction);
        assert_ne!(paint.paint, semantic.paint);
        assert_eq!(paint.layout, semantic.layout);

        assert!(!store.mutate(0, OutputMutation::Semantic, |_| false));
        assert_eq!(store.revisions()[0], paint);
    }

    #[test]
    fn initial_items_build_the_shared_handle_index() {
        let app = AppState::new("session".into(), None).with_initial_items(vec![
            OutputItem::Bash {
                handle: "bash-1".into(),
                title: None,
                command: None,
                output: String::new(),
                done: true,
                expanded: false,
            },
            OutputItem::SubAgentActivity {
                handle: "flow-1".into(),
                goal: String::new(),
                child_run_id: String::new(),
                model: String::new(),
                status: "ok".into(),
                output: String::new(),
                iteration: 1,
                done: true,
                expanded: false,
                messages: Vec::new(),
                workflow_graph: WorkflowProjection::new(atman_runtime::event::TurnId::now()),
                expanded_nodes: HashSet::new(),
                workflow_expanded: false,
            },
        ]);

        assert_eq!(app.handle_index.get("bash-1"), Some(&0));
        assert_eq!(app.handle_index.get("flow-1"), Some(&1));
    }

    #[test]
    fn initial_workflow_items_restore_the_run_lookup() {
        let mut source = AppState::new("source".into(), None);
        source.apply_stream_frame(StreamFrame::FlowStart {
            run_id: "restored-run".into(),
            flow_name: "restored".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        let restored = AppState::new("restored".into(), None)
            .with_initial_items(source.items.iter().cloned().collect());

        assert_eq!(restored.workflow_run_to_panel.get("restored-run"), Some(&0));
        assert_eq!(restored.last_workflow_panel_idx, Some(0));
    }

    #[test]
    fn output_store_detects_same_cardinality_interaction_change() {
        let mut store = OutputStore::default();
        store.push(OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: WorkflowProjection::new(atman_runtime::event::TurnId::now()),
            expanded_nodes: HashSet::from(["old".into()]),
            panel_expanded: true,
            started_at: Instant::now(),
            ended_at: None,
            cancelled: false,
        });
        let before = store.revisions()[0];
        assert!(store.mutate(0, OutputMutation::Interaction, |item| {
            let OutputItem::WorkflowPanel { expanded_nodes, .. } = item else {
                return false;
            };
            expanded_nodes.remove("old");
            expanded_nodes.insert("new".into());
            true
        }));
        let after = store.revisions()[0];
        assert_ne!(after.interaction, before.interaction);
        assert_ne!(after.layout, before.layout);
        assert_eq!(after.semantic, before.semantic);
    }

    #[test]
    fn output_store_tracks_animated_ids_across_lifecycle_changes() {
        let mut store = OutputStore::default();
        store.push(OutputItem::Thinking {
            text: "working".into(),
            done: false,
            disclosure: Disclosure::Summary,
            retried: false,
        });
        let thinking_id = store.revisions()[0].id;
        assert_eq!(
            store.animated_ids().iter().copied().collect::<Vec<_>>(),
            [thinking_id]
        );

        assert!(store.mutate(0, OutputMutation::Semantic, |item| {
            let OutputItem::Thinking { done, .. } = item else {
                return false;
            };
            *done = true;
            true
        }));
        assert!(store.animated_ids().is_empty());

        store.push(OutputItem::Bash {
            handle: "bash-1".into(),
            title: None,
            command: None,
            output: String::new(),
            done: false,
            expanded: false,
        });
        let bash_id = store.revisions()[1].id;
        assert_eq!(
            store.animated_ids().iter().copied().collect::<Vec<_>>(),
            [bash_id]
        );
        store.remove(1);
        assert!(store.animated_ids().is_empty());
    }

    #[test]
    fn assistant_append_preserves_source_generation_until_finalization() {
        let mut app = AppState::new("source-generation".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "one".into(),
            model: "model".into(),
            run_id: None,
        });
        let initial = app.items.revisions()[0];
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: " two".into(),
            model: "model".into(),
            run_id: None,
        });
        let appended = app.items.revisions()[0];
        assert_eq!(appended.source_generation, initial.source_generation);
        assert_ne!(appended.layout, initial.layout);

        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 2,
            run_id: None,
        });
        let finalized = app.items.revisions()[0];
        assert_ne!(finalized.source_generation, appended.source_generation);
    }

    #[test]
    fn task_hover_does_not_invalidate_output_structure() {
        let mut app = AppState::new("s".into(), None);
        app.push_note("stable", NoteLevel::Info);
        let items_version = app.items_version;
        let structure_revision = app.items.structure_revision();
        app.set_hovered_task(Some(atman_runtime::TaskId::now()));
        assert_eq!(app.items_version, items_version);
        assert_eq!(app.items.structure_revision(), structure_revision);
    }

    #[test]
    fn duplicate_terminal_exit_preserves_item_revision() {
        let mut app = AppState::new("s".into(), None);
        app.push_item(OutputItem::Terminal {
            handle: "term_s_0".into(),
            title: None,
            command: None,
            screen: TerminalScreen {
                rows: 0,
                cols: 0,
                cells: Vec::new(),
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: Vec::new(),
            mode: TerminalViewMode::Capture,
            done: false,
            expanded: false,
            scroll_offset: None,
        });
        let frame = StreamFrame::TerminalExited {
            handle: "term_s_0".into(),
            exit_code: Some(0),
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        };
        app.apply_stream_frame(frame.clone());
        let after_first = app.items.revisions()[0];
        app.apply_stream_frame(frame);
        assert_eq!(app.items.revisions()[0], after_first);
    }

    #[test]
    fn workflow_stream_versions_change_only_for_applied_mutations() {
        let mut app = AppState::new("s".into(), None);
        let baseline = app.items_version;
        app.apply_stream_frame(flow_start("f", "r1"));
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph: atman_runtime::nodegraph::FlowGraph {
                flow_name: "f".into(),
                root: Vec::new(),
            },
        });
        let after_flow = app.items_version;
        assert_ne!(after_flow, baseline, "FlowGraph should bump version");
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "missing".into(),
            tool_use_id: "tu".into(),
            tool: "t".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });
        assert_eq!(
            app.items_version, after_flow,
            "a tool whose parent is absent does not mutate the projection"
        );
    }

    #[test]
    fn ctrl_o_targets_latest_workflow_tool_node() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("f", "r1"));
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph: atman_runtime::nodegraph::FlowGraph {
                flow_name: "f".into(),
                root: Vec::new(),
            },
        });
        app.apply_stream_frame(StreamFrame::FlowNodeStart {
            run_id: "r1".into(),
            node_id: "stmt_0".into(),
            kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
            label: "stmt_0".into(),
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "stmt_0".into(),
            tool_use_id: "tu_last".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });
        assert!(app.toggle_last_tool_expansion());
        let expanded = app.items.iter().find_map(|it| match it {
            OutputItem::WorkflowPanel { expanded_nodes, .. } => Some(expanded_nodes.clone()),
            _ => None,
        });
        assert!(expanded.unwrap().contains("0/0/0"));
    }

    #[test]
    fn nested_node_start_attaches_under_parent() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("f", "r1"));
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph: atman_runtime::nodegraph::FlowGraph {
                flow_name: "f".into(),
                root: Vec::new(),
            },
        });
        app.apply_stream_frame(StreamFrame::FlowNodeStart {
            run_id: "r1".into(),
            node_id: "stmt_0".into(),
            kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
            label: "stmt_0".into(),
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "stmt_0".into(),
            tool_use_id: "tu_1".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });
        let panel = app
            .items
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .unwrap();
        let stmt = panel.find_node("r1::stmt_0").unwrap();
        assert_eq!(stmt.children.len(), 1);
        assert_eq!(stmt.children[0].id, "tool:r1:tu_1");
    }

    // ── Workflow panel state machine tests ──

    fn flow_start(name: &str, run_id: &str) -> StreamFrame {
        StreamFrame::FlowStart {
            run_id: run_id.into(),
            flow_name: name.into(),
            parent_run_id: None,
            parent_node_id: None,
        }
    }

    fn flow_done(run_id: &str, cancelled: bool) -> StreamFrame {
        StreamFrame::FlowDone {
            run_id: run_id.into(),
            flow_name: "test".into(),
            ok: !cancelled,
            cancelled,
            suicide: false,
        }
    }

    fn subflow_start(name: &str, run_id: &str, parent_run_id: &str) -> StreamFrame {
        StreamFrame::FlowStart {
            run_id: run_id.into(),
            flow_name: name.into(),
            parent_run_id: Some(parent_run_id.into()),
            parent_node_id: None,
        }
    }

    fn workflow_panels(app: &AppState) -> Vec<(usize, bool)> {
        app.items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| match it {
                OutputItem::WorkflowPanel { ended_at, .. } => Some((i, ended_at.is_none())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn normal_flow_lifecycle_creates_and_closes_panel() {
        let mut app = AppState::new("s".into(), None);
        assert!(!app.has_running_workflow());

        app.apply_stream_frame(flow_start("agent", "r1"));
        assert!(app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1);
        assert!(panels[0].1, "panel should be open");

        app.apply_stream_frame(flow_done("r1", false));
        assert!(!app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1);
        assert!(!panels[0].1, "panel should be closed");
    }

    #[test]
    fn subflow_reuses_parent_panel() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));
        assert_eq!(workflow_panels(&app).len(), 1);

        // Subflow with parent_run_id should reuse parent's panel.
        app.apply_stream_frame(subflow_start("agent_loop", "r2", "r1"));
        let panels = workflow_panels(&app);
        assert_eq!(
            panels.len(),
            1,
            "subflow must reuse parent panel, not create new one"
        );
    }

    #[test]
    fn subflow_orphan_parent_not_in_map_creates_new_panel() {
        let mut app = AppState::new("s".into(), None);
        // No parent flow Start → parent not in HashMap.
        app.apply_stream_frame(subflow_start("orphan", "r2", "nonexistent"));
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1, "orphan subflow creates new panel");
    }

    #[test]
    fn course_correct_reopens_cancelled_panel() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));
        // Flow is cancelled.
        app.apply_stream_frame(flow_done("r1", true));
        assert!(!app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1);
        assert!(!panels[0].1, "panel closed after cancelled FlowDone");

        // New flow starts (course-correct restart).
        app.apply_stream_frame(flow_start("agent", "r2"));
        assert!(app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(
            panels.len(),
            1,
            "must reopen cancelled panel, not create new"
        );
        assert!(panels[0].1, "panel must be reopened");
    }

    #[test]
    fn course_correct_after_push_user_turn_peeks_past_userturn() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));

        // push_user_turn means a new user turn started — this only happens
        // when has_running_workflow() is false. Course-correct during a
        // running flow does NOT call push_user_turn, so this scenario
        // represents a normal new message after a cancelled flow.
        app.push_user_turn("new msg".into());
        app.close_current_workflow_panel(true, None);

        // UserTurn between the old panel and FlowStart means new turn →
        // new panel, not reopen.
        app.apply_stream_frame(flow_start("agent", "r2"));
        let panels = workflow_panels(&app);
        assert_eq!(
            panels.len(),
            2,
            "UserTurn means new turn — new panel expected"
        );
        assert!(panels[1].1, "new panel must be open");
        assert!(app.workflow_run_to_panel.contains_key("r2"));
    }

    #[test]
    fn scoped_tool_frames_route_to_their_run_panel() {
        use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
        use atman_runtime::workflow::NodeStatus;

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("first", "r1"));
        app.push_user_turn("next".into());
        app.apply_stream_frame(flow_start("second", "r2"));

        app.apply_stream_frame(StreamFrame::FlowNodeStart {
            run_id: "r1".into(),
            node_id: "dispatch_all".into(),
            kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                path: "dispatch_all".into(),
            },
            label: "dispatch_all".into(),
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "dispatch_all".into(),
            tool_use_id: "tu_1".into(),
            tool: "fs.read".into(),
            args_preview: String::new(),
            call_intent: None,
        });
        app.apply_stream_frame(StreamFrame::ToolResultMsg {
            flow_run_id: Some("r1".into()),
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "done".into(),
                    is_error: false,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        let first_idx = app.workflow_run_to_panel["r1"];
        let second_idx = app.workflow_run_to_panel["r2"];
        let OutputItem::WorkflowPanel {
            graph: first_graph, ..
        } = &app.items[first_idx]
        else {
            panic!("first workflow panel");
        };
        let tool = first_graph.find_node("tool:r1:tu_1").expect("r1 tool");
        assert_eq!(tool.status, NodeStatus::Ok);
        let OutputItem::WorkflowPanel {
            graph: second_graph,
            ..
        } = &app.items[second_idx]
        else {
            panic!("second workflow panel");
        };
        assert!(second_graph.find_node("tool:r1:tu_1").is_none());
        assert!(second_graph.find_node("r1::dispatch_all").is_none());
    }

    #[test]
    fn has_running_workflow_false_when_all_closed() {
        let mut app = AppState::new("s".into(), None);
        assert!(!app.has_running_workflow());

        app.apply_stream_frame(flow_start("agent", "r1"));
        assert!(app.has_running_workflow());

        app.apply_stream_frame(flow_done("r1", false));
        assert!(!app.has_running_workflow());
    }

    #[test]
    fn has_running_workflow_true_with_two_open_panels() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("a", "r1"));
        app.apply_stream_frame(flow_start("b", "r2"));
        assert!(app.has_running_workflow());
        assert_eq!(workflow_panels(&app).len(), 2);

        // Close one panel — still running.
        app.apply_stream_frame(flow_done("r1", false));
        assert!(app.has_running_workflow());

        // Close the other — done.
        app.apply_stream_frame(flow_done("r2", false));
        assert!(!app.has_running_workflow());
    }

    #[test]
    fn close_current_workflow_panel_upgrades_cancelled_flag() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));

        // push_user_turn closes with cancelled:false
        app.push_user_turn("msg".into());

        let panels = workflow_panels(&app);
        assert!(!panels[0].1, "closed by push_user_turn");

        // Interjection handler closes with cancelled:true — must upgrade.
        app.close_current_workflow_panel(true, None);

        let panel = match &app.items[panels[0].0] {
            OutputItem::WorkflowPanel { cancelled, .. } => *cancelled,
            _ => panic!("expected WorkflowPanel"),
        };
        assert!(panel, "cancelled flag must be upgraded from false to true");
    }

    #[test]
    fn flow_done_for_non_flowstart_event_does_not_create_panel() {
        let mut app = AppState::new("s".into(), None);
        // FlowNodeEnd is not FlowStart or FlowDone — should not create panel.
        app.apply_stream_frame(StreamFrame::FlowNodeEnd {
            run_id: "r1".into(),
            node_id: "n1".into(),
            status: atman_runtime::event::FlowNodeStatus::Ok,
            output_preview: None,
            parent_node_id: None,
        });
        assert_eq!(workflow_panels(&app).len(), 0);
    }

    #[test]
    fn two_consecutive_flows_without_userturn_reuses_panel() {
        // Scenario: course-correct restarts flow immediately (no UserTurn).
        let mut app = AppState::new("s".into(), None);

        // First flow.
        app.apply_stream_frame(flow_start("agent", "r1"));
        app.apply_stream_frame(flow_done("r1", true)); // cancelled

        // Second flow starts immediately.
        app.apply_stream_frame(flow_start("agent", "r2"));
        app.apply_stream_frame(flow_done("r2", false));

        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1, "should reuse panel, not create second");
    }

    #[test]
    fn multiple_flow_starts_without_done_only_one_open_panel() {
        // Simulates L1 nudges: flow restarts LLM internally, new FlowStart
        // for each restart within same run.
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));

        // L1 nudge 1: internal LLM restart — a new subflow might start
        // but the top-level flow is unchanged.
        app.apply_stream_frame(subflow_start("agent_loop", "s1", "r1"));
        app.apply_stream_frame(StreamFrame::FlowDone {
            run_id: "s1".into(),
            flow_name: "agent_loop".into(),
            ok: true,
            cancelled: false,
            suicide: false,
        });

        // L1 nudge 2: another internal restart.
        app.apply_stream_frame(subflow_start("agent_loop", "s2", "r1"));
        app.apply_stream_frame(StreamFrame::FlowDone {
            run_id: "s2".into(),
            flow_name: "agent_loop".into(),
            ok: true,
            cancelled: false,
            suicide: false,
        });

        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1, "L1 nudges must not create extra panels");
        assert!(panels[0].1, "top-level panel still open");
    }

    #[test]
    fn flow_done_routes_to_correct_panel_by_run_id() {
        // Two concurrent flows: FlowDone for r1 closes panel[0], r2 stays open.
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("a", "r1"));
        app.apply_stream_frame(flow_start("b", "r2"));

        app.apply_stream_frame(flow_done("r1", false));

        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 2);
        assert!(!panels[0].1, "panel 0 (r1) should be closed");
        assert!(panels[1].1, "panel 1 (r2) should still be open");
        assert!(app.has_running_workflow());
    }

    #[test]
    fn cancel_escalates_through_all_levels() {
        // L4 hard stop: FlowDone(cancelled:true) → panel marked cancelled.
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));
        app.apply_stream_frame(flow_done("r1", true));

        let panel = match &app.items[0] {
            OutputItem::WorkflowPanel { cancelled, .. } => *cancelled,
            _ => panic!("expected WorkflowPanel"),
        };
        assert!(panel, "L4 hard stop must mark panel as cancelled");
    }
}

#[cfg(test)]
mod terminal_stream_tests {
    use super::*;
    use atman_runtime::tools::term::{TermStateSnapshot, TerminalScreen};

    fn dummy_screen() -> TerminalScreen {
        TerminalScreen {
            rows: 2,
            cols: 3,
            cells: vec![atman_runtime::tools::term::TerminalCell::default(); 6],
            cursor: None,
            alt_screen: false,
        }
    }

    #[test]
    fn terminal_chunk_creates_output_item() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            call_intent: atman_runtime::message::ToolCallIntent::new("检查终端状态"),
            tool_use_id: None,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::Terminal {
                handle,
                title,
                mode,
                done,
                ..
            } => {
                assert_eq!(handle, "term_s_0");
                assert_eq!(title.as_deref(), Some("检查终端状态"));
                assert_eq!(*mode, TerminalViewMode::Capture);
                assert!(!*done);
            }
            _ => panic!("expected Terminal item"),
        }
    }

    #[test]
    fn terminal_chunk_projects_raw_command_from_task_registry() {
        let registry = atman_runtime::TaskRegistry::new();
        registry.register(
            atman_runtime::TaskKind::Terminal,
            atman_runtime::TaskDisplay {
                label: "检查系统负载".into(),
                command: Some("htop --sort-key PERCENT_CPU".into()),
            },
            "term_s_0".into(),
            "s".into(),
            tokio_util::sync::CancellationToken::new(),
        );
        let mut app = AppState::new("s".into(), None).with_task_registry(registry);
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: Vec::new(),
            screen: Some(dummy_screen()),
            state: TermStateSnapshot::Running,
            call_intent: atman_runtime::message::ToolCallIntent::new("检查系统负载"),
            tool_use_id: None,
            run_id: None,
        });
        let OutputItem::Terminal { title, command, .. } = &app.items[0] else {
            panic!("expected terminal item");
        };
        assert_eq!(title.as_deref(), Some("检查系统负载"));
        assert_eq!(command.as_deref(), Some("htop --sort-key PERCENT_CPU"));
    }

    #[test]
    fn terminal_chunk_updates_existing_item() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b" world".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1, "should update existing, not create new");
        match &app.items[0] {
            OutputItem::Terminal {
                accumulated_bytes, ..
            } => {
                assert_eq!(accumulated_bytes, b"hi world");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn terminal_exited_marks_done() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen),
            state: TermStateSnapshot::Running,
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::TerminalExited {
            handle: "term_s_0".into(),
            exit_code: Some(0),
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::Terminal { done, .. } => assert!(*done),
            _ => panic!(),
        }
    }

    #[test]
    fn bash_exit_without_output_keeps_intent_title() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::BashExited {
            handle: "bg_s_0".into(),
            exit_code: Some(0),
            error: None,
            call_intent: atman_runtime::message::ToolCallIntent::new("检查构建结果"),
            tool_use_id: None,
            run_id: None,
        });

        assert!(matches!(
            &app.items[0],
            OutputItem::Bash {
                title: Some(title),
                output,
                done: true,
                ..
            } if title == "检查构建结果" && output.is_empty()
        ));
    }

    #[test]
    fn sub_agent_tool_frames_do_not_mutate_main_document_items() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::SubAgentStarted {
            handle: "agent_1".into(),
            goal: "check".into(),
            child_run_id: "child_run".into(),
            model: "m".into(),
            tool_use_id: None,
        });
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"main".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            kind: "stdout".into(),
            line: "main\n".into(),
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });

        let baseline = app.items.len();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"sub".to_vec(),
            screen: Some(screen),
            state: TermStateSnapshot::Running,
            call_intent: None,
            tool_use_id: None,
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::TerminalExited {
            handle: "term_s_0".into(),
            exit_code: Some(0),
            call_intent: None,
            tool_use_id: None,
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            kind: "stdout".into(),
            line: "sub\n".into(),
            call_intent: None,
            tool_use_id: None,
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::BashExited {
            handle: "bg_s_0".into(),
            exit_code: Some(0),
            error: None,
            call_intent: None,
            tool_use_id: None,
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::DiffPreview {
            title: "sub diff".into(),
            old_content: None,
            new_content: None,
            unified_diff: Some("diff".into()),
            tool_use_id: None,
            run_id: Some("child_run".into()),
        });

        assert_eq!(app.items.len(), baseline);
        match &app.items[1] {
            OutputItem::Terminal {
                accumulated_bytes,
                done,
                ..
            } => {
                assert_eq!(accumulated_bytes, b"main");
                assert!(!*done);
            }
            _ => panic!("expected Terminal item"),
        }
        match &app.items[2] {
            OutputItem::Bash { output, done, .. } => {
                assert_eq!(output, "main\n");
                assert!(!*done);
            }
            _ => panic!("expected Bash item"),
        }
        assert!(matches!(
            app.detached_task_details
                .get("term_s_0")
                .map(|detail| &detail.item),
            Some(OutputItem::Terminal {
                accumulated_bytes,
                done: true,
                ..
            }) if accumulated_bytes == b"sub"
        ));
        assert!(matches!(
            app.detached_task_details
                .get("bg_s_0")
                .map(|detail| &detail.item),
            Some(OutputItem::Bash {
                output,
                done: true,
                ..
            }) if output == "sub\n"
        ));
    }

    #[test]
    fn duplicate_sub_agent_workflow_frame_does_not_leak_to_main_workflow() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::SubAgentStarted {
            handle: "agent_1".into(),
            goal: "check".into(),
            child_run_id: "child_run".into(),
            model: "m".into(),
            tool_use_id: None,
        });
        let frame = StreamFrame::FlowStart {
            run_id: "child_run".into(),
            flow_name: "child".into(),
            parent_run_id: None,
            parent_node_id: None,
        };

        app.apply_stream_frame(frame.clone());
        app.apply_stream_frame(frame);

        assert_eq!(
            app.items
                .iter()
                .filter(|item| matches!(item, OutputItem::WorkflowPanel { .. }))
                .count(),
            0
        );
        let OutputItem::SubAgentActivity { workflow_graph, .. } = &app.items[0] else {
            panic!("expected sub-agent activity");
        };
        assert_eq!(workflow_graph.root.len(), 1);
    }

    #[test]
    fn open_task_panel_different_handles_create_different_panels() {
        use atman_runtime::tools::term::{TerminalCell, TerminalScreen};
        let mut app = crate::UiState::new(AppState::new("s".into(), None));
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.items.push(OutputItem::Terminal {
            handle: "term_s_0".into(),
            title: None,
            command: None,
            screen: TerminalScreen {
                rows: 2,
                cols: 5,
                cells: vec![TerminalCell::default(); 10],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"hi".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        app.items.push(OutputItem::Terminal {
            handle: "term_s_1".into(),
            title: None,
            command: None,
            screen: TerminalScreen {
                rows: 3,
                cols: 7,
                cells: vec![TerminalCell::default(); 21],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"bye".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        let canvas = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.open_task_panel("term_s_0", canvas);
        app.open_task_panel("term_s_1", canvas);
        assert_eq!(app.wm.panels.len(), 2);
        assert_eq!(app.wm.panels[0].label, "term_s_0");
        assert_eq!(app.wm.panels[1].label, "term_s_1");
    }

    #[test]
    fn open_task_panel_background_preserves_focus() {
        use atman_runtime::tools::term::{TerminalCell, TerminalScreen};
        let mut app = crate::UiState::new(AppState::new("s".into(), None));
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.items.push(OutputItem::Terminal {
            handle: "term_s_0".into(),
            title: None,
            command: None,
            screen: TerminalScreen {
                rows: 2,
                cols: 5,
                cells: vec![TerminalCell::default(); 10],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"hi".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        app.items.push(OutputItem::Terminal {
            handle: "term_s_1".into(),
            title: None,
            command: None,
            screen: TerminalScreen {
                rows: 3,
                cols: 7,
                cells: vec![TerminalCell::default(); 21],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"bye".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        let canvas = ratatui::layout::Rect::new(0, 0, 80, 24);

        // Open the first panel via a foreground click (steals focus).
        app.open_task_panel("term_s_0", canvas);
        let focused_a = app.wm.focused_id();
        assert!(focused_a.is_some());

        // A background completion for term_s_1 must NOT change focus.
        app.open_task_panel_background("term_s_1");
        assert_eq!(app.wm.panels.len(), 2);
        assert_eq!(app.wm.focused_id(), focused_a, "must keep foreground focus");
        assert!(
            app.toasts
                .iter()
                .any(|t| t.message.contains("background task")),
            "should push a toast when focus preserved"
        );
    }
}

#[cfg(test)]
mod terminal_e2e_tests {
    use super::*;
    use crate::output::{LayoutCache, LayoutKey, LayoutRequest, RenderCtx};
    use atman_runtime::tools::term::{TermStateSnapshot, TerminalCell, TerminalScreen};

    #[test]
    fn full_pipeline_terminal_chunk_to_rendered_lines() {
        let mut app = AppState::new("s".into(), None);
        let screen = TerminalScreen {
            rows: 2,
            cols: 5,
            cells: {
                let mut v = vec![TerminalCell::default(); 10];
                v[0].chars = "h".into();
                v[1].chars = "i".into();
                v
            },
            cursor: Some((0, 2)),
            alt_screen: false,
        };
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });

        let cache_key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let empty_set = std::collections::HashSet::new();
        let ctx = RenderCtx {
            expanded_tools: &empty_set,
            messages: &[],
            panel_width: 80,
            hovered_thinking_idx: None,
            hovered_output_node: None,
            animation_frame: 0,
        };
        let mut cache = LayoutCache::default();
        let metrics = cache.update_dirty(
            cache_key,
            &app.items,
            &ctx,
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 50,
                follow_tail_rows: None,
            },
        );
        let (lines, _ranges, _regions) =
            cache.visible_slice(0, metrics.total_rows.min(50), ctx.animation_frame);
        assert!(
            lines.len() > 2,
            "should render header + blank + screen rows"
        );
        let header = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(header.contains("term_s_0"), "header should contain handle");
        assert!(header.contains("capture"), "should be capture mode");
    }

    #[test]
    fn bash_panel_from_history_renders_content() {
        use ratatui::backend::TestBackend;
        use std::collections::HashSet;

        let mut app = crate::UiState::new(AppState::new("s".into(), None));
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 24));

        // Simulate a completed bash task: item in items, snapshot in task_snapshots.
        app.apply_stream_frame(StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            kind: "stdout".into(),
            line: "hello from bash\n".into(),
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::BashExited {
            handle: "bg_s_0".into(),
            exit_code: Some(0),
            error: None,
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        app.apply_task_event(atman_runtime::TaskEvent::Registered(
            TaskSnapshotHolder::snap("bg_s_0"),
        ));

        let canvas = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.open_task_panel("bg_s_0", canvas);

        // Find the opened panel.
        let panel = app
            .wm
            .panels
            .iter_mut()
            .find(|p| p.label == "bg_s_0")
            .unwrap();
        assert!(
            panel.content.is_some(),
            "open_task_panel must set panel.content for bash"
        );

        let backend = TestBackend::new(60, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        // Split the UiState into its field borrows so the panel (in wm) can be
        // mutated independently of the read-only AppState fields.
        let wm = &mut app.wm;
        let snapshots = &app.app.task_snapshots;
        let items = &app.app.items;
        let item_revisions = app.app.items.revisions();
        let handle_index = &app.app.handle_index;
        let detached_task_details = &app.app.detached_task_details;
        let task_handle_index = &app.app.task_handle_index;
        let workflow_run_to_panel = &app.app.workflow_run_to_panel;
        let task_snapshots_revision = app.app.task_snapshots_revision;
        let activity_nodes = &app.app.activity_nodes;

        terminal
            .draw(|f| {
                let mut hitmap = crate::wm::WmHitmap::default();
                let empty_mcp: std::collections::HashSet<String> = HashSet::new();
                let empty_resources = std::collections::HashMap::new();
                let empty_prompts = std::collections::HashMap::new();
                let browser = crate::mcp_manager::McpBrowserState {
                    tab: crate::mcp_manager::McpBrowserTab::Resources,
                    content_revision: 0,
                    resources: &empty_resources,
                    prompts: &empty_prompts,
                };
                crate::wm::content::render_panel_content(
                    f,
                    f.area(),
                    &mut wm.panels[0],
                    snapshots,
                    items,
                    item_revisions,
                    handle_index,
                    detached_task_details,
                    task_handle_index,
                    workflow_run_to_panel,
                    task_snapshots_revision,
                    activity_nodes,
                    &None,
                    &None,
                    &mut hitmap,
                    0,
                    true,
                    false,
                    &[],
                    &empty_mcp,
                    0,
                    &None,
                    &browser,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let joined: String = buf
            .content
            .chunks(60)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined.contains("hello from bash"),
            "bash output must appear in panel, got: {joined}"
        );
    }

    struct TaskSnapshotHolder;
    impl TaskSnapshotHolder {
        fn snap(src: &str) -> atman_runtime::TaskSnapshot {
            atman_runtime::TaskSnapshot {
                id: atman_runtime::TaskId::now(),
                kind: atman_runtime::TaskKind::Bash,
                label: format!("bash {src}"),
                command: None,
                status: atman_runtime::TaskStatus::Ok,
                started_at: std::time::Instant::now(),
                ended_at: Some(std::time::Instant::now()),
                source_handle: src.to_string(),
                session_id: "s".to_string(),
                workspace_id: None,
                flow_run_id: None,
                termination: None,
            }
        }
    }
}
