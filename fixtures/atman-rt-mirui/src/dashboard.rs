use mirui::{
    ecs::{Entity, World},
    prelude::{AlignItems, Color, ColorToken, FontToken, Padding, Theme},
    ui::{
        Style,
        widgets::{ParagraphStyle, ProgressBar, Text, TextAlign},
    },
};

const PANEL: ColorToken = ColorToken::custom("panel");
const PANEL_RAISED: ColorToken = ColorToken::custom("panel-raised");
const CANVAS_IDLE: ColorToken = ColorToken::custom("canvas-idle");
const CANVAS_ACQUIRE: ColorToken = ColorToken::custom("canvas-acquire");
const CANVAS_MUTATE: ColorToken = ColorToken::custom("canvas-mutate");
const CANVAS_PRESENT: ColorToken = ColorToken::custom("canvas-present");
const INACTIVE: ColorToken = ColorToken::custom("inactive");

pub(crate) fn theme() -> Theme {
    Theme::dark().with_many([
        (ColorToken::Surface, Color::rgb(15, 19, 24)),
        (ColorToken::OnSurface, Color::rgb(216, 222, 224)),
        (ColorToken::SurfaceVariant, Color::rgb(23, 29, 36)),
        (ColorToken::OnSurfaceVariant, Color::rgb(129, 141, 148)),
        (ColorToken::Outline, Color::rgb(48, 58, 67)),
        (ColorToken::Primary, Color::rgb(112, 151, 142)),
        (ColorToken::OnPrimary, Color::rgb(14, 21, 21)),
        (ColorToken::Secondary, Color::rgb(111, 132, 153)),
        (ColorToken::OnSecondary, Color::rgb(14, 19, 24)),
        (ColorToken::Tertiary, Color::rgb(142, 132, 149)),
        (ColorToken::OnTertiary, Color::rgb(20, 17, 22)),
        (ColorToken::Success, Color::rgb(118, 148, 121)),
        (ColorToken::Error, Color::rgb(159, 115, 112)),
        (ColorToken::Shadow, Color::rgb(4, 6, 8)),
        (PANEL, Color::rgb(26, 33, 40)),
        (PANEL_RAISED, Color::rgb(31, 39, 47)),
        (CANVAS_IDLE, Color::rgb(27, 35, 41)),
        (CANVAS_ACQUIRE, Color::rgb(31, 43, 44)),
        (CANVAS_MUTATE, Color::rgb(31, 39, 48)),
        (CANVAS_PRESENT, Color::rgb(42, 37, 44)),
        (INACTIVE, Color::rgb(52, 62, 68)),
    ])
}

#[derive(Clone, Copy)]
pub(crate) struct DashboardNodes {
    pub(crate) root: Entity,
    pub(crate) canvas: Entity,
    title: Entity,
    subtitle: Entity,
    status: Entity,
    event_text: Entity,
    bridge_state: Entity,
    boundary_note: Entity,
    metric_labels: [Entity; 3],
    metric_values: [Entity; 3],
    stages: [Entity; 3],
    stage_labels: [Entity; 3],
    links: [Entity; 2],
    progress: Entity,
}

impl DashboardNodes {
    pub(crate) fn build(world: &mut World, root: Entity) -> Result<Self, String> {
        mirui::ui! {
            :(
                parent: root
                world: world
            :)

            Column (
                grow: 1.0,
                padding: Padding::all(24),
                row_gap: 14,
                bg_color: ColorToken::Surface,
                clip_children: true
            ) {
                Row (height: 52, align: AlignItems::Center, column_gap: 12) {
                    View (width: 3, height: 34, bg_color: ColorToken::Primary)
                    Column (grow: 1.0, row_gap: 3) {
                        Text (
                            "ATMAN RUNTIME",
                            id: "app_title",
                            height: 24,
                            font: FontToken::Heading,
                            font_size: 20,
                            text_color: ColorToken::OnSurface,
                            paragraph: left_label()
                        )
                        Text (
                            "VM-DRIVEN UI / OWNER-THREAD RENDER",
                            id: "app_subtitle",
                            height: 15,
                            font: FontToken::Mono,
                            font_size: 9,
                            text_color: ColorToken::OnSurfaceVariant,
                            paragraph: left_label()
                        )
                    }
                    Text (
                        "RUN / WAITING",
                        id: "run_status",
                        width: 132,
                        height: 28,
                        bg_color: PANEL_RAISED,
                        border_radius: 2,
                        font: FontToken::Mono,
                        font_size: 9,
                        text_color: ColorToken::OnSurfaceVariant,
                        paragraph: ParagraphStyle::label()
                    )
                }
                Row (height: 76, column_gap: 10) {
                    Column (
                        grow: 1.0,
                        padding: Padding::all(12),
                        row_gap: 5,
                        bg_color: ColorToken::SurfaceVariant,
                        border_radius: 2
                    ) {
                        Text (
                            "FRAME",
                            id: "metric_label_1",
                            height: 13,
                            font: FontToken::Mono,
                            font_size: 8,
                            text_color: ColorToken::OnSurfaceVariant,
                            paragraph: left_label()
                        )
                        Text (
                            "00 / 03",
                            id: "metric_value_1",
                            height: 28,
                            font: FontToken::Heading,
                            font_size: 19,
                            text_color: ColorToken::OnSurface,
                            paragraph: left_label()
                        )
                    }
                    Column (
                        grow: 1.0,
                        padding: Padding::all(12),
                        row_gap: 5,
                        bg_color: ColorToken::SurfaceVariant,
                        border_radius: 2
                    ) {
                        Text (
                            "THREAD BOUNDARY",
                            id: "metric_label_2",
                            height: 13,
                            font: FontToken::Mono,
                            font_size: 8,
                            text_color: ColorToken::OnSurfaceVariant,
                            paragraph: left_label()
                        )
                        Text (
                            "WORKER + OWNER",
                            id: "metric_value_2",
                            height: 28,
                            font: FontToken::Heading,
                            font_size: 16,
                            text_color: ColorToken::OnSurface,
                            paragraph: left_label()
                        )
                    }
                    Column (
                        grow: 1.0,
                        padding: Padding::all(12),
                        row_gap: 5,
                        bg_color: ColorToken::SurfaceVariant,
                        border_radius: 2
                    ) {
                        Text (
                            "RESOURCE LEASE",
                            id: "metric_label_3",
                            height: 13,
                            font: FontToken::Mono,
                            font_size: 8,
                            text_color: ColorToken::OnSurfaceVariant,
                            paragraph: left_label()
                        )
                        Text (
                            "IDLE",
                            id: "metric_value_3",
                            height: 28,
                            font: FontToken::Heading,
                            font_size: 16,
                            text_color: ColorToken::OnSurface,
                            paragraph: left_label()
                        )
                    }
                }
                Row (grow: 1.0, column_gap: 12) {
                    Column (
                        id: "runtime_canvas",
                        grow: 1.0,
                        padding: Padding::all(18),
                        row_gap: 14,
                        bg_color: CANVAS_IDLE,
                        border_radius: 2
                    ) {
                        Row (height: 22, align: AlignItems::Center) {
                            Text (
                                "FRAME PIPELINE",
                                grow: 1.0,
                                font: FontToken::Mono,
                                font_size: 9,
                                text_color: ColorToken::OnSurfaceVariant,
                                paragraph: left_label()
                            )
                            Text (
                                "LIVE DATA",
                                font: FontToken::Mono,
                                font_size: 8,
                                text_color: ColorToken::Primary
                            )
                        }
                        Text (
                            "Awaiting the first VM command.",
                            id: "event_text",
                            grow: 1.0,
                            font: FontToken::Heading,
                            font_size: 18,
                            text_color: ColorToken::OnSurface
                        )
                        ProgressBar (
                            id: "progress",
                            height: 5,
                            border_radius: 2,
                            value: 0.0,
                            track_color: INACTIVE,
                            fill_color: ColorToken::Primary
                        )
                        Row (height: 34, align: AlignItems::Center, column_gap: 8) {
                            Text (
                                "01",
                                id: "stage_1",
                                width: 34,
                                height: 34,
                                bg_color: INACTIVE,
                                border_radius: 2,
                                font: FontToken::Mono,
                                font_size: 10,
                                text_color: ColorToken::OnSurfaceVariant,
                                paragraph: ParagraphStyle::label()
                            )
                            View (id: "link_1", grow: 1.0, height: 2, bg_color: INACTIVE)
                            Text (
                                "02",
                                id: "stage_2",
                                width: 34,
                                height: 34,
                                bg_color: INACTIVE,
                                border_radius: 2,
                                font: FontToken::Mono,
                                font_size: 10,
                                text_color: ColorToken::OnSurfaceVariant,
                                paragraph: ParagraphStyle::label()
                            )
                            View (id: "link_2", grow: 1.0, height: 2, bg_color: INACTIVE)
                            Text (
                                "03",
                                id: "stage_3",
                                width: 34,
                                height: 34,
                                bg_color: INACTIVE,
                                border_radius: 2,
                                font: FontToken::Mono,
                                font_size: 10,
                                text_color: ColorToken::OnSurfaceVariant,
                                paragraph: ParagraphStyle::label()
                            )
                        }
                        Row (height: 16, column_gap: 8) {
                            Text (
                                "ACQUIRE",
                                id: "stage_label_1",
                                grow: 1.0,
                                font: FontToken::Mono,
                                font_size: 8,
                                text_color: ColorToken::OnSurfaceVariant,
                                paragraph: ParagraphStyle::label()
                            )
                            Text (
                                "MUTATE",
                                id: "stage_label_2",
                                grow: 1.0,
                                font: FontToken::Mono,
                                font_size: 8,
                                text_color: ColorToken::OnSurfaceVariant,
                                paragraph: ParagraphStyle::label()
                            )
                            Text (
                                "PRESENT",
                                id: "stage_label_3",
                                grow: 1.0,
                                font: FontToken::Mono,
                                font_size: 8,
                                text_color: ColorToken::OnSurfaceVariant,
                                paragraph: ParagraphStyle::label()
                            )
                        }
                    }
                    Column (
                        width: 236,
                        padding: Padding::all(16),
                        row_gap: 10,
                        bg_color: PANEL,
                        border_radius: 2
                    ) {
                        Text (
                            "HOST BOUNDARY",
                            height: 18,
                            font: FontToken::Mono,
                            font_size: 9,
                            text_color: ColorToken::OnSurfaceVariant,
                            paragraph: left_label()
                        )
                        Row (height: 30, align: AlignItems::Center, column_gap: 9) {
                            View (width: 3, height: 18, bg_color: ColorToken::Primary)
                            Text (
                                "ATMAN VM WORKER",
                                grow: 1.0,
                                font: FontToken::Mono,
                                font_size: 9,
                                text_color: ColorToken::OnSurface,
                                paragraph: left_label()
                            )
                        }
                        Row (height: 30, align: AlignItems::Center, column_gap: 9) {
                            View (width: 3, height: 18, bg_color: ColorToken::Secondary)
                            Text (
                                "COMMAND CHANNEL",
                                grow: 1.0,
                                font: FontToken::Mono,
                                font_size: 9,
                                text_color: ColorToken::OnSurface,
                                paragraph: left_label()
                            )
                        }
                        Row (height: 30, align: AlignItems::Center, column_gap: 9) {
                            View (width: 3, height: 18, bg_color: ColorToken::Tertiary)
                            Text (
                                "MIRUI OWNER THREAD",
                                grow: 1.0,
                                font: FontToken::Mono,
                                font_size: 9,
                                text_color: ColorToken::OnSurface,
                                paragraph: left_label()
                            )
                        }
                        View (height: 1, bg_color: ColorToken::Outline)
                        Text (
                            "CHANNEL IDLE",
                            id: "bridge_state",
                            height: 22,
                            font: FontToken::Mono,
                            font_size: 9,
                            text_color: ColorToken::OnSurfaceVariant,
                            paragraph: left_label()
                        )
                        Text (
                            "Thread-affine UI state never crosses the boundary. The VM only retains an opaque lease.",
                            id: "boundary_note",
                            grow: 1.0,
                            font_size: 10,
                            text_color: ColorToken::OnSurfaceVariant
                        )
                    }
                }
                Row (height: 24, align: AlignItems::Center) {
                    Text (
                        "atman-rt  /  mirui 0.46.3  /  SDL2",
                        grow: 1.0,
                        font: FontToken::Mono,
                        font_size: 8,
                        text_color: ColorToken::OnSurfaceVariant,
                        paragraph: left_label()
                    )
                    Text (
                        "ESC TO CLOSE",
                        font: FontToken::Mono,
                        font_size: 8,
                        text_color: ColorToken::OnSurfaceVariant
                    )
                }
            }
        };

        Ok(Self {
            root,
            canvas: find(world, "runtime_canvas")?,
            title: find(world, "app_title")?,
            subtitle: find(world, "app_subtitle")?,
            status: find(world, "run_status")?,
            event_text: find(world, "event_text")?,
            bridge_state: find(world, "bridge_state")?,
            boundary_note: find(world, "boundary_note")?,
            metric_labels: [
                find(world, "metric_label_1")?,
                find(world, "metric_label_2")?,
                find(world, "metric_label_3")?,
            ],
            metric_values: [
                find(world, "metric_value_1")?,
                find(world, "metric_value_2")?,
                find(world, "metric_value_3")?,
            ],
            stages: [
                find(world, "stage_1")?,
                find(world, "stage_2")?,
                find(world, "stage_3")?,
            ],
            stage_labels: [
                find(world, "stage_label_1")?,
                find(world, "stage_label_2")?,
                find(world, "stage_label_3")?,
            ],
            links: [find(world, "link_1")?, find(world, "link_2")?],
            progress: find(world, "progress")?,
        })
    }

    pub(crate) fn configure(self, world: &mut World, title: String, subtitle: String) {
        set_text(world, self.title, title);
        set_text(world, self.subtitle, subtitle);
        world.mark_subtree_dirty(self.root);
    }

    pub(crate) fn status(
        self,
        world: &mut World,
        text: String,
        tone: String,
    ) -> Result<(), String> {
        let color = match tone.as_str() {
            "active" => ColorToken::Primary,
            "success" => ColorToken::Success,
            "muted" => ColorToken::OnSurfaceVariant,
            _ => return Err(format!("unknown status tone `{tone}`")),
        };
        set_text(world, self.status, text);
        set_text_color(world, self.status, color);
        world.mark_subtree_dirty(self.root);
        Ok(())
    }

    pub(crate) fn metric(
        self,
        world: &mut World,
        slot: i64,
        label: String,
        value: String,
    ) -> Result<(), String> {
        let index = usize::try_from(slot - 1).map_err(|_| format!("invalid metric slot {slot}"))?;
        let label_entity = *self
            .metric_labels
            .get(index)
            .ok_or_else(|| format!("invalid metric slot {slot}"))?;
        let value_entity = *self
            .metric_values
            .get(index)
            .ok_or_else(|| format!("invalid metric slot {slot}"))?;
        set_text(world, label_entity, label);
        set_text(world, value_entity, value);
        world.mark_subtree_dirty(self.root);
        Ok(())
    }

    pub(crate) fn stage(
        self,
        world: &mut World,
        index: i64,
        label: String,
        detail: String,
        accent: String,
        progress_value: f64,
    ) -> Result<(), String> {
        let position = usize::try_from(index - 1).map_err(|_| format!("invalid stage {index}"))?;
        let stage = *self
            .stages
            .get(position)
            .ok_or_else(|| format!("invalid stage {index}"))?;
        let stage_label = *self
            .stage_labels
            .get(position)
            .ok_or_else(|| format!("invalid stage {index}"))?;
        let (fill, on_fill, canvas) = accent_tokens(&accent)?;

        set_text(world, stage_label, label);
        set_text(world, self.event_text, detail);
        set_stage(world, stage, fill, on_fill);
        set_canvas_color(world, self.canvas, canvas);
        if let Some(link) = position
            .checked_sub(1)
            .and_then(|index| self.links.get(index))
        {
            set_canvas_color(world, *link, fill);
        }
        if let Some(progress) = world.get_mut::<ProgressBar>(self.progress) {
            progress.value = progress_value as f32;
            progress.fill_color = fill.into();
        }
        world.invalidate_visual(self.progress);
        world.mark_subtree_dirty(self.root);
        Ok(())
    }

    pub(crate) fn boundary(self, world: &mut World, state: String, note: String) {
        set_text(world, self.bridge_state, state);
        set_text(world, self.boundary_note, note);
        world.mark_subtree_dirty(self.root);
    }
}

fn accent_tokens(accent: &str) -> Result<(ColorToken, ColorToken, ColorToken), String> {
    match accent {
        "sage" => Ok((ColorToken::Primary, ColorToken::OnPrimary, CANVAS_ACQUIRE)),
        "steel" => Ok((
            ColorToken::Secondary,
            ColorToken::OnSecondary,
            CANVAS_MUTATE,
        )),
        "mauve" => Ok((ColorToken::Tertiary, ColorToken::OnTertiary, CANVAS_PRESENT)),
        _ => Err(format!("unknown dashboard accent `{accent}`")),
    }
}

fn find(world: &World, id: &'static str) -> Result<Entity, String> {
    world
        .find_by_id(id)
        .ok_or_else(|| format!("dashboard node `{id}` is missing"))
}

fn left_label() -> ParagraphStyle {
    ParagraphStyle::label().with_align(TextAlign::Start)
}

fn set_text(world: &mut World, entity: Entity, content: impl Into<String>) {
    if let Some(text) = world.get_mut::<Text>(entity) {
        text.set_content(content.into());
    }
    world.invalidate(entity);
}

fn set_text_color(world: &mut World, entity: Entity, color: ColorToken) {
    if let Some(style) = world.get_mut::<Style>(entity) {
        style.set_text_color(color);
    }
    world.invalidate_visual(entity);
}

fn set_canvas_color(world: &mut World, entity: Entity, color: ColorToken) {
    if let Some(style) = world.get_mut::<Style>(entity) {
        style.set_bg_color(color);
    }
    world.invalidate_visual(entity);
}

fn set_stage(world: &mut World, entity: Entity, bg: ColorToken, text: ColorToken) {
    if let Some(style) = world.get_mut::<Style>(entity) {
        style.set_bg_color(bg).set_text_color(text);
    }
    world.invalidate_visual(entity);
}
