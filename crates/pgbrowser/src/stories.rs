//! Dev-only component sandbox. `PGB_STORY=cells cargo run -p pgbrowser` opens a window showing the
//! data grid's cells in every state they can be in, side by side, instead of the workspace. It uses
//! the real `DataTable` and `Grid` delegate with fixture data, so it needs no database and shows
//! exactly what a tab would. Combine with `PGB_THEME` to review a theme, and with the screenshot
//! tooling (`script/story`) for before/after captures.
//!
//! Hover and click behaviour can't be staged here (the grid has no view behind it); everything
//! else that changes how a cell looks — values, staged edits, selection, editing — can.

use crate::data_view::{Editability, Grid, Pending};
use gpui_kit::base::v_flex;
use gpui_kit::component::input::InputState;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, StyledExt as _};
use gpui_kit::*;
use pgcore::catalog::ForeignKey;
use pgcore::data::{Cell, Column, Identity};
use pgcore::edit::NewValue;

/// The story named by `PGB_STORY`, or `None` if there is no such story.
pub fn open(name: &str, window: &mut Window, cx: &mut App) -> Option<AnyView> {
    match name {
        "cells" => Some(cx.new(|cx| CellStory::new(window, cx)).into()),
        _ => None,
    }
}

struct Case {
    title: &'static str,
    /// What the user should be able to tell from the cell, for judging whether it does.
    expect: &'static str,
    table: Entity<TableState<Grid>>,
    rows: usize,
}

pub struct CellStory {
    sections: Vec<(&'static str, Vec<Case>)>,
}

// ---- fixtures ------------------------------------------------------------------------------------

fn col(name: &str, ty: &str) -> Column {
    Column { name: name.into(), type_name: ty.into(), ..Default::default() }
}

fn columns() -> Vec<Column> {
    vec![
        Column { is_primary_key: true, not_null: true, identity: Identity::Always, ..col("id", "bigint") },
        Column { not_null: true, ..col("customer_id", "bigint") },
        Column { not_null: true, ..col("name", "text") },
        col("price", "numeric(10,2)"),
        col("notes", "text"),
        col("profile", "jsonb"),
        Column { not_null: true, has_default: true, ..col("placed_at", "timestamptz") },
        Column { generated: true, ..col("total", "numeric(12,2)") },
    ]
}

/// Column indexes, so cases read as `(row, NAME)`.
const ID: usize = 0;
const CUSTOMER: usize = 1;
const NAME: usize = 2;
const PRICE: usize = 3;
const NOTES: usize = 4;
const PLACED_AT: usize = 6;

fn fks() -> Vec<ForeignKey> {
    vec![ForeignKey {
        name: "orders_customer_id_fkey".into(),
        columns: vec!["customer_id".into()],
        ref_schema: "shop".into(),
        ref_table: "customers".into(),
        ref_columns: vec!["id".into()],
    }]
}

fn t(s: &str) -> Cell {
    Cell::Text(s.into())
}

fn row(id: &str, customer: Cell, name: Cell, price: Cell, notes: Cell, profile: Cell) -> Vec<Cell> {
    vec![t(id), customer, name, price, notes, profile, t("2026-09-21 04:51:04.856157+00"), t("0.00")]
}

/// Every kind of loaded value: plain text, numbers, links, NULL, empty and blank strings,
/// multi-line and too-long text, and large values that are not loaded into the grid.
fn value_rows() -> Vec<Vec<Cell>> {
    vec![
        row("1", t("42"), t("Widget"), t("9.99"), t("fragile"), t(r#"{"vip": true}"#)),
        row("2", t("7"), t(""), t("24.50"), Cell::Null, Cell::Null),
        row("3", t("13"), t("   "), t("1234567.89"), t("line one\nline two\tand a tab"), Cell::Large {
            stored_bytes: 884_383,
        }),
        row("4", t("2500"), t("Gizmo"), Cell::Null, t(&"lorem ipsum dolor sit amet ".repeat(30)), Cell::Large {
            stored_bytes: 31_457_280,
        }),
    ]
}

fn small_rows() -> Vec<Vec<Cell>> {
    value_rows().into_iter().take(2).collect()
}

// ---- the story -----------------------------------------------------------------------------------

/// One table in the story; anything left out means nothing staged, selected or being edited.
#[derive(Default)]
struct Spec {
    title: &'static str,
    expect: &'static str,
    rows: Vec<Vec<Cell>>,
    /// Makes the whole table read-only, as a view or a table without a primary key is.
    read_only: Option<&'static str>,
    pending: Pending,
    selected: Option<(usize, usize)>,
    /// Opens the selected cell for editing with this text.
    editing: Option<&'static str>,
}

fn staged(f: impl FnOnce(&mut Pending)) -> Pending {
    let mut p = Pending::default();
    f(&mut p);
    p
}

impl CellStory {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut case = |spec: Spec| {
            let editing = spec.selected.zip(spec.editing).map(|((r, c), text)| {
                let input = cx.new(|cx| InputState::new(window, cx));
                input.update(cx, |input, cx| {
                    input.set_value(text.to_string(), window, cx);
                    input.focus(window, cx);
                });
                (r, c, input)
            });
            let editability = match spec.read_only {
                Some(reason) => Editability { read_only_reason: Some(reason.into()), ..Default::default() },
                None => Editability { read_only_reason: None, insert: true, update: true, delete: true },
            };
            let rows = spec.rows.len() + spec.pending.inserts.len();
            let grid = Grid::detached(columns(), spec.rows, fks(), editability, spec.pending, spec.selected, editing);
            let table = cx.new(|cx| {
                TableState::new(grid, window, cx).col_resizable(true).col_movable(false).sortable(true)
            });
            Case { title: spec.title, expect: spec.expect, table, rows }
        };
        let n = columns().len();

        let values = vec![case(Spec {
            title: "Loaded values",
            expect: "Text, numbers, a link (customer_id), NULL vs empty vs blank text, multi-line and long text, \
                     large unloaded values (profile), a generated column (total).",
            rows: value_rows(),
            ..Default::default()
        })];

        let edits = vec![case(Spec {
            title: "Staged edits",
            expect: "Row 2: name and price changed. Row 3: notes set to NULL, placed_at to DEFAULT. Row 4: marked \
                     for deletion. Rows 5–6: new rows, one untouched (database fills it) and one partly filled.",
            rows: value_rows(),
            pending: staged(|p| {
                p.set_cell(4, 1, NAME, NewValue::Text("Gadget Pro".into()), &t(""));
                p.set_cell(4, 1, PRICE, NewValue::Text("19.00".into()), &t("24.50"));
                p.set_cell(4, 2, NOTES, NewValue::Null, &t("x"));
                p.set_cell(4, 2, PLACED_AT, NewValue::Default, &t("x"));
                p.toggle_delete(3);
                p.add_row(4, n);
                let draft = p.add_row(4, n);
                p.set_cell(4, draft, CUSTOMER, NewValue::Text("42".into()), &Cell::Null);
                p.set_cell(4, draft, NAME, NewValue::Text("Sprocket".into()), &Cell::Null);
                p.set_cell(4, draft, NOTES, NewValue::Null, &Cell::Null);
            }),
            ..Default::default()
        })];

        let selected = |title, expect, selected, pending| Spec {
            title,
            expect,
            rows: small_rows(),
            pending,
            selected: Some(selected),
            ..Default::default()
        };
        let selection = vec![
            case(selected("Selected: plain value", "Which cell has focus.", (0, NAME), Pending::default())),
            case(selected("Selected: link", "A link that is also selected.", (0, CUSTOMER), Pending::default())),
            case(selected("Selected: NULL", "A NULL that is selected.", (1, NOTES), Pending::default())),
            case(selected(
                "Selected: modified cell",
                "Selected, and its value is a staged change.",
                (0, NAME),
                staged(|p| p.set_cell(2, 0, NAME, NewValue::Text("Widget v2".into()), &t("Widget"))),
            )),
            case(selected(
                "Selected: in a deleted row",
                "Selected, in a row marked for deletion (can't be edited).",
                (1, NAME),
                staged(|p| {
                    p.toggle_delete(1);
                }),
            )),
            case(selected(
                "Selected: in a new row",
                "Selected, in a draft row: name is NOT NULL without a default, so it must be filled in.",
                (2, NAME),
                staged(|p| {
                    p.add_row(2, n);
                }),
            )),
            case(selected(
                "Selected: read-only column",
                "Selected, but the column can't be written (GENERATED ALWAYS AS IDENTITY).",
                (0, ID),
                Pending::default(),
            )),
            case(Spec {
                read_only: Some("Views, materialized views and foreign tables are read-only here."),
                ..selected("Selected: read-only table", "Selected, in a view: nothing can be edited.", (0, NAME), Pending::default())
            }),
        ];

        let editing = vec![
            case(Spec {
                editing: Some("Widget"),
                ..selected(
                    "Editing: existing value",
                    "The cell is open for typing (focused, caret visible).",
                    (0, NAME),
                    Pending::default(),
                )
            }),
            case(Spec {
                editing: Some(""),
                ..selected(
                    "Editing: new row",
                    "Typing into a draft row's cell.",
                    (2, NAME),
                    staged(|p| {
                        p.add_row(2, n);
                    }),
                )
            }),
        ];

        Self {
            sections: vec![("Values", values), ("Staged edits", edits), ("Selection", selection), ("Editing", editing)],
        }
    }
}

impl Render for CellStory {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // Header row plus data rows at the default table row height, plus the scrollbar.
        let table_height = |rows: usize| px(32. * (rows + 1) as f32 + 12.);
        v_flex()
            .id("cell-story")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_5()
            .bg(theme.background)
            .text_color(theme.foreground)
            .children(self.sections.iter().map(|(section, cases)| {
                v_flex()
                    .gap_2()
                    .child(div().text_lg().font_semibold().child(*section))
                    .child(v_flex().gap_3().children(cases.iter().map(|case| {
                        v_flex()
                            .w_full()
                            .gap_1()
                            .child(div().text_sm().font_semibold().child(case.title))
                            .child(div().text_xs().text_color(theme.muted_foreground).child(case.expect))
                            .child(
                                div()
                                    .h(table_height(case.rows))
                                    .border_1()
                                    .border_color(theme.border)
                                    .child(DataTable::new(&case.table).stripe(false).bordered(false)),
                            )
                    })))
            }))
    }
}
