//! A table's rows in a virtualized grid: sorting, filtering, paging, foreign-key links and editing.
//!
//! Each view owns its database connection, so a slow query or count in one tab can never block or
//! cancel another tab (or the navigator).
//!
//! Editing is staged: changes are highlighted in the grid but nothing is written until the user
//! reviews the generated SQL and submits, which applies everything in one transaction. While
//! changes are pending, paging/sorting/filtering are locked so staged edits can't be misplaced.
//! Large values are never in the grid (see `pgcore::data`), so they can't be edited inline.

use crate::runtime;
use crate::workspace::error_card;
use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use pgcore::catalog::{self, ForeignKey};
use pgcore::config::ConnectionParams;
use pgcore::data::{self, Access, Cell, Filter, Query, Sort, TablePage};
use pgcore::edit::{self, CellChange, EditSet, NewValue, RowChange};
use pgcore::{Session, UserFacingError};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Cells longer than this are cut off in the grid (the full value is still in memory).
const MAX_CELL_CHARS: usize = 400;
const PAGE_SIZES: [usize; 5] = [50, 100, 500, 1000, 5000];
pub const DEFAULT_PAGE_SIZE: usize = 500;
/// How long the exact row count may run before we give up and show an estimate instead.
const COUNT_TIMEOUT: Duration = Duration::from_secs(8);

pub enum DataViewEvent {
    /// The user followed a foreign key; the workspace decides where to show the target.
    OpenTable { schema: String, table: String, filter: Filter },
}

impl EventEmitter<DataViewEvent> for DataView {}

/// Where the view starts (used when opening from a foreign key, and by dev hooks).
#[derive(Debug, Clone)]
pub struct Initial {
    pub query: Query,
    pub page_size: usize,
    pub offset: usize,
}

impl Default for Initial {
    fn default() -> Self {
        Self { query: Query::default(), page_size: DEFAULT_PAGE_SIZE, offset: 0 }
    }
}

enum Load {
    Loading,
    Ready,
    /// The very first load failed, so there is no grid to keep showing.
    Failed(UserFacingError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Total {
    Counting,
    Exact(i64),
    /// The exact count timed out; the planner's estimate (if any) is shown instead.
    Estimated(Option<i64>),
}

// ---- staged edits (pure, unit tested) ------------------------------------------------------------

/// Edits that have not been written yet. Indexes refer to rows of the *current page*, which is why
/// paging, sorting and filtering are locked while anything is pending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pending {
    /// `(row, column)` of an existing row → its new value.
    pub cells: BTreeMap<(usize, usize), NewValue>,
    pub deleted: BTreeSet<usize>,
    /// Draft rows appended after the page; one slot per column, `None` = not set (database default).
    pub inserts: Vec<Vec<Option<NewValue>>>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty() && self.deleted.is_empty() && self.inserts.is_empty()
    }

    /// Number of rows that will be touched (an edited-then-deleted row counts once, as a delete).
    pub fn change_count(&self) -> usize {
        let edited: BTreeSet<usize> =
            self.cells.keys().map(|(r, _)| *r).filter(|r| !self.deleted.contains(r)).collect();
        edited.len() + self.deleted.len() + self.inserts.len()
    }

    /// Stages `value` for a cell. Setting a cell back to what it already holds un-stages it.
    pub fn set_cell(&mut self, existing_rows: usize, row: usize, col: usize, value: NewValue, original: &Cell) {
        if row >= existing_rows {
            if let Some(draft) = self.inserts.get_mut(row - existing_rows) {
                if let Some(slot) = draft.get_mut(col) {
                    *slot = Some(value);
                }
            }
            return;
        }
        let unchanged = match (&value, original) {
            (NewValue::Text(t), Cell::Text(o)) => t == o,
            (NewValue::Null, Cell::Null) => true,
            _ => false,
        };
        if unchanged {
            self.cells.remove(&(row, col));
        } else {
            self.cells.insert((row, col), value);
        }
    }

    /// Removes a staged value (a draft cell goes back to "database default").
    pub fn clear_cell(&mut self, existing_rows: usize, row: usize, col: usize) {
        if row >= existing_rows {
            if let Some(slot) = self.inserts.get_mut(row - existing_rows).and_then(|d| d.get_mut(col)) {
                *slot = None;
            }
        } else {
            self.cells.remove(&(row, col));
        }
    }

    /// Marks an existing row for deletion, or unmarks it. Returns whether it is now marked.
    pub fn toggle_delete(&mut self, row: usize) -> bool {
        if !self.deleted.remove(&row) {
            self.deleted.insert(row);
            true
        } else {
            false
        }
    }

    /// Appends an empty draft row and returns its row index.
    pub fn add_row(&mut self, existing_rows: usize, columns: usize) -> usize {
        self.inserts.push(vec![None; columns]);
        existing_rows + self.inserts.len() - 1
    }

    pub fn remove_draft(&mut self, existing_rows: usize, row: usize) {
        if row >= existing_rows && row - existing_rows < self.inserts.len() {
            self.inserts.remove(row - existing_rows);
        }
    }

    pub fn clear(&mut self) {
        *self = Pending::default();
    }
}

/// What the user may do with this table, and why not when they may not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editability {
    pub read_only_reason: Option<String>,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
}

pub fn editability(access: &Access, columns: &[data::Column]) -> Editability {
    let reason = if !access.is_plain_table() {
        Some("Views, materialized views and foreign tables are read-only here.")
    } else if !columns.iter().any(|c| c.is_primary_key) {
        Some("No primary key, so rows can't be identified safely. Read-only.")
    } else if !(access.insert || access.update || access.delete) {
        Some("You don't have write privileges on this table.")
    } else {
        None
    };
    match reason {
        Some(r) => Editability { read_only_reason: Some(r.into()), ..Default::default() },
        None => Editability {
            read_only_reason: None,
            insert: access.insert,
            update: access.update,
            delete: access.delete,
        },
    }
}

/// The primary-key values of an existing row, or an error if they aren't loaded as text.
fn row_key(columns: &[data::Column], row: &[Cell]) -> Result<edit::Key, UserFacingError> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_primary_key)
        .map(|(ix, c)| match row.get(ix) {
            Some(Cell::Text(v)) => Ok((c.name.clone(), v.clone())),
            _ => Err(UserFacingError::config(
                "Can't identify a row",
                format!("The value of key column \"{}\" isn't available for this row.", c.name),
            )),
        })
        .collect()
}

/// Turns staged edits into an [`EditSet`]: updates, then deletes, then inserts.
pub fn build_edit_set(
    schema: &str,
    table: &str,
    columns: &[data::Column],
    rows: &[Vec<Cell>],
    pending: &Pending,
) -> Result<EditSet, UserFacingError> {
    let mut changes = Vec::new();
    let stale = || UserFacingError::config("Stale edit", "A staged row no longer exists.");
    let edited_rows: BTreeSet<usize> =
        pending.cells.keys().map(|(r, _)| *r).filter(|r| !pending.deleted.contains(r)).collect();
    for row_ix in edited_rows {
        let row = rows.get(row_ix).ok_or_else(stale)?;
        let cell_changes = pending
            .cells
            .range((row_ix, 0)..=(row_ix, usize::MAX))
            .map(|(&(_, col), new)| CellChange {
                column: columns[col].name.clone(),
                old: row[col].clone(),
                new: new.clone(),
            })
            .collect();
        changes.push(RowChange::Update { key: row_key(columns, row)?, changes: cell_changes });
    }
    for &row_ix in &pending.deleted {
        let row = rows.get(row_ix).ok_or_else(stale)?;
        changes.push(RowChange::Delete { key: row_key(columns, row)? });
    }
    for draft in &pending.inserts {
        let values = draft
            .iter()
            .enumerate()
            .filter_map(|(ix, v)| v.clone().map(|v| (columns[ix].name.clone(), v)))
            .collect();
        changes.push(RowChange::Insert { values });
    }
    Ok(EditSet { schema: schema.into(), table: table.into(), changes })
}

// ---- the view ------------------------------------------------------------------------------------

pub struct DataView {
    params: ConnectionParams,
    session: Option<Session>,
    schema: String,
    table: String,
    /// Planner row estimate from the navigator, shown while the exact count runs.
    estimate: Option<i64>,

    columns: Vec<data::Column>,
    fks: Vec<ForeignKey>,
    editability: Editability,
    /// What the grid currently shows.
    query: Query,
    offset: usize,
    page_size: usize,
    has_more: bool,
    row_count: usize,
    total: Total,

    load: Load,
    /// A failed request (error) or a notice (info) shown above the grid; the grid keeps its data.
    banner: Option<(UserFacingError, bool)>,
    /// Shown as an info banner after the next successful load (e.g. "Saved: 2 updated").
    saved_notice: Option<String>,
    /// Bumped per request so a slow, stale response can't overwrite a newer one.
    generation: u64,
    count_generation: u64,

    pending: Pending,
    selected: Option<(usize, usize)>,
    editing: Option<(usize, usize)>,
    /// SQL lines shown for review before submitting.
    review: Option<Vec<String>>,
    applying: bool,

    grid: Entity<TableState<Grid>>,
    filter_input: Entity<InputState>,
    edit_input: Entity<InputState>,
}

struct Grid {
    columns: Vec<data::Column>,
    rows: Vec<Vec<Cell>>,
    fks: Vec<ForeignKey>,
    sort: Option<Sort>,
    owner: WeakEntity<DataView>,
    // Mirrors of the view's editing state, so cells can render it.
    pending: Pending,
    selected: Option<(usize, usize)>,
    editing: Option<(usize, usize, Entity<InputState>)>,
}

impl DataView {
    pub fn open(
        params: ConnectionParams,
        schema: String,
        table: String,
        estimate: Option<i64>,
        initial: Initial,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let owner = cx.entity().downgrade();
        let grid = cx.new(|cx| {
            TableState::new(
                Grid {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    fks: Vec::new(),
                    sort: None,
                    owner,
                    pending: Pending::default(),
                    selected: None,
                    editing: None,
                },
                window,
                cx,
            )
            .col_resizable(true)
            .col_movable(false)
            .sortable(true)
        });
        let filter_text = initial.query.filter.display();
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter rows: a SQL condition, e.g. price > 10 AND name ILIKE '%pen%'")
        });
        filter_input.update(cx, |input, cx| input.set_value(filter_text, window, cx));
        cx.subscribe_in(&filter_input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.apply_filter_text(cx);
            }
        })
        .detach();

        let edit_input = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe_in(&edit_input, window, |this, _, event: &InputEvent, _, cx| {
            // Enter or clicking away keeps the edit; Escape (handled by the cell) drops it first.
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_edit(cx);
            }
        })
        .detach();

        let mut this = Self {
            params,
            session: None,
            schema,
            table,
            estimate: estimate.filter(|n| *n >= 0),
            columns: Vec::new(),
            fks: Vec::new(),
            editability: Editability::default(),
            query: Query::default(),
            offset: 0,
            page_size: initial.page_size,
            has_more: false,
            row_count: 0,
            total: Total::Counting,
            load: Load::Loading,
            banner: None,
            saved_notice: None,
            generation: 0,
            count_generation: 0,
            pending: Pending::default(),
            selected: None,
            editing: None,
            review: None,
            applying: false,
            grid,
            filter_input,
            edit_input,
        };
        this.load(initial.query, initial.offset, initial.page_size, true, cx);
        this
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// The filter currently applied to the grid.
    pub fn filter(&self) -> &Filter {
        &self.query.filter
    }

    /// Unsaved edits exist (closing the tab would lose them).
    pub fn is_dirty(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn warn_unsaved(&mut self, cx: &mut Context<Self>) {
        self.notice("Unsaved changes", "Submit or revert your changes before closing this tab.", cx);
    }

    fn locked(&self) -> bool {
        !self.pending.is_empty() || self.review.is_some() || self.applying
    }

    fn notice(&mut self, title: &str, detail: &str, cx: &mut Context<Self>) {
        self.banner = Some((UserFacingError::config(title, detail), true));
        cx.notify();
    }

    /// Refuses a navigation-type action while edits are pending.
    fn refuse_while_locked(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.locked() {
            return false;
        }
        self.notice(
            "Changes are pending",
            "Submit or revert your changes before paging, sorting, filtering or reloading.",
            cx,
        );
        // A header click already flipped its arrow; put it back.
        self.grid.update(cx, |state, cx| state.refresh(cx));
        true
    }

    /// Mirrors editing state into the grid so cells can draw it.
    fn sync_grid(&mut self, cx: &mut Context<Self>) {
        let pending = self.pending.clone();
        let selected = self.selected;
        let editing = self.editing.map(|(r, c)| (r, c, self.edit_input.clone()));
        self.grid.update(cx, |state, cx| {
            let g = state.delegate_mut();
            g.pending = pending;
            g.selected = selected;
            g.editing = editing;
            cx.notify();
        });
        cx.notify();
    }

    /// Requests a page with the given query. On failure the previous state is kept and the error
    /// is shown as a banner (or as the main card when nothing has loaded yet).
    fn load(&mut self, query: Query, offset: usize, page_size: usize, recount: bool, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.load = if self.columns.is_empty() { Load::Loading } else { Load::Ready };
        cx.notify();

        let existing = self.session.clone();
        let params = self.params.clone();
        let (schema, table) = (self.schema.clone(), self.table.clone());
        let known = (!self.columns.is_empty()).then(|| (self.columns.clone(), self.fks.clone()));
        let requested = query.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                // Reconnect if this tab's connection was lost since the last request.
                let session = match existing {
                    Some(s) if s.is_connected() => s,
                    _ => Session::connect(&params).await?,
                };
                let (columns, fks, access) = match known {
                    Some((columns, fks)) => (columns, fks, None),
                    None => {
                        let columns = data::table_columns(&session, &schema, &table).await?;
                        // Links and privileges are conveniences: failing to read them must not hide
                        // the data (it just makes the table read-only / link-less).
                        let fks = catalog::foreign_keys(&session, &schema, &table).await.unwrap_or_default();
                        let access = data::table_access(&session, &schema, &table).await.unwrap_or_default();
                        (columns, fks, Some(access))
                    }
                };
                let page = data::fetch_page(&session, &schema, &table, &columns, &requested, offset, page_size).await?;
                Ok((session, fks, access, page))
            })
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok((session, fks, access, page)) => {
                        this.apply(session, fks, access, page, query, page_size, recount, cx)
                    }
                    Err(err) => {
                        if this.columns.is_empty() {
                            this.load = Load::Failed(err);
                        } else {
                            this.banner = Some((err, false));
                            this.load = Load::Ready;
                            // Put the header sort arrows back to what is actually applied.
                            this.grid.update(cx, |state, cx| state.refresh(cx));
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        session: Session,
        fks: Vec<ForeignKey>,
        access: Option<Access>,
        page: TablePage,
        query: Query,
        page_size: usize,
        recount: bool,
        cx: &mut Context<Self>,
    ) {
        self.session = Some(session);
        self.fks = fks.clone();
        self.columns = page.columns.clone();
        if let Some(access) = access {
            self.editability = editability(&access, &page.columns);
        }
        let filter_changed = self.query.filter != query.filter;
        self.query = query.clone();
        self.offset = page.offset;
        self.page_size = page_size;
        self.has_more = page.has_more;
        self.row_count = page.rows.len();
        self.banner = self.saved_notice.take().map(|msg| (UserFacingError::config("Saved", msg), true));
        self.load = Load::Ready;
        // Fresh data invalidates any staged indexes.
        self.pending.clear();
        self.selected = None;
        self.editing = None;
        self.review = None;

        let owner = cx.entity().downgrade();
        let sort = query.sort;
        let columns_changed = self.grid.read(cx).delegate().columns != page.columns;
        self.grid.update(cx, |state, cx| {
            *state.delegate_mut() = Grid {
                columns: page.columns,
                rows: page.rows,
                fks,
                sort,
                owner,
                pending: Pending::default(),
                selected: None,
                editing: None,
            };
            // Rebuilding the columns resets widths the user dragged, so only do it when the
            // column set really changed (first load); paging and sorting keep their widths.
            if columns_changed {
                state.refresh(cx);
            }
            state.scroll_to_row(0, cx);
            cx.notify();
        });

        if !page.has_more {
            // The last page shows everything: the total is known without another query.
            self.count_generation += 1;
            self.total = Total::Exact((page.offset + self.row_count) as i64);
        } else if recount || filter_changed {
            self.start_count(cx);
        }
        self.stage_from_env(cx);
        cx.notify();
    }

    /// Runs the exact count on its own short-lived connection with a time limit.
    fn start_count(&mut self, cx: &mut Context<Self>) {
        self.count_generation += 1;
        let generation = self.count_generation;
        self.total = Total::Counting;
        let params = self.params.clone();
        let (schema, table) = (self.schema.clone(), self.table.clone());
        let columns = self.columns.clone();
        let filter = self.query.filter.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                let session = Session::connect(&params).await?;
                match tokio::time::timeout(
                    COUNT_TIMEOUT,
                    data::count_rows(&session, &schema, &table, &columns, &filter),
                )
                .await
                {
                    Ok(count) => count.map(Some),
                    Err(_) => {
                        // Too slow: stop the server-side work, then fall back to the estimate.
                        let _ = session.cancel().await;
                        Ok(None)
                    }
                }
            })
            .await;
            this.update(cx, |this, cx| {
                if this.count_generation != generation {
                    return;
                }
                this.total = match result {
                    Ok(Some(n)) => Total::Exact(n),
                    Ok(None) => Total::Estimated(this.estimate.filter(|_| this.query.filter.is_none())),
                    // Errors (e.g. a bad filter) are already reported by the page request.
                    Err(_) => Total::Estimated(None),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- navigation actions ----------------------------------------------------------------------

    fn set_sort(&mut self, sort: Option<Sort>, cx: &mut Context<Self>) {
        if self.refuse_while_locked(cx) {
            return;
        }
        let query = Query { sort, filter: self.query.filter.clone() };
        self.load(query, 0, self.page_size, false, cx);
    }

    fn apply_filter_text(&mut self, cx: &mut Context<Self>) {
        if self.refuse_while_locked(cx) {
            return;
        }
        let text = self.filter_input.read(cx).value().to_string();
        // Unedited text of a foreign-key jump keeps its exact (parameterised) form.
        let filter = if matches!(self.query.filter, Filter::Match(_)) && text.trim() == self.query.filter.display() {
            self.query.filter.clone()
        } else {
            Filter::from_text(&text)
        };
        let query = Query { sort: self.query.sort.clone(), filter };
        self.load(query, 0, self.page_size, true, cx);
    }

    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.refuse_while_locked(cx) {
            return;
        }
        self.filter_input.update(cx, |input, cx| input.set_value("", window, cx));
        let query = Query { sort: self.query.sort.clone(), filter: Filter::None };
        self.load(query, 0, self.page_size, true, cx);
    }

    fn go_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.refuse_while_locked(cx) {
            return;
        }
        self.load(self.query.clone(), offset, self.page_size, false, cx);
    }

    fn set_page_size(&mut self, size: usize, cx: &mut Context<Self>) {
        if self.refuse_while_locked(cx) {
            return;
        }
        // Keep the first visible row on screen: snap the offset down to the new page boundary.
        let offset = (self.offset / size) * size;
        self.load(self.query.clone(), offset, size, false, cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.refuse_while_locked(cx) {
            return;
        }
        self.load(self.query.clone(), self.offset, self.page_size, true, cx);
    }

    /// Follows a foreign key from a grid row to the referenced row.
    fn follow_fk(&mut self, row_ix: usize, fk_ix: usize, cx: &mut Context<Self>) {
        let Some(fk) = self.fks.get(fk_ix) else { return };
        let grid = self.grid.read(cx).delegate();
        let Some(row) = grid.rows.get(row_ix) else { return };
        let mut pairs = Vec::with_capacity(fk.columns.len());
        for (col, ref_col) in fk.columns.iter().zip(&fk.ref_columns) {
            let Some(ix) = self.columns.iter().position(|c| &c.name == col) else { return };
            // MATCH SIMPLE: a NULL member means the row references nothing.
            let Some(Cell::Text(value)) = row.get(ix) else { return };
            pairs.push((ref_col.clone(), value.clone()));
        }
        cx.emit(DataViewEvent::OpenTable {
            schema: fk.ref_schema.clone(),
            table: fk.ref_table.clone(),
            filter: Filter::Match(pairs),
        });
    }

    // ---- editing actions -------------------------------------------------------------------------

    fn existing_rows(&self, cx: &App) -> usize {
        self.grid.read(cx).delegate().rows.len()
    }

    fn original_cell(&self, row: usize, col: usize, cx: &App) -> Cell {
        self.grid
            .read(cx)
            .delegate()
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .cloned()
            .unwrap_or(Cell::Null)
    }

    /// A click on a cell: ⌘-click follows a foreign key, a click selects, a double-click edits.
    fn cell_clicked(
        &mut self,
        row: usize,
        col: usize,
        clicks: usize,
        follow_link: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.applying || self.review.is_some() {
            return;
        }
        if follow_link {
            let column = self.columns.get(col);
            if let Some(fk_ix) =
                self.fks.iter().position(|fk| column.is_some_and(|c| fk.columns.contains(&c.name)))
            {
                self.follow_fk(row, fk_ix, cx);
                return;
            }
        }
        // Clicking another cell while editing commits first (the input's blur does it).
        self.selected = Some((row, col));
        self.sync_grid(cx);
        if clicks >= 2 {
            self.begin_edit(row, col, window, cx);
        }
    }

    /// Whether the cell may be edited, with the reason if not.
    fn edit_block_reason(&self, row: usize, col: usize, cx: &App) -> Option<String> {
        if let Some(reason) = &self.editability.read_only_reason {
            return Some(reason.clone());
        }
        let existing = self.existing_rows(cx);
        let is_draft = row >= existing;
        if is_draft && !self.editability.insert {
            return Some("You don't have INSERT privilege on this table.".into());
        }
        if !is_draft && !self.editability.update {
            return Some("You don't have UPDATE privilege on this table.".into());
        }
        if !is_draft && self.pending.deleted.contains(&row) {
            return Some("This row is marked for deletion.".into());
        }
        let column = self.columns.get(col)?;
        if !column.is_writable() {
            return Some(format!("\"{}\" is generated by the database and can't be edited.", column.name));
        }
        if !is_draft
            && !self.pending.cells.contains_key(&(row, col))
            && self.original_cell(row, col, cx).is_large()
        {
            return Some(
                "This is a large value that isn't loaded. Editing large values needs the value editor, which isn't built yet."
                    .into(),
            );
        }
        None
    }

    fn begin_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reason) = self.edit_block_reason(row, col, cx) {
            self.notice("Can't edit this cell", &reason, cx);
            return;
        }
        let existing = self.existing_rows(cx);
        let current = if row >= existing {
            match self.pending.inserts.get(row - existing).and_then(|d| d.get(col)) {
                Some(Some(NewValue::Text(t))) => t.clone(),
                _ => String::new(),
            }
        } else {
            match self.pending.cells.get(&(row, col)) {
                Some(NewValue::Text(t)) => t.clone(),
                Some(_) => String::new(),
                None => self.original_cell(row, col, cx).as_text().unwrap_or("").to_string(),
            }
        };
        self.banner = None;
        self.editing = Some((row, col));
        self.edit_input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        self.sync_grid(cx);
    }

    fn commit_edit(&mut self, cx: &mut Context<Self>) {
        let Some((row, col)) = self.editing.take() else { return };
        let text = self.edit_input.read(cx).value().to_string();
        let existing = self.existing_rows(cx);
        let original = self.original_cell(row, col, cx);
        if row >= existing && text.is_empty() {
            // An empty draft cell means "not set": the database default applies.
            self.pending.clear_cell(existing, row, col);
        } else {
            self.pending.set_cell(existing, row, col, NewValue::Text(text), &original);
        }
        self.sync_grid(cx);
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.sync_grid(cx);
        }
    }

    /// Sets the selected cell to NULL or its DEFAULT.
    fn set_selected(&mut self, value: NewValue, cx: &mut Context<Self>) {
        let Some((row, col)) = self.selected else { return };
        if let Some(reason) = self.edit_block_reason(row, col, cx) {
            self.notice("Can't edit this cell", &reason, cx);
            return;
        }
        let existing = self.existing_rows(cx);
        let original = self.original_cell(row, col, cx);
        self.pending.set_cell(existing, row, col, value, &original);
        self.sync_grid(cx);
    }

    fn add_row(&mut self, cx: &mut Context<Self>) {
        if !self.editability.insert || self.locked_for_edit() {
            return;
        }
        let existing = self.existing_rows(cx);
        let row = self.pending.add_row(existing, self.columns.len());
        let first_writable = self.columns.iter().position(|c| c.is_writable()).unwrap_or(0);
        self.selected = Some((row, first_writable));
        self.sync_grid(cx);
        self.grid.update(cx, |state, cx| state.scroll_to_row(row, cx));
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some((row, _)) = self.selected else { return };
        let existing = self.existing_rows(cx);
        if row >= existing {
            self.pending.remove_draft(existing, row);
            self.selected = None;
        } else if self.editability.delete {
            self.pending.toggle_delete(row);
        } else {
            self.notice("Can't delete", "You don't have DELETE privilege on this table.", cx);
            return;
        }
        self.sync_grid(cx);
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.editing = None;
        self.review = None;
        self.banner = None;
        self.sync_grid(cx);
    }

    fn locked_for_edit(&self) -> bool {
        self.applying || self.review.is_some()
    }

    fn edit_set(&self, cx: &App) -> Result<EditSet, UserFacingError> {
        let grid = self.grid.read(cx).delegate();
        build_edit_set(&self.schema, &self.table, &self.columns, &grid.rows, &self.pending)
    }

    /// Shows the SQL that would run, for the user to check.
    fn review_changes(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.commit_edit(cx);
        }
        let result = self.edit_set(cx).and_then(|set| edit::preview(&set, &self.columns));
        match result {
            Ok(lines) if lines.is_empty() => self.notice("Nothing to submit", "No effective changes are staged.", cx),
            Ok(lines) => {
                self.banner = None;
                self.review = Some(lines);
                cx.notify();
            }
            Err(err) => {
                self.banner = Some((err, false));
                cx.notify();
            }
        }
    }

    /// Writes everything in one transaction; on any failure nothing is written and the staged
    /// edits stay so the user can fix and retry.
    fn apply_edits(&mut self, cx: &mut Context<Self>) {
        if self.applying {
            return;
        }
        let set = match self.edit_set(cx) {
            Ok(set) => set,
            Err(err) => {
                self.banner = Some((err, false));
                cx.notify();
                return;
            }
        };
        self.applying = true;
        cx.notify();
        let params = self.params.clone();
        let columns = self.columns.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                edit::apply(&params, &set, &columns).await.map_err(UserFacingError::from)
            })
            .await;
            this.update(cx, |this, cx| {
                this.applying = false;
                match result {
                    Ok(report) => {
                        this.saved_notice = Some(format!(
                            "{} updated, {} inserted, {} deleted.",
                            report.updated, report.inserted, report.deleted
                        ));
                        this.review = None;
                        // Reload the same page (this also clears the staged state).
                        this.load(this.query.clone(), this.offset, this.page_size, true, cx);
                    }
                    Err(err) => {
                        this.review = None;
                        this.banner = Some((err, false));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Dev hook for reproducing edit states in screenshots. `PGB_STAGE` is a `;`-separated list:
    /// `set:ROW:COL=text`, `null:ROW:COL`, `default:ROW:COL`, `del:ROW`, `ins`; `PGB_REVIEW=1` then
    /// opens the review panel. Uses the same staging code as the UI.
    fn stage_from_env(&mut self, cx: &mut Context<Self>) {
        let Ok(script) = std::env::var("PGB_STAGE") else { return };
        let existing = self.existing_rows(cx);
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (verb, rest) = step.split_once(':').unwrap_or((step, ""));
            let cell = |spec: &str| -> Option<(usize, usize)> {
                let (r, c) = spec.split_once(':')?;
                Some((r.parse().ok()?, c.parse().ok()?))
            };
            match verb {
                "set" => {
                    if let Some((pos, text)) = rest.split_once('=') {
                        if let Some((r, c)) = cell(pos) {
                            let orig = self.original_cell(r, c, cx);
                            self.pending.set_cell(existing, r, c, NewValue::Text(text.to_string()), &orig);
                        }
                    }
                }
                "null" | "default" => {
                    if let Some((r, c)) = cell(rest) {
                        let orig = self.original_cell(r, c, cx);
                        let value = if verb == "null" { NewValue::Null } else { NewValue::Default };
                        self.pending.set_cell(existing, r, c, value, &orig);
                    }
                }
                "del" => {
                    if let Ok(r) = rest.parse() {
                        self.pending.toggle_delete(r);
                    }
                }
                "ins" => {
                    self.pending.add_row(existing, self.columns.len());
                }
                _ => {}
            }
        }
        self.sync_grid(cx);
        if std::env::var_os("PGB_REVIEW").is_some() {
            self.review_changes(cx);
        }
    }

    // ---- rendering helpers -----------------------------------------------------------------------

    fn render_edit_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let e = &self.editability;
        let busy = self.locked_for_edit();
        let selected = self.selected;
        let existing = self.existing_rows(cx);
        let sel_col = selected.and_then(|(_, c)| self.columns.get(c));
        let sel_is_draft = selected.is_some_and(|(r, _)| r >= existing);
        let can_null = !busy
            && sel_col.is_some_and(|c| c.is_writable() && !c.not_null)
            && selected.is_some_and(|(r, c)| self.edit_block_reason(r, c, cx).is_none());
        let can_default = !busy
            && sel_col.is_some_and(|c| c.is_writable() && (c.has_default || c.identity != data::Identity::None))
            && selected.is_some_and(|(r, c)| self.edit_block_reason(r, c, cx).is_none());
        let can_delete = !busy && selected.is_some() && (sel_is_draft || e.delete);
        let dirty = !self.pending.is_empty();
        let count = self.pending.change_count();

        h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .px_3()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("add-row")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Plus))
                    .label("Row")
                    .disabled(!e.insert || busy)
                    .on_click(cx.listener(|this, _, _, cx| this.add_row(cx))),
            )
            .child(
                Button::new("delete-row")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Trash))
                    .label("Delete")
                    .disabled(!can_delete)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))),
            )
            .child(
                Button::new("set-null")
                    .ghost()
                    .small()
                    .label("Set NULL")
                    .disabled(!can_null)
                    .on_click(cx.listener(|this, _, _, cx| this.set_selected(NewValue::Null, cx))),
            )
            .child(
                Button::new("set-default")
                    .ghost()
                    .small()
                    .label("Set Default")
                    .disabled(!can_default)
                    .on_click(cx.listener(|this, _, _, cx| this.set_selected(NewValue::Default, cx))),
            )
            .child(div().flex_1())
            .when_some(e.read_only_reason.clone(), |bar, reason| {
                bar.child(Icon::new(IconName::Lock).size_3().text_color(theme.muted_foreground))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(reason))
            })
            .when(e.read_only_reason.is_none() && !self.fks.is_empty() && !dirty, |bar| {
                bar.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Double-click to edit · ⌘-click a link to follow it"),
                )
            })
            .when(dirty, |bar| {
                bar.child(
                    Button::new("revert")
                        .ghost()
                        .small()
                        .label("Revert")
                        .disabled(self.applying)
                        .on_click(cx.listener(|this, _, _, cx| this.revert(cx))),
                )
                .child(
                    Button::new("submit")
                        .primary()
                        .small()
                        .label(format!("Review & submit ({count})"))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.review_changes(cx))),
                )
            })
    }

    fn render_review(&self, lines: &[String], cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .child(
                h_flex()
                    .h(px(36.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .child(div().font_semibold().child(format!(
                        "Review {} statement{}: they run together in one transaction",
                        lines.len(),
                        if lines.len() == 1 { "" } else { "s" }
                    )))
                    .child(div().flex_1())
                    .child(
                        Button::new("review-back")
                            .ghost()
                            .small()
                            .label("Back")
                            .disabled(self.applying)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.review = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("review-apply")
                            .primary()
                            .small()
                            .label(if self.applying { "Applying…" } else { "Apply" })
                            .loading(self.applying)
                            .disabled(self.applying)
                            .on_click(cx.listener(|this, _, _, cx| this.apply_edits(cx))),
                    ),
            )
            .child(
                v_flex()
                    .id("review-sql")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .children(lines.iter().map(|l| div().child(l.clone()))),
            )
    }
}

impl Render for DataView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let loading = matches!(self.load, Load::Loading);
        let idle = matches!(self.load, Load::Ready);
        let locked = self.locked();
        let view = cx.entity().downgrade();

        let page_size = self.page_size;
        let offset = self.offset;
        let can_prev = idle && offset > 0 && !locked;
        let can_next = idle && self.has_more && !locked;
        let last_offset = match self.total {
            Total::Exact(n) => Some(data::last_page_offset(n, page_size)),
            _ => None,
        };
        let can_last = idle && !locked && last_offset.is_some_and(|o| o > offset);

        let page_menu = {
            let view = view.clone();
            Button::new("page-size")
                .ghost()
                .small()
                .label(format!("{} rows", format_count(page_size as i64)))
                .disabled(locked)
                .dropdown_menu(move |menu, _, _| {
                    PAGE_SIZES.iter().fold(menu, |menu, &size| {
                        let view = view.clone();
                        menu.item(
                            PopupMenuItem::new(format!("{} rows per page", format_count(size as i64)))
                                .checked(size == page_size)
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |v, cx| v.set_page_size(size, cx)).ok();
                                }),
                        )
                    })
                })
        };

        let toolbar = h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(div().text_sm().text_color(theme.muted_foreground).child(if loading {
                "Loading…".to_string()
            } else if matches!(self.load, Load::Failed(_)) {
                String::new()
            } else {
                range_label(self.offset, self.row_count, &self.total, self.estimate, self.query.filter.is_none())
            }))
            .child(div().flex_1())
            .child(
                Button::new("first")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::ChevronsLeft))
                    .disabled(!can_prev)
                    .on_click(cx.listener(|this, _, _, cx| this.go_to(0, cx))),
            )
            .child(
                Button::new("prev")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::ChevronLeft))
                    .disabled(!can_prev)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.go_to(offset.saturating_sub(page_size), cx)
                    })),
            )
            .child(
                Button::new("next")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::ChevronRight))
                    .disabled(!can_next)
                    .on_click(cx.listener(move |this, _, _, cx| this.go_to(offset + page_size, cx))),
            )
            .child(
                Button::new("last")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::ChevronsRight))
                    .disabled(!can_last)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(o) = last_offset {
                            this.go_to(o, cx)
                        }
                    })),
            )
            .child(page_menu)
            .child(
                Button::new("reload")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::RefreshCw))
                    .disabled(loading || locked)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            );

        let filtered = !self.query.filter.is_none();
        let filter_bar = h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Icon::new(IconName::ListFilter)
                    .size_4()
                    .text_color(if filtered { theme.primary } else { theme.muted_foreground }),
            )
            .child(div().flex_1().child(Input::new(&self.filter_input).appearance(false)))
            .child(
                Button::new("clear-filter")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::X))
                    .disabled(locked || (!filtered && self.filter_input.read(cx).value().is_empty()))
                    .on_click(cx.listener(|this, _, window, cx| this.clear_filter(window, cx))),
            );

        let banner = self.banner.as_ref().map(|(err, is_info)| {
            let accent = if *is_info { theme.info } else { theme.danger };
            let mut text = err.detail.clone();
            if let Some(pos) = err.position {
                text.push_str(&format!(" (at character {pos} of the filter)"));
            }
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .gap_2()
                .items_start()
                .bg(accent.opacity(0.14))
                .border_b_1()
                .border_color(accent)
                .child(
                    Icon::new(if *is_info { IconName::Info } else { IconName::TriangleAlert })
                        .size_4()
                        .text_color(accent),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_semibold().child(err.title.clone()))
                        .child(div().text_sm().child(text))
                        .when_some(err.hint.clone(), |c, hint| {
                            c.child(div().text_sm().text_color(theme.muted_foreground).child(hint))
                        }),
                )
                .child(
                    Button::new("dismiss-banner")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::X))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.banner = None;
                            cx.notify();
                        })),
                )
        });

        let failed = matches!(self.load, Load::Failed(_));
        let body = match &self.load {
            Load::Failed(err) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .p_8()
                .child(error_card(err, cx))
                .into_any_element(),
            _ => div()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(DataTable::new(&self.grid).stripe(false).bordered(false))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .child(toolbar)
            .when(!failed, |v| v.child(self.render_edit_bar(cx)))
            .when(!failed && self.review.is_none(), |v| v.child(filter_bar))
            .when_some(self.review.clone(), |v, lines| v.child(self.render_review(&lines, cx)))
            .children(banner)
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl TableDelegate for Grid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len() + self.pending.inserts.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let c = &self.columns[col_ix];
        let is_fk = self.fks.iter().any(|fk| fk.columns.contains(&c.name));
        let mut col = Column::new(c.name.clone(), c.name.clone())
            .width(column_width(c, is_fk, &self.rows, col_ix))
            .min_width(px(60.))
            .resizable(true);
        // Report the applied sort so the header arrow survives page loads and refreshes.
        col = match &self.sort {
            Some(s) if s.column == c.name => {
                col.sort(if s.descending { ColumnSort::Descending } else { ColumnSort::Ascending })
            }
            _ => col.sortable(),
        };
        if is_numeric(&c.type_name) {
            col = col.text_right();
        }
        col
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(column) = self.columns.get(col_ix) else { return };
        let requested = match sort {
            ColumnSort::Default => None,
            ColumnSort::Ascending => Some(Sort { column: column.name.clone(), descending: false }),
            ColumnSort::Descending => Some(Sort { column: column.name.clone(), descending: true }),
        };
        self.owner.update(cx, |view, cx| view.set_sort(requested, cx)).ok();
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let c = &self.columns[col_ix];
        let muted = cx.theme().muted_foreground;
        let is_fk = self.fks.iter().any(|fk| fk.columns.contains(&c.name));
        h_flex()
            .size_full()
            .items_center()
            .gap_1p5()
            .overflow_hidden()
            // The name keeps its width; the type label is what gets shortened when space is tight.
            .child(div().flex_shrink_0().child(c.name.clone()))
            .when(c.is_primary_key, |h| h.child(Icon::new(IconName::Key).size_3().text_color(muted)))
            .when(is_fk, |h| h.child(Icon::new(IconName::Link).size_3().text_color(muted)))
            .child(div().text_xs().text_color(muted).truncate().child(c.type_name.clone()))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let existing = self.rows.len();
        let is_draft = row_ix >= existing;
        let column = self.columns.get(col_ix);
        let numeric = column.is_some_and(|c| is_numeric(&c.type_name));
        let is_selected = self.selected == Some((row_ix, col_ix));
        let deleted = !is_draft && self.pending.deleted.contains(&row_ix);

        // What the cell shows: a staged value if there is one, else what was loaded.
        let staged: Option<&NewValue> = if is_draft {
            self.pending.inserts.get(row_ix - existing).and_then(|d| d.get(col_ix)).and_then(|v| v.as_ref())
        } else {
            self.pending.cells.get(&(row_ix, col_ix))
        };
        let modified = staged.is_some() && !is_draft;

        let owner = self.owner.clone();
        let tint = if deleted {
            Some(theme.danger.opacity(0.16))
        } else if is_draft {
            Some(theme.success.opacity(0.14))
        } else if modified {
            Some(theme.warning.opacity(0.20))
        } else {
            None
        };
        let base = div()
            .id(("cell", row_ix * 4096 + col_ix))
            .w_full()
            .truncate()
            .border_1()
            .border_color(if is_selected { theme.primary } else { gpui_kit::transparent_black() })
            .when(numeric, |d| d.text_right())
            .when_some(tint, |d, c| d.bg(c))
            .when(deleted, |d| d.line_through().text_color(theme.muted_foreground))
            .on_click(move |ev, window, cx| {
                let clicks = ev.click_count();
                let follow = ev.modifiers().platform;
                owner
                    .update(cx, |view, cx| view.cell_clicked(row_ix, col_ix, clicks, follow, window, cx))
                    .ok();
            });

        // The cell being edited shows the shared input.
        if let Some((r, c, input)) = &self.editing {
            if (*r, *c) == (row_ix, col_ix) {
                let owner = self.owner.clone();
                return div()
                    .w_full()
                    .on_key_down(move |ev, _, cx| {
                        if ev.keystroke.key == "escape" {
                            owner.update(cx, |view, cx| view.cancel_edit(cx)).ok();
                        }
                    })
                    .child(Input::new(input).appearance(false))
                    .into_any_element();
            }
        }

        let italic_muted =
            |d: Stateful<Div>, text: &str| d.text_color(theme.muted_foreground).italic().child(text.to_string());
        match staged {
            Some(NewValue::Text(t)) => return base.child(display_text(t)).into_any_element(),
            Some(NewValue::Null) => return italic_muted(base, "NULL").into_any_element(),
            Some(NewValue::Default) => return italic_muted(base, "DEFAULT").into_any_element(),
            None => {}
        }
        if is_draft {
            // An untouched cell of a new row: the database fills it in.
            let label = match column {
                Some(c) if !c.is_writable() => "auto",
                Some(c) if c.has_default || c.identity != data::Identity::None => "DEFAULT",
                _ => "NULL",
            };
            return italic_muted(base, label).into_any_element();
        }

        let Some(cell_value) = self.rows.get(row_ix).and_then(|r| r.get(col_ix)) else {
            return div().into_any_element();
        };
        match cell_value {
            Cell::Null => italic_muted(base, "NULL").into_any_element(),
            // Large values are never in memory; show what they are and how big they are on disk.
            Cell::Large { stored_bytes } => {
                let ty = column.map_or("", |c| c.type_name.as_str());
                italic_muted(base, &large_label(ty, *stored_bytes)).into_any_element()
            }
            Cell::Text(value) => {
                // A foreign-key column with a value is a link (⌘-click follows it).
                let is_link = column.is_some_and(|c| self.fks.iter().any(|fk| fk.columns.contains(&c.name)));
                base.when(is_link && !deleted, |d| {
                    d.text_color(theme.link).cursor_pointer().hover(|s| s.underline())
                })
                .child(display_text(value))
                .into_any_element()
            }
        }
    }
}

// ---- pure helpers (unit tested) ------------------------------------------------------------------

/// "Rows 1–500 of 2,500", "Rows 501–1,000 of …" while counting, "of ~2.5k" for an estimate.
pub fn range_label(offset: usize, rows: usize, total: &Total, estimate: Option<i64>, unfiltered: bool) -> String {
    if rows == 0 {
        return "No rows".into();
    }
    let first = format_count((offset + 1) as i64);
    let last = format_count((offset + rows) as i64);
    let of = match total {
        Total::Exact(n) => format!(" of {}", format_count(*n)),
        Total::Counting => match estimate.filter(|_| unfiltered) {
            Some(est) => format!(" of ~{}…", crate::workspace::compact_count(est)),
            None => " of …".into(),
        },
        Total::Estimated(Some(est)) => format!(" of ~{}", crate::workspace::compact_count(*est)),
        Total::Estimated(None) => "+".into(),
    };
    format!("Rows {first}–{last}{of}")
}

/// 2500 → `2,500`.
pub fn format_count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// The scalar type name without precision (`numeric(10,2)` → `numeric`), or `""` for arrays: an
/// `integer[]` is displayed as `{1,2,3}` text and must not be treated like a number.
fn scalar_base(type_name: &str) -> &str {
    if type_name.ends_with("[]") {
        return "";
    }
    type_name.split('(').next().unwrap_or("").trim()
}

fn is_numeric(type_name: &str) -> bool {
    matches!(
        scalar_base(type_name),
        "smallint" | "integer" | "bigint" | "numeric" | "decimal" | "real" | "double precision" | "money" | "oid"
    )
}

/// Approximate glyph advance at the default 14px UI size; headers are a little wider.
const CELL_CHAR_PX: f32 = 8.0;
const HEADER_CHAR_PX: f32 = 8.5;
const TYPE_LABEL_CHAR_PX: f32 = 6.5;
const MIN_COLUMN_PX: f32 = 80.0;
const MAX_COLUMN_PX: f32 = 420.0;
/// How many rows are sampled to size a column.
const WIDTH_SAMPLE_ROWS: usize = 100;

/// Width that fits the header (name, key/link icons, a short type label) and the longest of the
/// sampled values, within sane limits. Sizing from the data avoids truncating short columns and
/// wasting space on long ones, which a per-type guess cannot do.
fn column_width(column: &data::Column, is_fk: bool, sample: &[Vec<Cell>], col_ix: usize) -> Pixels {
    let icons = if column.is_primary_key { 18.0 } else { 0.0 } + if is_fk { 18.0 } else { 0.0 };
    let header = column.name.chars().count() as f32 * HEADER_CHAR_PX
        + column.type_name.chars().count().min(14) as f32 * TYPE_LABEL_CHAR_PX
        + icons
        + 44.0; // padding, sort arrow, gaps
    let longest = sample
        .iter()
        .take(WIDTH_SAMPLE_ROWS)
        .filter_map(|row| row.get(col_ix))
        .map(|cell| match cell {
            Cell::Text(v) => v.chars().take(MAX_CELL_CHARS).count(),
            Cell::Null => 4,          // "NULL"
            Cell::Large { .. } => 26, // "jsonb · 884.4 KB stored"
        })
        .max()
        .unwrap_or(0);
    let cells = longest as f32 * CELL_CHAR_PX + 28.0;
    px(header.max(cells).clamp(MIN_COLUMN_PX, MAX_COLUMN_PX))
}

/// `884 KB`, `48.2 MB`: sizes as `pg_size_pretty` writes them (powers of 1024).
pub fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// What a large, unloaded cell shows. It says "stored": Postgres reports the on-disk (often
/// compressed) size, so the real text can be much larger.
pub fn large_label(type_name: &str, stored_bytes: i64) -> String {
    let ty = type_name.rsplit('.').next().unwrap_or(type_name);
    format!("{ty} · {} stored", format_bytes(stored_bytes))
}

/// One-line rendering of a cell: newlines become `↵` and very long values are cut off.
pub fn display_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(MAX_CELL_CHARS + 1));
    for (n, c) in value.chars().enumerate() {
        if n >= MAX_CELL_CHARS {
            out.push('…');
            break;
        }
        out.push(match c {
            '\n' | '\r' => '↵',
            '\t' => ' ',
            c => c,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would glob-import gpui's own `#[test]` macro over the built-in one.
    use super::{
        Editability, Pending, Total, build_edit_set, column_width, display_text, editability, format_bytes,
        format_count, is_numeric, large_label, range_label,
    };
    use pgcore::data::{Access, Cell, Column, Identity};
    use pgcore::edit::{NewValue, RowChange};

    fn col(name: &str, ty: &str, pk: bool) -> Column {
        Column { name: name.into(), type_name: ty.into(), is_primary_key: pk, ..Default::default() }
    }

    fn rows(values: &[Option<&str>]) -> Vec<Vec<Cell>> {
        values
            .iter()
            .map(|v| vec![v.map_or(Cell::Null, |t| Cell::Text(t.to_string()))])
            .collect()
    }

    fn px_value(p: gpui_kit::Pixels) -> f32 {
        f32::from(p)
    }

    fn text(t: &str) -> Cell {
        Cell::Text(t.into())
    }

    // -- display helpers

    #[test]
    fn newlines_and_tabs_stay_on_one_line() {
        assert_eq!(display_text("a\nb\r\nc\td"), "a↵b↵↵c d");
    }

    #[test]
    fn long_values_are_truncated_with_an_ellipsis() {
        let long = "x".repeat(1000);
        let shown = display_text(&long);
        assert_eq!(shown.chars().count(), 401);
        assert!(shown.ends_with('…'));
        assert_eq!(display_text("short"), "short");
    }

    #[test]
    fn numeric_types_are_recognised_including_modifiers_but_not_arrays() {
        for t in ["integer", "bigint", "numeric(10,2)", "numeric", "double precision", "smallint"] {
            assert!(is_numeric(t), "{t}");
        }
        for t in ["text", "text[]", "integer[]", "numeric(10,2)[]", "timestamp with time zone", "boolean", "jsonb"] {
            assert!(!is_numeric(t), "{t}");
        }
    }

    #[test]
    fn width_fits_the_widest_sampled_value() {
        let c = col("placed_at", "timestamp with time zone", false);
        let data = rows(&[Some("2026-09-21 04:51:04.856157+00"), Some("2026-01-01 00:00:00+00")]);
        let w = px_value(column_width(&c, false, &data, 0));
        assert!(w >= 29.0 * 8.0, "a 29-character timestamp must not be truncated, got {w}");
    }

    #[test]
    fn width_never_clips_the_header_even_for_narrow_data() {
        let c = col("customer_id", "bigint", false);
        let narrow = rows(&[Some("1"), Some("22")]);
        let with_link = px_value(column_width(&c, true, &narrow, 0));
        let without = px_value(column_width(&c, false, &narrow, 0));
        assert!(without >= "customer_id".len() as f32 * 8.5, "header text must fit: {without}");
        assert!(with_link > without, "the link icon needs room too");
    }

    #[test]
    fn width_is_clamped_and_handles_empty_tables_and_nulls() {
        let c = col("body", "text", false);
        let huge = rows(&[Some(&"x".repeat(5000))]);
        assert_eq!(px_value(column_width(&c, false, &huge, 0)), 420.0);
        let tiny = col("a", "int", false);
        assert_eq!(px_value(column_width(&tiny, false, &[], 0)), 80.0, "empty table falls back to the header");
        let nulls = rows(&[None, None]);
        assert!(px_value(column_width(&tiny, false, &nulls, 0)) >= 80.0);
    }

    #[test]
    fn byte_sizes_read_like_pg_size_pretty() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(1023), "1023 bytes");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(884_383), "864 KB");
        assert_eq!(format_bytes(30 * 1024 * 1024), "30.0 MB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.5 GB");
    }

    #[test]
    fn large_cells_say_stored_and_use_the_short_type_name() {
        assert_eq!(large_label("jsonb", 884_383), "jsonb · 864 KB stored");
        assert_eq!(large_label("pgb.custom_type", 20_000), "custom_type · 19.5 KB stored");
        assert_eq!(large_label("text[]", 9_000), "text[] · 8.8 KB stored");
    }

    #[test]
    fn a_large_cell_reserves_room_for_its_label() {
        let c = col("doc", "jsonb", false);
        let large = vec![vec![Cell::Large { stored_bytes: 5_000_000 }]];
        assert!(px_value(column_width(&c, false, &large, 0)) >= 26.0 * 8.0);
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(2500), "2,500");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(-4200), "-4,200");
    }

    #[test]
    fn range_label_covers_every_count_state() {
        assert_eq!(range_label(0, 500, &Total::Exact(2500), None, true), "Rows 1–500 of 2,500");
        assert_eq!(range_label(500, 500, &Total::Exact(2500), None, true), "Rows 501–1,000 of 2,500");
        assert_eq!(range_label(0, 500, &Total::Counting, None, true), "Rows 1–500 of …");
        assert_eq!(range_label(0, 500, &Total::Counting, Some(2500), true), "Rows 1–500 of ~2.5k…");
        // A filtered view must not show the unfiltered table estimate.
        assert_eq!(range_label(0, 500, &Total::Counting, Some(2500), false), "Rows 1–500 of …");
        assert_eq!(range_label(0, 500, &Total::Estimated(Some(2_500_000)), None, true), "Rows 1–500 of ~2.5M");
        assert_eq!(range_label(0, 500, &Total::Estimated(None), None, true), "Rows 1–500+");
        assert_eq!(range_label(0, 0, &Total::Exact(0), None, true), "No rows");
    }

    // -- staged edits

    #[test]
    fn setting_a_cell_back_to_its_original_unstages_it() {
        let mut p = Pending::default();
        let original = text("a@b.c");
        p.set_cell(3, 1, 2, NewValue::Text("new".into()), &original);
        assert_eq!(p.change_count(), 1);
        p.set_cell(3, 1, 2, NewValue::Text("a@b.c".into()), &original);
        assert!(p.is_empty(), "reverting to the original leaves nothing to save");
        // NULL over NULL is also not a change.
        p.set_cell(3, 0, 0, NewValue::Null, &Cell::Null);
        assert!(p.is_empty());
        p.set_cell(3, 0, 0, NewValue::Null, &text("x"));
        assert!(!p.is_empty());
    }

    #[test]
    fn change_count_counts_rows_not_cells_and_a_delete_supersedes_edits() {
        let mut p = Pending::default();
        p.set_cell(5, 0, 1, NewValue::Text("a".into()), &Cell::Null);
        p.set_cell(5, 0, 2, NewValue::Text("b".into()), &Cell::Null);
        p.set_cell(5, 1, 1, NewValue::Text("c".into()), &Cell::Null);
        assert_eq!(p.change_count(), 2, "two rows edited");
        p.toggle_delete(0);
        assert_eq!(p.change_count(), 2, "row 0 is now a delete, row 1 an update");
        p.add_row(5, 3);
        assert_eq!(p.change_count(), 3);
        assert!(!p.toggle_delete(0), "second toggle unmarks");
    }

    #[test]
    fn draft_rows_are_addressed_after_the_existing_rows() {
        let mut p = Pending::default();
        let first = p.add_row(4, 3);
        let second = p.add_row(4, 3);
        assert_eq!((first, second), (4, 5));
        p.set_cell(4, 5, 1, NewValue::Text("x".into()), &Cell::Null);
        assert_eq!(p.inserts[1][1], Some(NewValue::Text("x".into())));
        p.clear_cell(4, 5, 1);
        assert_eq!(p.inserts[1][1], None);
        p.remove_draft(4, 4);
        assert_eq!(p.inserts.len(), 1);
        p.remove_draft(4, 2); // an existing row index is never a draft
        assert_eq!(p.inserts.len(), 1);
    }

    // -- read-only rules

    fn access(select: bool, insert: bool, update: bool, delete: bool, kind: &str) -> Access {
        Access { select, insert, update, delete, relkind: kind.into() }
    }

    #[test]
    fn tables_with_a_key_and_privileges_are_editable() {
        let cols = [col("id", "bigint", true), col("name", "text", false)];
        let e = editability(&access(true, true, true, true, "r"), &cols);
        assert_eq!(e, Editability { read_only_reason: None, insert: true, update: true, delete: true });
        // Partial privileges are respected per operation.
        let e = editability(&access(true, false, true, false, "r"), &cols);
        assert_eq!((e.read_only_reason, e.insert, e.update, e.delete), (None, false, true, false));
        assert!(editability(&access(true, true, true, true, "p"), &cols).read_only_reason.is_none(), "partitioned");
    }

    #[test]
    fn views_missing_keys_and_missing_privileges_explain_why_they_are_read_only() {
        let keyed = [col("id", "bigint", true)];
        let unkeyed = [col("id", "bigint", false)];
        for kind in ["v", "m", "f"] {
            let e = editability(&access(true, true, true, true, kind), &keyed);
            assert!(e.read_only_reason.as_deref().unwrap().contains("read-only"), "{kind}");
            assert!(!e.insert && !e.update && !e.delete);
        }
        let e = editability(&access(true, true, true, true, "r"), &unkeyed);
        assert!(e.read_only_reason.unwrap().contains("primary key"));
        let e = editability(&access(true, false, false, false, "r"), &keyed);
        assert!(e.read_only_reason.unwrap().contains("privileges"));
    }

    // -- building the edit set

    fn sample_columns() -> Vec<Column> {
        vec![
            col("id", "bigint", true),
            col("name", "text", false),
            Column { generated: true, ..col("total", "numeric", false) },
            Column { identity: Identity::Always, ..col("seq", "int", false) },
        ]
    }

    fn sample_rows() -> Vec<Vec<Cell>> {
        vec![
            vec![text("1"), text("a"), text("0"), text("1")],
            vec![text("2"), Cell::Null, text("0"), text("2")],
            vec![text("3"), text("c"), text("0"), text("3")],
        ]
    }

    #[test]
    fn edit_set_orders_updates_then_deletes_then_inserts_and_keys_rows_by_primary_key() {
        let mut p = Pending::default();
        p.set_cell(3, 1, 1, NewValue::Text("named".into()), &Cell::Null);
        p.set_cell(3, 0, 1, NewValue::Null, &text("a"));
        p.toggle_delete(2);
        let d = p.add_row(3, 4);
        p.set_cell(3, d, 1, NewValue::Text("fresh".into()), &Cell::Null);
        let set = build_edit_set("shop", "items", &sample_columns(), &sample_rows(), &p).unwrap();
        assert_eq!((set.schema.as_str(), set.table.as_str()), ("shop", "items"));
        let kinds: Vec<&str> = set
            .changes
            .iter()
            .map(|c| match c {
                RowChange::Update { .. } => "update",
                RowChange::Delete { .. } => "delete",
                RowChange::Insert { .. } => "insert",
            })
            .collect();
        assert_eq!(kinds, ["update", "update", "delete", "insert"]);
        let RowChange::Update { key, changes } = &set.changes[0] else { panic!() };
        assert_eq!(key, &vec![("id".to_string(), "1".to_string())]);
        assert_eq!(changes[0].old, text("a"), "the old value the user saw is kept for the conflict check");
        let RowChange::Insert { values } = &set.changes[3] else { panic!() };
        assert_eq!(values, &vec![("name".to_string(), NewValue::Text("fresh".into()))], "only set cells are sent");
    }

    #[test]
    fn a_deleted_row_is_not_also_updated() {
        let mut p = Pending::default();
        p.set_cell(3, 1, 1, NewValue::Text("edited".into()), &Cell::Null);
        p.toggle_delete(1);
        let set = build_edit_set("s", "t", &sample_columns(), &sample_rows(), &p).unwrap();
        assert_eq!(set.changes.len(), 1);
        assert!(matches!(set.changes[0], RowChange::Delete { .. }));
    }

    #[test]
    fn a_row_whose_key_is_not_loaded_cannot_be_edited() {
        let mut rows = sample_rows();
        rows[0][0] = Cell::Large { stored_bytes: 1 };
        let mut p = Pending::default();
        p.toggle_delete(0);
        let err = build_edit_set("s", "t", &sample_columns(), &rows, &p).unwrap_err();
        assert_eq!(err.title, "Can't identify a row");
    }

    #[test]
    fn staging_against_a_row_that_no_longer_exists_is_reported() {
        let mut p = Pending::default();
        p.toggle_delete(9);
        assert_eq!(build_edit_set("s", "t", &sample_columns(), &sample_rows(), &p).unwrap_err().title, "Stale edit");
    }
}
