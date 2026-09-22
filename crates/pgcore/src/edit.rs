//! Turning staged grid edits into SQL and applying them atomically.
//!
//! Every value travels as a bound text parameter cast to the column's type, identifiers are always
//! quoted, and a row is identified by its full primary key. Updates are optimistic: the WHERE clause
//! also requires each *changed* cell to still hold the value the user saw, so a concurrent change is
//! reported as a conflict instead of being overwritten. Unchanged cells (including huge ones) are
//! never compared or sent.

use crate::config::ConnectionParams;
use crate::data::{Cell, Column};
use crate::error::{ErrorKind, UserFacingError};
use crate::session::Session;
use crate::sql::{quote_ident, quote_qualified};
use std::fmt::Write as _;

/// Primary-key column name → value (text form).
pub type Key = Vec<(String, String)>;

const ALIAS: &str = "pgb_t";
/// Values longer than this are abbreviated in the human-readable preview.
const PREVIEW_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewValue {
    Null,
    /// The column's `DEFAULT` (identity/serial next value, default expression).
    Default,
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellChange {
    pub column: String,
    /// What the user saw; used as the optimistic-concurrency guard.
    pub old: Cell,
    pub new: NewValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowChange {
    Update { key: Key, changes: Vec<CellChange> },
    Insert { values: Vec<(String, NewValue)> },
    Delete { key: Key },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSet {
    pub schema: String,
    pub table: String,
    pub changes: Vec<RowChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Update,
    Insert,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub kind: StatementKind,
    pub sql: String,
    pub params: Vec<String>,
    /// The statement with parameters shown as literals, for the review panel. Never executed.
    pub display: String,
    /// Index into [`EditSet::changes`], for error reporting.
    pub change_index: usize,
}

struct Builder {
    sql: String,
    display: String,
    params: Vec<String>,
}

impl Builder {
    fn new() -> Self {
        Self { sql: String::new(), display: String::new(), params: Vec::new() }
    }

    fn raw(&mut self, text: &str) {
        self.sql.push_str(text);
        self.display.push_str(text);
    }

    /// `CAST($n::text AS type)`.
    fn value(&mut self, value: &str, type_name: &str) {
        self.params.push(value.to_string());
        let n = self.params.len();
        let _ = write!(self.sql, "CAST(${n}::text AS {type_name})");
        let _ = write!(self.display, "CAST({} AS {type_name})", display_literal(value));
    }

    /// `$n::text`, for comparing text forms.
    fn text(&mut self, value: &str) {
        self.params.push(value.to_string());
        let n = self.params.len();
        let _ = write!(self.sql, "${n}::text");
        self.display.push_str(&display_literal(value));
    }

    fn new_value(&mut self, v: &NewValue, type_name: &str) {
        match v {
            NewValue::Null => self.raw("NULL"),
            NewValue::Default => self.raw("DEFAULT"),
            NewValue::Text(t) => self.value(t, type_name),
        }
    }
}

/// Renders a value as a SQL string literal **for display only**; long values are abbreviated.
pub fn display_literal(value: &str) -> String {
    let chars = value.chars().count();
    if chars > PREVIEW_MAX_CHARS {
        let head: String = value.chars().take(60).collect();
        format!("'{}…' /* {} chars */", head.replace('\'', "''"), chars)
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

fn column<'a>(columns: &'a [Column], name: &str) -> Result<&'a Column, UserFacingError> {
    columns.iter().find(|c| c.name == name).ok_or_else(|| {
        UserFacingError::config("Unknown column", format!("This table has no column \"{name}\"."))
    })
}

fn writable<'a>(columns: &'a [Column], name: &str) -> Result<&'a Column, UserFacingError> {
    let c = column(columns, name)?;
    if !c.is_writable() {
        return Err(UserFacingError::config(
            "Column can't be edited",
            format!(
                "\"{name}\" is {} and its value is produced by the database.",
                if c.generated { "a generated column" } else { "an always-generated identity column" }
            ),
        ));
    }
    Ok(c)
}

/// The key must name exactly the primary-key columns: anything else could touch several rows.
fn check_key(key: &Key, columns: &[Column]) -> Result<(), UserFacingError> {
    let mut pk: Vec<&str> = columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.as_str()).collect();
    if pk.is_empty() {
        return Err(UserFacingError::config(
            "No primary key",
            "Rows of a table without a primary key can't be identified safely, so it is read-only here.",
        ));
    }
    let mut given: Vec<&str> = key.iter().map(|(c, _)| c.as_str()).collect();
    pk.sort_unstable();
    given.sort_unstable();
    if pk != given {
        return Err(UserFacingError::config(
            "Row identity mismatch",
            format!("The row key must consist of exactly the primary-key columns ({}).", pk.join(", ")),
        ));
    }
    Ok(())
}

fn key_predicate(b: &mut Builder, key: &Key, columns: &[Column]) -> Result<(), UserFacingError> {
    for (i, (name, value)) in key.iter().enumerate() {
        if i > 0 {
            b.raw(" AND ");
        }
        let c = column(columns, name)?;
        b.raw(&format!("{}.{} = ", quote_ident(ALIAS), quote_ident(name)));
        b.value(value, &c.type_name);
    }
    Ok(())
}

fn is_noop(c: &CellChange) -> bool {
    matches!((&c.old, &c.new), (Cell::Text(o), NewValue::Text(n)) if o == n)
        || matches!((&c.old, &c.new), (Cell::Null, NewValue::Null))
}

/// Builds the statements for an edit set. Nothing is sent to the server; invalid edits are
/// rejected here, before any connection is opened.
pub fn build(edit: &EditSet, columns: &[Column]) -> Result<Vec<Statement>, UserFacingError> {
    let table = quote_qualified(&edit.schema, &edit.table);
    let mut out = Vec::new();
    for (change_index, change) in edit.changes.iter().enumerate() {
        let mut b = Builder::new();
        let kind = match change {
            RowChange::Update { key, changes } => {
                check_key(key, columns)?;
                // A change back to the value the cell already holds is a no-op.
                let effective: Vec<&CellChange> = changes.iter().filter(|c| !is_noop(c)).collect();
                if effective.is_empty() {
                    continue;
                }
                b.raw(&format!("UPDATE {table} AS {} SET ", quote_ident(ALIAS)));
                for (i, c) in effective.iter().enumerate() {
                    let col = writable(columns, &c.column)?;
                    if c.old.is_large() {
                        return Err(UserFacingError::config(
                            "Value too large to edit here",
                            format!(
                                "\"{}\" holds a large value that is not loaded; open it in the value editor.",
                                c.column
                            ),
                        ));
                    }
                    if i > 0 {
                        b.raw(", ");
                    }
                    b.raw(&format!("{} = ", quote_ident(&c.column)));
                    b.new_value(&c.new, &col.type_name);
                }
                b.raw(" WHERE ");
                key_predicate(&mut b, key, columns)?;
                // Optimistic guard on the changed cells only, compared in text form (json has no `=`).
                for c in &effective {
                    let col = format!("{}.{}", quote_ident(ALIAS), quote_ident(&c.column));
                    match &c.old {
                        Cell::Null => b.raw(&format!(" AND {col} IS NULL")),
                        Cell::Text(old) => {
                            b.raw(&format!(" AND {col}::text IS NOT DISTINCT FROM "));
                            b.text(old);
                        }
                        Cell::Large { .. } => unreachable!("rejected above"),
                    }
                }
                StatementKind::Update
            }
            RowChange::Insert { values } => {
                let mut cols = Vec::new();
                for (name, value) in values {
                    let col = writable(columns, name)?;
                    cols.push((col, value));
                }
                if cols.is_empty() {
                    b.raw(&format!("INSERT INTO {table} DEFAULT VALUES"));
                } else {
                    let names = cols.iter().map(|(c, _)| quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
                    b.raw(&format!("INSERT INTO {table} ({names}) VALUES ("));
                    for (i, (col, value)) in cols.iter().enumerate() {
                        if i > 0 {
                            b.raw(", ");
                        }
                        b.new_value(value, &col.type_name);
                    }
                    b.raw(")");
                }
                StatementKind::Insert
            }
            RowChange::Delete { key } => {
                check_key(key, columns)?;
                b.raw(&format!("DELETE FROM {table} AS {} WHERE ", quote_ident(ALIAS)));
                key_predicate(&mut b, key, columns)?;
                StatementKind::Delete
            }
        };
        out.push(Statement { kind, sql: b.sql, params: b.params, display: b.display, change_index });
    }
    Ok(out)
}

/// The review text shown before submitting: one statement per line, values as literals.
pub fn preview(edit: &EditSet, columns: &[Column]) -> Result<Vec<String>, UserFacingError> {
    Ok(build(edit, columns)?.into_iter().map(|s| format!("{};", s.display)).collect())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub updated: usize,
    pub inserted: usize,
    pub deleted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// Rejected before anything was sent.
    Invalid(UserFacingError),
    /// A statement matched no row: the row was changed or deleted since it was loaded. Everything
    /// was rolled back.
    Conflict { change_index: usize, kind: StatementKind },
    /// The database refused (constraint, permission, connection...). Everything was rolled back.
    Failed { change_index: Option<usize>, error: UserFacingError },
}

impl From<ApplyError> for UserFacingError {
    fn from(err: ApplyError) -> Self {
        match err {
            ApplyError::Invalid(e) => e,
            ApplyError::Failed { error, .. } => error,
            ApplyError::Conflict { kind, .. } => UserFacingError {
                kind: ErrorKind::Conflict,
                title: "Row changed or deleted".into(),
                detail: match kind {
                    StatementKind::Delete => "A row you deleted no longer exists.".into(),
                    _ => "A row you edited was changed or deleted by someone else since you loaded it."
                        .into(),
                },
                hint: Some("Nothing was saved. Reload the table, then repeat your edits.".into()),
                sqlstate: None,
                position: None,
                retryable: false,
                raw: String::new(),
            },
        }
    }
}

/// Applies all changes in one transaction on a fresh connection (so nothing else can interleave).
/// Any failure or conflict rolls everything back.
pub async fn apply(
    params: &ConnectionParams,
    edit: &EditSet,
    columns: &[Column],
) -> Result<ApplyReport, ApplyError> {
    let statements = build(edit, columns).map_err(ApplyError::Invalid)?;
    if statements.is_empty() {
        return Ok(ApplyReport::default());
    }
    let session = Session::connect(params)
        .await
        .map_err(|error| ApplyError::Failed { change_index: None, error })?;
    let client = session.client();
    let target = session.info().endpoint.clone();
    let fail = |e: tokio_postgres::Error, change_index: Option<usize>| ApplyError::Failed {
        change_index,
        error: UserFacingError::from_pg(&e, Some(&target)),
    };

    client.batch_execute("BEGIN").await.map_err(|e| fail(e, None))?;
    let mut report = ApplyReport::default();
    for st in &statements {
        let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            st.params.iter().map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
        match client.execute(&st.sql, &refs).await {
            Ok(1) => match st.kind {
                StatementKind::Update => report.updated += 1,
                StatementKind::Insert => report.inserted += 1,
                StatementKind::Delete => report.deleted += 1,
            },
            Ok(_) => {
                let _ = client.batch_execute("ROLLBACK").await;
                return Err(ApplyError::Conflict { change_index: st.change_index, kind: st.kind });
            }
            Err(e) => {
                let _ = client.batch_execute("ROLLBACK").await;
                return Err(fail(e, Some(st.change_index)));
            }
        }
    }
    if let Err(e) = client.batch_execute("COMMIT").await {
        let _ = client.batch_execute("ROLLBACK").await;
        return Err(fail(e, None));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Identity;

    fn col(name: &str, ty: &str, pk: bool) -> Column {
        Column { name: name.into(), type_name: ty.into(), is_primary_key: pk, ..Default::default() }
    }

    fn cols() -> Vec<Column> {
        vec![
            col("id", "bigint", true),
            col("email", "text", false),
            col("price", "numeric(10,2)", false),
            col("doc", "jsonb", false),
        ]
    }

    fn key(id: &str) -> Key {
        vec![("id".into(), id.into())]
    }

    fn update(changes: Vec<CellChange>) -> EditSet {
        EditSet {
            schema: "shop".into(),
            table: "customers".into(),
            changes: vec![RowChange::Update { key: key("7"), changes }],
        }
    }

    fn change(column: &str, old: Cell, new: NewValue) -> CellChange {
        CellChange { column: column.into(), old, new }
    }

    #[test]
    fn update_sets_values_and_guards_the_changed_cells_only() {
        let edit = update(vec![
            change("email", Cell::Text("a@b.c".into()), NewValue::Text("new@b.c".into())),
            change("price", Cell::Null, NewValue::Default),
        ]);
        let st = &build(&edit, &cols()).unwrap()[0];
        assert_eq!(
            st.sql,
            "UPDATE \"shop\".\"customers\" AS \"pgb_t\" SET \"email\" = CAST($1::text AS text), \"price\" = DEFAULT \
             WHERE \"pgb_t\".\"id\" = CAST($2::text AS bigint) \
             AND \"pgb_t\".\"email\"::text IS NOT DISTINCT FROM $3::text \
             AND \"pgb_t\".\"price\" IS NULL"
        );
        assert_eq!(st.params, ["new@b.c", "7", "a@b.c"]);
        assert_eq!(st.kind, StatementKind::Update);
    }

    #[test]
    fn setting_null_and_editing_json_compare_in_text_form() {
        let edit = update(vec![change("doc", Cell::Text("{\"a\": 1}".into()), NewValue::Null)]);
        let st = &build(&edit, &cols()).unwrap()[0];
        assert!(st.sql.contains("SET \"doc\" = NULL"), "{}", st.sql);
        assert!(st.sql.contains("\"pgb_t\".\"doc\"::text IS NOT DISTINCT FROM $2::text"), "{}", st.sql);
    }

    #[test]
    fn values_are_parameters_never_sql_text_and_previews_escape_quotes() {
        let evil = "x'); DROP TABLE customers; --";
        let edit = update(vec![change("email", Cell::Text("old".into()), NewValue::Text(evil.into()))]);
        let st = &build(&edit, &cols()).unwrap()[0];
        assert!(!st.sql.contains("DROP"), "the value must never appear in the executed SQL: {}", st.sql);
        assert!(st.params.contains(&evil.to_string()));
        assert!(st.display.contains("'x''); DROP TABLE customers; --'"), "display escapes quotes: {}", st.display);
    }

    #[test]
    fn noop_changes_are_dropped_and_an_all_noop_row_produces_no_statement() {
        let same = change("email", Cell::Text("a".into()), NewValue::Text("a".into()));
        let null_to_null = change("price", Cell::Null, NewValue::Null);
        assert!(build(&update(vec![same.clone(), null_to_null]), &cols()).unwrap().is_empty());
        let real = change("email", Cell::Text("a".into()), NewValue::Text("b".into()));
        let st = &build(&update(vec![same, real]), &cols()).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].sql.matches("SET").count(), 1);
        assert!(!st[0].sql.contains("\"price\""));
    }

    #[test]
    fn generated_and_always_identity_columns_cannot_be_written() {
        let mut c = cols();
        c[1].generated = true;
        c[2].identity = Identity::Always;
        for column in ["email", "price"] {
            let edit = update(vec![change(column, Cell::Null, NewValue::Text("1".into()))]);
            let err = build(&edit, &c).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Config, "{column}");
        }
        let insert = EditSet {
            schema: "s".into(),
            table: "t".into(),
            changes: vec![RowChange::Insert { values: vec![("price".into(), NewValue::Default)] }],
        };
        assert!(build(&insert, &c).is_err());
        // BY DEFAULT identity is writable.
        c[2].identity = Identity::ByDefault;
        assert!(build(&update(vec![change("price", Cell::Null, NewValue::Text("1".into()))]), &c).is_ok());
    }

    #[test]
    fn the_key_must_be_exactly_the_primary_key() {
        let mut c = cols();
        let delete = |k: Key| EditSet {
            schema: "s".into(),
            table: "t".into(),
            changes: vec![RowChange::Delete { key: k }],
        };
        assert!(build(&delete(vec![]), &c).is_err(), "empty key");
        assert!(build(&delete(vec![("email".into(), "x".into())]), &c).is_err(), "non-key column");
        assert!(
            build(&delete(vec![("id".into(), "1".into()), ("email".into(), "x".into())]), &c).is_err(),
            "extra column"
        );
        assert!(build(&delete(key("1")), &c).is_ok());
        // Composite key needs every member.
        c[1].is_primary_key = true;
        assert!(build(&delete(key("1")), &c).is_err(), "half of a composite key");
        assert!(build(&delete(vec![("id".into(), "1".into()), ("email".into(), "x".into())]), &c).is_ok());
        // No primary key at all: refused, with a clear reason.
        let none: Vec<Column> = cols()
            .into_iter()
            .map(|mut c| {
                c.is_primary_key = false;
                c
            })
            .collect();
        let err = build(&delete(key("1")), &none).unwrap_err();
        assert_eq!(err.title, "No primary key");
    }

    #[test]
    fn a_large_unloaded_cell_cannot_be_edited_in_the_grid() {
        let edit = update(vec![change("doc", Cell::Large { stored_bytes: 5_000_000 }, NewValue::Text("{}".into()))]);
        let err = build(&edit, &cols()).unwrap_err();
        assert_eq!(err.title, "Value too large to edit here");
    }

    #[test]
    fn unknown_columns_are_rejected() {
        let edit = update(vec![change("nope\"; DROP", Cell::Null, NewValue::Text("1".into()))]);
        assert_eq!(build(&edit, &cols()).unwrap_err().kind, ErrorKind::Config);
    }

    #[test]
    fn inserts_use_defaults_and_typed_parameters() {
        let insert = |values: Vec<(String, NewValue)>| EditSet {
            schema: "shop".into(),
            table: "customers".into(),
            changes: vec![RowChange::Insert { values }],
        };
        let st = &build(
            &insert(vec![
                ("email".into(), NewValue::Text("n@x.y".into())),
                ("price".into(), NewValue::Default),
                ("doc".into(), NewValue::Null),
            ]),
            &cols(),
        )
        .unwrap()[0];
        assert_eq!(
            st.sql,
            "INSERT INTO \"shop\".\"customers\" (\"email\", \"price\", \"doc\") VALUES (CAST($1::text AS text), DEFAULT, NULL)"
        );
        assert_eq!(st.params, ["n@x.y"]);
        let st = &build(&insert(vec![]), &cols()).unwrap()[0];
        assert_eq!(st.sql, "INSERT INTO \"shop\".\"customers\" DEFAULT VALUES");
    }

    #[test]
    fn deletes_match_on_the_whole_key() {
        let mut c = cols();
        c[1].is_primary_key = true;
        let edit = EditSet {
            schema: "shop".into(),
            table: "customers".into(),
            changes: vec![RowChange::Delete {
                key: vec![("id".into(), "9".into()), ("email".into(), "a@b.c".into())],
            }],
        };
        let st = &build(&edit, &c).unwrap()[0];
        assert_eq!(
            st.sql,
            "DELETE FROM \"shop\".\"customers\" AS \"pgb_t\" WHERE \"pgb_t\".\"id\" = CAST($1::text AS bigint) \
             AND \"pgb_t\".\"email\" = CAST($2::text AS text)"
        );
        assert_eq!(st.params, ["9", "a@b.c"]);
    }

    #[test]
    fn statements_keep_the_order_and_index_of_the_staged_changes() {
        let edit = EditSet {
            schema: "s".into(),
            table: "t".into(),
            changes: vec![
                RowChange::Insert { values: vec![] },
                RowChange::Update {
                    key: key("1"),
                    changes: vec![change("email", Cell::Null, NewValue::Text("x".into()))],
                },
                RowChange::Delete { key: key("2") },
            ],
        };
        let sts = build(&edit, &cols()).unwrap();
        assert_eq!(
            sts.iter().map(|s| (s.change_index, s.kind)).collect::<Vec<_>>(),
            [(0, StatementKind::Insert), (1, StatementKind::Update), (2, StatementKind::Delete)]
        );
    }

    #[test]
    fn previews_abbreviate_huge_values() {
        let big = "z".repeat(10_000);
        let edit = update(vec![change("email", Cell::Text("a".into()), NewValue::Text(big.clone()))]);
        let lines = preview(&edit, &cols()).unwrap();
        assert!(lines[0].len() < 600, "preview must not embed the payload: {} bytes", lines[0].len());
        assert!(lines[0].contains("/* 10000 chars */"));
        assert!(lines[0].ends_with(';'));
        // The executed statement still carries the full value.
        assert_eq!(build(&edit, &cols()).unwrap()[0].params[0], big);
    }

    #[test]
    fn conflict_and_failure_convert_to_user_facing_errors() {
        let c: UserFacingError = ApplyError::Conflict { change_index: 0, kind: StatementKind::Update }.into();
        assert_eq!(c.kind, ErrorKind::Conflict);
        assert!(c.hint.unwrap().contains("Nothing was saved"));
        let d: UserFacingError = ApplyError::Conflict { change_index: 0, kind: StatementKind::Delete }.into();
        assert!(d.detail.contains("deleted"));
    }
}
