//! A dialog that browses and edits a large value without ever loading all of it.
//!
//! For a `jsonb` column this is a lazy tree: only the node currently in view and one page of its
//! children are ever fetched (see `pgcore::jsontree`). For any other large-capable column (`text`,
//! `bytea`, plain `json`, ...) there is no sub-structure to browse, so it is a single capped read
//! of the whole value. Both share one model: "you are looking at one node; if it has children you
//! can navigate into one; if it has text you can view and edit that text" — the jsonb root and a
//! plain column are simply the case with no children.
//!
//! Every write goes through `jsontree::set_at_path` / `delete_at_path`, guarded by the row's
//! `xmin`: if the row changed since the value was opened, nothing is written.

use crate::runtime;
use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pgcore::config::ConnectionParams;
use pgcore::data::{self, CellFetch, Column};
use pgcore::jsontree::{self, ChildKey, NodeInfo, NodeKind, NodeLookup, PathSegment};
use pgcore::{Session, UserFacingError};

/// A large value never seen before is read up to this many bytes; past it, the viewer says how big
/// it is instead of trying to load it. Applies to a jsonb leaf/subtree and to a whole plain value.
const VIEW_CAP: usize = 4 * 1024 * 1024;
const CHILD_PAGE: usize = jsontree::CHILD_PAGE_SIZE;

/// One step of a dev-only navigation path (see `data_view`'s `PGB_VIEW` hook).
pub enum DevPathStep {
    Key(String),
    Index(i64),
}

/// What the viewer is pointed at.
#[derive(Clone)]
pub struct Target {
    pub params: ConnectionParams,
    pub schema: String,
    pub table: String,
    pub columns: Vec<Column>,
    pub key: Vec<(String, String)>,
    pub column: String,
}

enum Load {
    Loading,
    Failed(UserFacingError),
    Ready,
}

/// The current node's own value, independent of whether it has children.
enum TextState {
    /// A container: nothing to show as text (see `children` instead).
    None,
    Inline(String),
    NotLoaded { total_or_stored_bytes: i64 },
    Loading,
    TooLarge { total_bytes: i64 },
}

pub struct ValueViewer {
    target: Target,
    is_jsonb: bool,
    editable: bool,
    path: Vec<PathSegment>,
    /// Display labels, same length as `path`.
    crumbs: Vec<String>,
    xmin: Option<String>,

    load: Load,
    info: Option<NodeInfo>,
    text: TextState,
    children: Vec<jsontree::Child>,
    children_has_more: bool,
    loading_more: bool,

    editing: bool,
    edit_input: Entity<TextareaState>,
    saving: bool,
    banner: Option<(UserFacingError, bool)>,
    generation: u64,
}

impl ValueViewer {
    pub fn open(target: Target, editable: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let is_jsonb = target.columns.iter().any(|c| c.name == target.column && c.type_name == "jsonb");
        let edit_input = cx.new(|cx| TextareaState::new(window, cx));
        let mut this = Self {
            target,
            is_jsonb,
            editable,
            path: Vec::new(),
            crumbs: Vec::new(),
            xmin: None,
            load: Load::Loading,
            info: None,
            text: TextState::None,
            children: Vec::new(),
            children_has_more: false,
            loading_more: false,
            editing: false,
            edit_input,
            saving: false,
            banner: None,
            generation: 0,
        };
        this.reload(window, cx);
        this
    }

    fn session_target(&self) -> (ConnectionParams, String, String, Vec<Column>, Vec<(String, String)>, String) {
        let t = &self.target;
        (t.params.clone(), t.schema.clone(), t.table.clone(), t.columns.clone(), t.key.clone(), t.column.clone())
    }

    /// Re-reads the current node (and the row's `xmin`) from scratch: after navigating, after a
    /// successful write (which always changes `xmin`), and on first open.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.load = Load::Loading;
        self.editing = false;
        self.children.clear();
        cx.notify();

        let (params, schema, table, columns, key, column) = self.session_target();
        let is_jsonb = self.is_jsonb;
        let path = self.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<PlainOrJson, UserFacingError> = runtime::run(async move {
                let session = Session::connect(&params).await?;
                let xmin = jsontree::read_xmin(&session, &schema, &table, &columns, &key).await?;
                if xmin.is_none() {
                    return Ok((None, None, Vec::new(), false, None));
                }
                if is_jsonb {
                    match jsontree::describe_node(&session, &schema, &table, &columns, &key, &column, &path).await? {
                        NodeLookup::RowMissing => Ok((None, None, Vec::new(), false, None)),
                        NodeLookup::Missing => Ok((xmin, None, Vec::new(), false, None)),
                        NodeLookup::Found(info) if info.kind.is_container() => {
                            let (children, more) = jsontree::list_children(
                                &session, &schema, &table, &columns, &key, &column, &path, info.kind, 0, CHILD_PAGE,
                            )
                            .await?;
                            Ok((xmin, Some(info), children, more, None))
                        }
                        NodeLookup::Found(info) => Ok((xmin, Some(info), Vec::new(), false, None)),
                    }
                } else {
                    // A plain large-capable column (text/bytea/json/...): no tree, just the root.
                    let fetch = data::fetch_cell(&session, &schema, &table, &columns, &key, &column, VIEW_CAP).await?;
                    match fetch {
                        CellFetch::RowMissing => Ok((None, None, Vec::new(), false, None)),
                        CellFetch::Null => Ok((xmin, None, Vec::new(), false, None)),
                        full_or_too_large => {
                            let info = NodeInfo { kind: NodeKind::String, stored_bytes: 0, count: None, preview: None };
                            Ok((xmin, Some(info), Vec::new(), false, Some(full_or_too_large)))
                        }
                    }
                }
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.apply_load(result, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn apply_load(
        &mut self,
        result: Result<PlainOrJson, UserFacingError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Err(err) => self.load = Load::Failed(err),
            Ok((xmin, info, children, more, plain_fetch)) => {
                self.xmin = xmin.clone();
                self.info = info;
                self.children = children;
                self.children_has_more = more;
                self.load = Load::Ready;
                self.text = match (&self.info, plain_fetch) {
                    (None, _) => TextState::None,
                    (Some(info), _) if info.kind.is_container() => TextState::None,
                    (Some(_), Some(fetch)) => text_state_from_fetch(fetch),
                    (Some(info), None) => match &info.preview {
                        Some(p) => TextState::Inline(p.clone()),
                        None => TextState::NotLoaded { total_or_stored_bytes: info.stored_bytes },
                    },
                };
                if xmin.is_none() {
                    self.banner = Some((
                        UserFacingError::config("Row not found", "This row no longer exists."),
                        false,
                    ));
                }
            }
        }
        let _ = window;
        cx.notify();
    }

    // ---- navigation --------------------------------------------------------------------------

    fn open_child(&mut self, child: &jsontree::Child, window: &mut Window, cx: &mut Context<Self>) {
        let (segment, label) = match &child.key {
            ChildKey::Key(k) => (PathSegment::Key(k.clone()), k.clone()),
            ChildKey::Index(i) => (PathSegment::Index(*i), format!("[{i}]")),
        };
        self.path.push(segment);
        self.crumbs.push(label);
        self.reload(window, cx);
    }

    /// Dev-only (see `data_view`'s `PGB_VIEW` hook): navigates as if the matching child row had
    /// been clicked, without needing that row to already be loaded in `self.children`.
    pub fn dev_open_child(&mut self, step: &DevPathStep, window: &mut Window, cx: &mut Context<Self>) {
        let (segment, label) = match step {
            DevPathStep::Key(k) => (PathSegment::Key(k.clone()), k.clone()),
            DevPathStep::Index(i) => (PathSegment::Index(*i), format!("[{i}]")),
        };
        self.path.push(segment);
        self.crumbs.push(label);
        self.reload(window, cx);
    }

    /// Dev-only: opens the editor for the current node, as if its Edit button had been clicked.
    pub fn dev_begin_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_edit(window, cx);
    }


    fn go_to_crumb(&mut self, depth: usize, window: &mut Window, cx: &mut Context<Self>) {
        if depth >= self.path.len() {
            return;
        }
        self.path.truncate(depth);
        self.crumbs.truncate(depth);
        self.reload(window, cx);
    }

    fn load_more_children(&mut self, cx: &mut Context<Self>) {
        if self.loading_more || !self.children_has_more {
            return;
        }
        self.loading_more = true;
        cx.notify();
        let (params, schema, table, columns, key, column) = self.session_target();
        let path = self.path.clone();
        let kind = self.info.as_ref().map(|i| i.kind).unwrap_or(NodeKind::Object);
        let offset = self.children.len();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                let session = Session::connect(&params).await?;
                jsontree::list_children(&session, &schema, &table, &columns, &key, &column, &path, kind, offset, CHILD_PAGE).await
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading_more = false;
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok((mut more_children, has_more)) => {
                        this.children.append(&mut more_children);
                        this.children_has_more = has_more;
                    }
                    Err(err) => this.banner = Some((err, false)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- viewing / editing the current node's text -----------------------------------------

    /// Loads the current node's text under the cap (used for a leaf whose preview was too big to
    /// inline, and to prefill the editor for a container being replaced wholesale).
    fn load_text(&mut self, cx: &mut Context<Self>) {
        self.text = TextState::Loading;
        cx.notify();
        let (params, schema, table, columns, key, column) = self.session_target();
        let is_jsonb = self.is_jsonb;
        let path = self.path.clone();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                let session = Session::connect(&params).await?;
                if is_jsonb {
                    jsontree::read_node_text(&session, &schema, &table, &columns, &key, &column, &path, VIEW_CAP).await
                } else {
                    data::fetch_cell(&session, &schema, &table, &columns, &key, &column, VIEW_CAP).await
                }
            })
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(fetch) => this.text = text_state_from_fetch(fetch),
                    Err(err) => {
                        this.text = TextState::None;
                        this.banner = Some((err, false));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn begin_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = match &self.text {
            TextState::Inline(t) => t.clone(),
            _ => return, // Load is shown instead of Edit until the text is available.
        };
        self.banner = None;
        self.editing = true;
        self.edit_input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editing = false;
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(xmin) = self.xmin.clone() else { return };
        if self.saving {
            return;
        }
        let new_text = self.edit_input.read(cx).value().to_string();
        self.saving = true;
        self.banner = None;
        cx.notify();

        let (params, schema, table, columns, key, column) = self.session_target();
        let path = self.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = runtime::run(async move {
                let session = Session::connect(&params).await?;
                jsontree::set_at_path(&session, &schema, &table, &columns, &key, &column, &path, &new_text, &xmin)
                    .await
                    .map_err(UserFacingError::from)
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.editing = false;
                        this.banner = Some((UserFacingError::config("Saved", "The value was updated."), true));
                        this.reload(window, cx);
                    }
                    Err(err) => this.banner = Some((err, false)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path.is_empty() {
            return;
        }
        let label = self.crumbs.last().cloned().unwrap_or_default();
        let entity = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let entity = entity.clone();
            let label = label.clone();
            alert
                .icon(Icon::new(IconName::TriangleAlert).text_color(gpui_kit::red()))
                .title("Delete this value?")
                .description(format!("\"{label}\" will be removed. This can't be undone from here."))
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.do_delete(window, cx));
                    true
                })
        });
    }

    fn do_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(xmin) = self.xmin.clone() else { return };
        self.saving = true;
        cx.notify();
        let (params, schema, table, columns, key, column) = self.session_target();
        let path = self.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = runtime::run(async move {
                let session = Session::connect(&params).await?;
                jsontree::delete_at_path(&session, &schema, &table, &columns, &key, &column, &path, &xmin)
                    .await
                    .map_err(UserFacingError::from)
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        // The node under us is gone: step back to its parent.
                        this.path.pop();
                        this.crumbs.pop();
                        this.reload(window, cx);
                    }
                    Err(err) => {
                        this.banner = Some((err, false));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- rendering ---------------------------------------------------------------------------

    fn render_breadcrumbs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut row = h_flex().gap_1().items_center().flex_wrap();
        let root_label = self.target.column.clone();
        row = row.child(
            Button::new("crumb-root")
                .ghost()
                .xsmall()
                .label(root_label)
                .disabled(self.path.is_empty())
                .on_click(cx.listener(|this, _, window, cx| this.go_to_crumb(0, window, cx))),
        );
        for (depth, label) in self.crumbs.iter().enumerate() {
            row = row
                .child(Icon::new(IconName::ChevronRight).size_3().text_color(theme.muted_foreground))
                .child(
                    Button::new(("crumb", depth))
                        .ghost()
                        .xsmall()
                        .label(label.clone())
                        .disabled(depth + 1 == self.crumbs.len())
                        .on_click(cx.listener(move |this, _, window, cx| this.go_to_crumb(depth + 1, window, cx))),
                );
        }
        row
    }

    fn render_children(&self, info: &NodeInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let count = info.count.map(|n| crate::data_view::format_count(n)).unwrap_or_default();
        let noun = if info.kind == NodeKind::Array { "elements" } else { "keys" };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{count} {noun}")),
            )
            .child(
                v_flex()
                    .id("children")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().children(self.children.iter().enumerate().map(|(ix, child)| {
                        let label = match &child.key {
                            ChildKey::Key(k) => k.clone(),
                            ChildKey::Index(i) => format!("[{i}]"),
                        };
                        let is_container = child.info.kind.is_container();
                        h_flex()
                            .id(("child", ix))
                            .h(px(28.))
                            .px_3()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.list_hover))
                            .on_click(cx.listener({
                                let child = child.clone();
                                move |this, _, window, cx| this.open_child(&child, window, cx)
                            }))
                            .child(
                                Icon::new(node_icon(child.info.kind))
                                    .size_3()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(div().flex_shrink_0().child(label))
                            .child(div().flex_1().min_w_0().truncate().text_color(theme.muted_foreground).child(
                                child.info.preview.clone().unwrap_or_else(|| match child.info.kind {
                                    NodeKind::Object => format!("{{…}} {}", crate::data_view::format_bytes(child.info.stored_bytes)),
                                    NodeKind::Array => format!("[…] {}", crate::data_view::format_bytes(child.info.stored_bytes)),
                                    _ => crate::data_view::format_bytes(child.info.stored_bytes),
                                }),
                            ))
                            .when(is_container, |row| {
                                row.child(Icon::new(IconName::ChevronRight).size_3().text_color(theme.muted_foreground))
                            })
                    })))
                    .when(self.children_has_more, |v| {
                        v.child(
                            div().p_2().child(
                                Button::new("load-more")
                                    .ghost()
                                    .small()
                                    .label(if self.loading_more { "Loading…" } else { "Load more" })
                                    .loading(self.loading_more)
                                    .disabled(self.loading_more)
                                    .on_click(cx.listener(|this, _, _, cx| this.load_more_children(cx))),
                            ),
                        )
                    }),
            )
    }

    fn render_text_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        match &self.text {
            TextState::None => div().into_any_element(),
            TextState::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Icon::new(IconName::LoaderCircle).size_5().text_color(theme.muted_foreground))
                .into_any_element(),
            TextState::TooLarge { total_bytes } => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_1()
                .p_4()
                .child(Icon::new(IconName::TriangleAlert).size_5().text_color(theme.muted_foreground))
                .child(div().child(format!("{} — too large to load here.", crate::data_view::format_bytes(*total_bytes))))
                .when(self.is_jsonb, |v| {
                    v.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Browse into it below instead of loading the whole thing."),
                    )
                })
                .into_any_element(),
            TextState::NotLoaded { total_or_stored_bytes } => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(div().text_color(theme.muted_foreground).child(format!(
                    "{} — not loaded yet.",
                    crate::data_view::format_bytes(*total_or_stored_bytes)
                )))
                .child(
                    Button::new("load-value")
                        .outline()
                        .small()
                        .label(format!("Load (up to {})", crate::data_view::format_bytes(VIEW_CAP as i64)))
                        .on_click(cx.listener(|this, _, _, cx| this.load_text(cx))),
                )
                .into_any_element(),
            TextState::Inline(text) => {
                if self.editing {
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_2()
                        .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                            if ev.keystroke.key == "escape" {
                                this.cancel_edit(cx);
                            }
                        }))
                        .child(
                            Textarea::new(&self.edit_input)
                                .h(px(320.))
                                .font_family(theme.mono_font_family.clone()),
                        )
                        .into_any_element()
                } else {
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .id("text-view")
                        .overflow_y_scroll()
                        .p_2()
                        .font_family(theme.mono_font_family.clone())
                        .text_sm()
                        .child(text.clone())
                        .into_any_element()
                }
            }
        }
    }
}

impl Render for ValueViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_container = self.info.as_ref().is_some_and(|i| i.kind.is_container());
        let has_text = matches!(self.text, TextState::Inline(_));
        let can_edit = self.editable && !self.saving && (has_text || matches!(self.text, TextState::None) && is_container);
        let can_delete = self.editable && !self.saving && !self.path.is_empty() && self.xmin.is_some();

        let toolbar = h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(self.render_breadcrumbs(cx))
            .child(div().flex_1())
            .when(!self.editable, |bar| {
                bar.child(Icon::new(IconName::Lock).size_3().text_color(theme.muted_foreground))
                    .child(div().text_xs().text_color(theme.muted_foreground).child("Read-only"))
            })
            .when(self.editing, |bar| {
                bar.child(Button::new("cancel").ghost().small().label("Cancel").disabled(self.saving).on_click(
                    cx.listener(|this, _, _, cx| this.cancel_edit(cx)),
                ))
                .child(
                    Button::new("save")
                        .primary()
                        .small()
                        .label(if self.saving { "Saving…" } else { "Save" })
                        .loading(self.saving)
                        .disabled(self.saving)
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                )
            })
            .when(!self.editing, |bar| {
                bar.when(can_delete, |bar| {
                    bar.child(
                        Button::new("delete-node")
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Trash))
                            .label("Delete")
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx))),
                    )
                })
                .when(can_edit && !is_container, |bar| {
                    bar.child(
                        Button::new("edit-node")
                            .outline()
                            .small()
                            .label("Edit")
                            .on_click(cx.listener(|this, _, window, cx| this.begin_edit(window, cx))),
                    )
                })
                .when(can_edit && is_container && !has_text, |bar| {
                    bar.child(
                        Button::new("edit-whole")
                            .outline()
                            .small()
                            .label("Replace whole value…")
                            .on_click(cx.listener(|this, _, _, cx| this.load_text(cx))),
                    )
                })
                .when(can_edit && is_container && has_text, |bar| {
                    bar.child(
                        Button::new("edit-whole")
                            .outline()
                            .small()
                            .label("Edit whole value")
                            .on_click(cx.listener(|this, _, window, cx| this.begin_edit(window, cx))),
                    )
                })
            });

        let banner = self.banner.as_ref().map(|(err, is_info)| {
            let accent = if *is_info { theme.success } else { theme.danger };
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_start()
                .bg(accent.opacity(0.14))
                .border_b_1()
                .border_color(accent)
                .child(
                    Icon::new(if *is_info { IconName::CircleCheck } else { IconName::TriangleAlert })
                        .size_4()
                        .text_color(accent),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_semibold().child(err.title.clone()))
                        .child(div().text_sm().child(err.detail.clone()))
                        .when_some(err.hint.clone(), |c, hint| {
                            c.child(div().text_sm().text_color(theme.muted_foreground).child(hint))
                        }),
                )
        });

        let body = match &self.load {
            Load::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(Icon::new(IconName::LoaderCircle).size_6().text_color(theme.muted_foreground))
                .into_any_element(),
            Load::Failed(err) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_4()
                .gap_1()
                .child(Icon::new(IconName::TriangleAlert).size_5().text_color(theme.danger))
                .child(div().font_semibold().child(err.title.clone()))
                .child(div().text_sm().text_color(theme.muted_foreground).child(err.detail.clone()))
                .into_any_element(),
            Load::Ready => match &self.info {
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(div().text_color(theme.muted_foreground).italic().child("NULL"))
                    .into_any_element(),
                Some(info) if info.kind.is_container() && !self.editing => {
                    self.render_children(info, cx).into_any_element()
                }
                Some(_) => self.render_text_area(cx).into_any_element(),
            },
        };
        let _ = window;

        v_flex()
            .size_full()
            .child(toolbar)
            .children(banner)
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// `(xmin, node info, first page of children, has more, raw fetch for a plain column)`.
type PlainOrJson = (Option<String>, Option<NodeInfo>, Vec<jsontree::Child>, bool, Option<CellFetch>);

fn text_state_from_fetch(fetch: CellFetch) -> TextState {
    match fetch {
        CellFetch::Full(t) => TextState::Inline(t),
        CellFetch::TooLarge { total_bytes } => TextState::TooLarge { total_bytes },
        CellFetch::Null | CellFetch::RowMissing => TextState::None,
    }
}

fn node_icon(kind: NodeKind) -> IconName {
    match kind {
        NodeKind::Object => IconName::Braces,
        NodeKind::Array => IconName::Brackets,
        NodeKind::String => IconName::Quote,
        _ => IconName::Hash,
    }
}
