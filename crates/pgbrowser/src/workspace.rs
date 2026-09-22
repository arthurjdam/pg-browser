//! The main window: title bar, connection sidebar (schema/table tree), content and status bar.

use crate::data_view::{DataView, DataViewEvent, Initial};
use crate::runtime;
use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, TitleBar};
use gpui_kit::*;
use pgcore::catalog::{self, Relation, RelationKind, Schema};
use pgcore::config::{ConnectionParams, parse_conninfo};
use pgcore::data::{Filter, Sort};
use pgcore::{ServerInfo, Session, UserFacingError};

enum Connection {
    Idle,
    Connecting,
    Failed(UserFacingError),
    Connected(Connected),
}

struct Connected {
    /// Kept so every table tab can open its own connection.
    params: ConnectionParams,
    session: Session,
    schemas: Vec<SchemaNode>,
}

struct SchemaNode {
    schema: Schema,
    expanded: bool,
    relations: Relations,
}

enum Relations {
    NotLoaded,
    Loading,
    Loaded(Vec<Relation>),
    Failed(UserFacingError),
}

pub struct Workspace {
    url_input: Entity<InputState>,
    connection: Connection,
    /// Open tables, one connection each.
    tabs: Vec<Tab>,
    active_tab: usize,
}

struct Tab {
    view: Entity<DataView>,
    _subscription: Subscription,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("postgresql://user:password@host:5432/database")
        });
        if let Ok(url) = std::env::var("PGB_URL") {
            url_input.update(cx, |input, cx| input.set_value(url, window, cx));
        }
        cx.subscribe_in(&url_input, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.connect(window, cx);
            }
        })
        .detach();
        let mut this = Self { url_input, connection: Connection::Idle, tabs: Vec::new(), active_tab: 0 };
        // Dev convenience: `PGB_URL=... PGB_AUTOCONNECT=1 cargo run` connects on startup.
        if std::env::var_os("PGB_AUTOCONNECT").is_some() {
            this.connect(window, cx);
        }
        this
    }

    /// Connects using the URL in the input box, then loads the schema list.
    pub fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.connection, Connection::Connecting) {
            return;
        }
        let text = self.url_input.read(cx).value().to_string();
        let params = match parse_conninfo(&text) {
            Ok(info) => info.params,
            Err(err) => {
                self.connection = Connection::Failed(UserFacingError::config(
                    "Can't read this connection string",
                    err.to_string(),
                ));
                cx.notify();
                return;
            }
        };
        self.connection = Connection::Connecting;
        self.tabs.clear();
        self.active_tab = 0;
        cx.notify();

        let kept_params = params.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = runtime::run(async move {
                let session = Session::connect(&params).await?;
                let schemas = catalog::list_schemas(&session).await?;
                Ok((session, schemas))
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.connection = match result {
                    Ok((session, schemas)) => Connection::Connected(Connected {
                        params: kept_params,
                        session,
                        schemas: schemas
                            .into_iter()
                            .map(|schema| SchemaNode {
                                expanded: false,
                                schema,
                                relations: Relations::NotLoaded,
                            })
                            .collect(),
                    }),
                    Err(err) => Connection::Failed(err),
                };
                // Dev convenience: `PGB_EXPAND=shop,secret` opens those schemas after connecting.
                if let Ok(names) = std::env::var("PGB_EXPAND") {
                    let wanted: Vec<usize> = match &this.connection {
                        Connection::Connected(c) => c
                            .schemas
                            .iter()
                            .enumerate()
                            .filter(|(_, n)| names.split(',').any(|w| w == n.schema.name))
                            .map(|(ix, _)| ix)
                            .collect(),
                        _ => Vec::new(),
                    };
                    for ix in wanted {
                        this.toggle_schema(ix, cx);
                    }
                }
                // Dev convenience: `PGB_OPEN=shop.customers` opens that table after connecting.
                if let Some((schema, table)) = std::env::var("PGB_OPEN").ok().as_deref().and_then(|v| v.split_once('.')) {
                    this.open_table(schema.to_string(), table.to_string(), None, initial_from_env(), window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens `schema.table` in a tab, re-using an existing tab that shows exactly the same rows.
    pub fn open_table(
        &mut self,
        schema: String,
        table: String,
        estimate: Option<i64>,
        initial: Initial,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Connection::Connected(conn) = &self.connection else { return };
        let same = self.tabs.iter().position(|t| {
            let v = t.view.read(cx);
            v.schema() == schema && v.table() == table && *v.filter() == initial.query.filter
        });
        if let Some(ix) = same {
            self.active_tab = ix;
            cx.notify();
            return;
        }
        let params = conn.params.clone();
        let view = cx.new(|cx| DataView::open(params, schema, table, estimate, initial, window, cx));
        let subscription = cx.subscribe_in(&view, window, |this, _, event: &DataViewEvent, window, cx| {
            let DataViewEvent::OpenTable { schema, table, filter } = event;
            let estimate = this.estimate_for(schema, table);
            let initial = Initial {
                query: pgcore::data::Query { sort: None, filter: filter.clone() },
                ..Initial::default()
            };
            this.open_table(schema.clone(), table.clone(), estimate, initial, window, cx);
        });
        self.tabs.push(Tab { view, _subscription: subscription });
        self.active_tab = self.tabs.len() - 1;
        cx.notify();
    }

    /// The planner's row estimate for a relation, if the navigator has loaded its schema.
    fn estimate_for(&self, schema: &str, table: &str) -> Option<i64> {
        let Connection::Connected(conn) = &self.connection else { return None };
        conn.schemas
            .iter()
            .find(|n| n.schema.name == schema)
            .and_then(|n| match &n.relations {
                Relations::Loaded(rels) => rels.iter().find(|r| r.name == table).map(|r| r.estimated_rows),
                _ => None,
            })
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        // Never discard staged edits silently: show the tab and say why it stays open.
        let view = self.tabs[ix].view.clone();
        if view.read(cx).is_dirty() {
            self.active_tab = ix;
            view.update(cx, |v, cx| v.warn_unsaved(cx));
            cx.notify();
            return;
        }
        self.tabs.remove(ix);
        if ix < self.active_tab {
            self.active_tab -= 1;
        }
        self.active_tab = self.active_tab.min(self.tabs.len().saturating_sub(1));
        cx.notify();
    }

    pub fn toggle_schema(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Connection::Connected(conn) = &mut self.connection else { return };
        let Some(node) = conn.schemas.get_mut(ix) else { return };
        node.expanded = !node.expanded;
        if node.expanded && matches!(node.relations, Relations::NotLoaded | Relations::Failed(_)) {
            node.relations = Relations::Loading;
            let session = conn.session.clone();
            let name = node.schema.name.clone();
            cx.spawn(async move |this, cx| {
                let schema = name.clone();
                let result = runtime::run(async move { catalog::list_relations(&session, &schema).await }).await;
                this.update(cx, |this, cx| {
                    if let Connection::Connected(conn) = &mut this.connection {
                        if let Some(node) = conn.schemas.iter_mut().find(|n| n.schema.name == name) {
                            node.relations = match result {
                                Ok(rels) => Relations::Loaded(rels),
                                Err(err) => Relations::Failed(err),
                            };
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let connecting = matches!(self.connection, Connection::Connecting);
        v_flex()
            .w(px(300.))
            .flex_shrink_0()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(Input::new(&self.url_input))
                    .child(
                        Button::new("connect")
                            .primary()
                            .small()
                            .label(if connecting { "Connecting…" } else { "Connect" })
                            .loading(connecting)
                            .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
                    ),
            )
            .child(self.render_tree(cx))
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Connection::Connected(conn) = &self.connection else {
            return div().into_any_element();
        };
        let mut rows = v_flex().id("navigator").flex_1().min_h_0().overflow_y_scroll().px_2().pb_2();
        for (ix, node) in conn.schemas.iter().enumerate() {
            let dim = node.schema.is_system || !node.schema.can_use;
            rows = rows.child(
                h_flex()
                    .id(("schema", ix))
                    .h(px(26.))
                    .px_1()
                    .gap_1p5()
                    .rounded(px(4.))
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.sidebar_accent))
                    .text_color(if dim { theme.muted_foreground } else { theme.sidebar_foreground })
                    .child(
                        Icon::new(if node.expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                            .size_3(),
                    )
                    .child(Icon::new(IconName::Folder).size_4())
                    .child(div().flex_1().truncate().child(node.schema.name.clone()))
                    .when(!node.schema.can_use, |row| {
                        row.child(Icon::new(IconName::Lock).size_3().text_color(theme.muted_foreground))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_schema(ix, cx))),
            );
            if !node.expanded {
                continue;
            }
            match &node.relations {
                Relations::NotLoaded => {}
                Relations::Loading => {
                    rows = rows.child(
                        div().pl_8().h(px(24.)).text_sm().text_color(theme.muted_foreground).child("Loading…"),
                    );
                }
                Relations::Failed(err) => {
                    rows = rows.child(
                        div().pl_8().py_1().text_sm().text_color(theme.danger).child(err.title.clone()),
                    );
                }
                Relations::Loaded(rels) if rels.is_empty() => {
                    rows = rows.child(
                        div().pl_8().h(px(24.)).text_sm().text_color(theme.muted_foreground).child("No tables"),
                    );
                }
                Relations::Loaded(rels) => {
                    for (rix, rel) in rels.iter().enumerate() {
                        rows = rows.child(
                            h_flex()
                                .id(("relation", ix * 100_000 + rix))
                                .h(px(24.))
                                .pl_6()
                                .pr_1()
                                .gap_1p5()
                                .rounded(px(4.))
                                .items_center()
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.sidebar_accent))
                                .on_click({
                                    let (schema, name, est) = (rel.schema.clone(), rel.name.clone(), rel.estimated_rows);
                                    cx.listener(move |this, _, window, cx| {
                                        this.open_table(schema.clone(), name.clone(), Some(est), Initial::default(), window, cx)
                                    })
                                })
                                .text_color(if rel.can_select {
                                    theme.sidebar_foreground
                                } else {
                                    theme.muted_foreground
                                })
                                .child(Icon::new(relation_icon(rel.kind)).size_4().text_color(theme.muted_foreground))
                                .child(div().flex_1().truncate().child(rel.name.clone()))
                                .when(!rel.can_select, |row| {
                                    row.child(Icon::new(IconName::Lock).size_3().text_color(theme.muted_foreground))
                                })
                                .when(rel.can_select && rel.estimated_rows >= 0, |row| {
                                    row.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("~{}", compact_count(rel.estimated_rows))),
                                    )
                                }),
                        );
                    }
                }
            }
        }
        rows.into_any_element()
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .id("tabs")
            .h(px(34.))
            .flex_shrink_0()
            .overflow_x_scroll()
            .bg(theme.tab_bar)
            .border_b_1()
            .border_color(theme.border)
            .children(self.tabs.iter().enumerate().map(|(ix, tab)| {
                let view = tab.view.read(cx);
                let active = ix == self.active_tab;
                h_flex()
                    .id(("tab", ix))
                    .h_full()
                    .flex_shrink_0()
                    .pl_3()
                    .pr_1()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .bg(if active { theme.tab_active } else { theme.tab })
                    .text_color(if active { theme.tab_active_foreground } else { theme.tab_foreground })
                    .border_r_1()
                    .border_color(theme.border)
                    .child(Icon::new(IconName::Table2).size_3())
                    .child(div().text_sm().child(view.table().to_string()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(view.schema().to_string()))
                    .when(!view.filter().is_none(), |t| {
                        t.child(Icon::new(IconName::ListFilter).size_3().text_color(theme.primary))
                    })
                    .child(
                        Button::new(("close-tab", ix))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::X))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_tab = ix;
                        cx.notify();
                    }))
            }))
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let centered = || v_flex().size_full().items_center().justify_center().gap_2().p_8();
        match &self.connection {
            Connection::Idle => centered()
                .child(Icon::new(IconName::Database).size_8().text_color(theme.muted_foreground))
                .child(div().text_lg().child("Connect to a Postgres server"))
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Enter a connection URL in the sidebar and press Connect."),
                )
                .into_any_element(),
            Connection::Connecting => centered()
                .child(Icon::new(IconName::LoaderCircle).size_6().text_color(theme.muted_foreground))
                .child(div().text_color(theme.muted_foreground).child("Connecting…"))
                .into_any_element(),
            Connection::Failed(err) => centered().child(error_card(err, cx)).into_any_element(),
            Connection::Connected(_) if !self.tabs.is_empty() => v_flex()
                .size_full()
                .child(self.render_tabs(cx))
                .child(div().flex_1().min_h_0().child(self.tabs[self.active_tab].view.clone()))
                .into_any_element(),
            Connection::Connected(conn) => {
                let info = conn.session.info();
                centered()
                    .child(Icon::new(IconName::CircleCheck).size_8().text_color(theme.success))
                    .child(div().text_lg().child(format!("Connected to {}", info.database)))
                    .child(info_grid(info, cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Expand a schema in the sidebar to browse its tables."),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (dot, text) = match &self.connection {
            Connection::Idle => (theme.muted_foreground, "Not connected".to_string()),
            Connection::Connecting => (theme.warning, "Connecting…".to_string()),
            Connection::Failed(err) => (theme.danger, err.title.clone()),
            Connection::Connected(c) => {
                let i = c.session.info();
                (theme.success, format!("{}@{}  ·  {}", i.user, i.endpoint, i.database))
            }
        };
        h_flex()
            .h(px(24.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .items_center()
            .text_xs()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .text_color(theme.muted_foreground)
            .child(div().size(px(8.)).rounded_full().bg(dot))
            .child(div().child(text))
            .child(div().flex_1())
            .when_some(
                match &self.connection {
                    Connection::Connected(c) => Some(short_version(&c.session.info().version)),
                    _ => None,
                },
                |bar, v| bar.child(div().child(v)),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Icon::new(IconName::Database).size_4())
                        .child(div().text_sm().child("pg-browser")),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(div().flex_1().min_w_0().h_full().child(self.render_main(cx))),
            )
            .child(self.render_status_bar(cx))
    }
}

/// Dev hooks for reproducing UI states: `PGB_FILTER`, `PGB_MATCH=col=value`, `PGB_SORT=col[:desc]`,
/// `PGB_PAGE_SIZE`, `PGB_OFFSET`.
fn initial_from_env() -> Initial {
    let mut initial = Initial::default();
    if let Ok(f) = std::env::var("PGB_FILTER") {
        initial.query.filter = Filter::from_text(&f);
    }
    if let Some((col, value)) = std::env::var("PGB_MATCH").ok().as_deref().and_then(|m| m.split_once('=')) {
        initial.query.filter = Filter::Match(vec![(col.to_string(), value.to_string())]);
    }
    if let Ok(s) = std::env::var("PGB_SORT") {
        let (column, dir) = s.split_once(':').unwrap_or((&s, "asc"));
        initial.query.sort = Some(Sort { column: column.to_string(), descending: dir == "desc" });
    }
    if let Some(n) = std::env::var("PGB_PAGE_SIZE").ok().and_then(|v| v.parse().ok()) {
        initial.page_size = n;
    }
    if let Some(n) = std::env::var("PGB_OFFSET").ok().and_then(|v| v.parse().ok()) {
        initial.offset = n;
    }
    initial
}

fn relation_icon(kind: RelationKind) -> IconName {
    match kind {
        RelationKind::Table | RelationKind::PartitionedTable => IconName::Table2,
        RelationKind::View | RelationKind::MaterializedView => IconName::Eye,
        RelationKind::ForeignTable => IconName::Server,
    }
}

pub(crate) fn error_card(err: &UserFacingError, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .max_w(px(560.))
        .w_full()
        .p_4()
        .gap_2()
        .rounded(px(8.))
        .border_1()
        .border_color(theme.danger)
        .bg(theme.popover)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Icon::new(IconName::TriangleAlert).size_5().text_color(theme.danger))
                .child(div().font_semibold().child(err.title.clone())),
        )
        .child(div().text_sm().child(err.detail.clone()))
        .when_some(err.hint.clone(), |card, hint| {
            card.child(div().text_sm().text_color(theme.muted_foreground).child(hint))
        })
        .when_some(err.sqlstate.clone(), |card, code| {
            card.child(div().text_xs().text_color(theme.muted_foreground).child(format!("SQLSTATE {code}")))
        })
}

fn info_grid(info: &ServerInfo, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let row = |label: &'static str, value: String| {
        h_flex()
            .gap_3()
            .child(div().w(px(80.)).text_color(theme.muted_foreground).child(label))
            .child(div().child(value))
    };
    v_flex()
        .gap_1()
        .text_sm()
        .child(row("Server", short_version(&info.version)))
        .child(row("Database", info.database.clone()))
        .child(row("User", info.user.clone()))
        .child(row("Endpoint", info.endpoint.clone()))
}

/// `PostgreSQL 16.4 (Debian …) on aarch64 …` → `PostgreSQL 16.4`.
fn short_version(version: &str) -> String {
    version.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

/// 1_234 → `1.2k`, 2_500_000 → `2.5M`.
pub(crate) fn compact_count(n: i64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{}k", n / 1_000),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would glob-import gpui's own `#[test]` macro over the built-in one.
    use super::{compact_count, short_version};

    #[test]
    fn version_is_shortened() {
        assert_eq!(
            short_version("PostgreSQL 16.4 (Debian 16.4-1.pgdg120+2) on aarch64-unknown-linux-gnu, compiled by gcc"),
            "PostgreSQL 16.4"
        );
    }

    #[test]
    fn counts_are_compact() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_500), "1.5k");
        assert_eq!(compact_count(25_000), "25k");
        assert_eq!(compact_count(2_500_000), "2.5M");
    }
}
