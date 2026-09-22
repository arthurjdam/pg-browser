//! Lazy, size-bounded access to `jsonb` values: describing a node, paging its children, reading a
//! leaf's text, and writing one path at a time — so a multi-gigabyte document can be browsed and
//! edited without ever transferring more of it than what is actually being looked at or changed.
//!
//! Every "preview" and read here is guarded by [`data::INLINE_STORED_LIMIT`] exactly like the grid's
//! cell guard in [`crate::data`], and every write is a single, targeted SQL expression
//! (`jsonb_set` / `#-`) bound with an optimistic-concurrency check on `xmin` — the whole document is
//! never fetched to change one key, and never re-sent to change one key either.
//!
//! [`describe_node`] and [`list_children`] work on any path of a `jsonb` column. [`set_at_path`] and
//! [`delete_at_path`] write one path. A value's *text* is always its JSON text form (`'"hi"'` for the
//! string `hi`, not the unquoted content): editing a node means editing valid JSON text, which is
//! what [`set_at_path`] validates before it ever reaches the server.

use crate::data::{self, CellFetch, Column};
use crate::error::UserFacingError;
use crate::session::Session;
use crate::sql::{quote_ident, quote_qualified};
use tokio_postgres::types::ToSql;

const ALIAS: &str = "pgb_t";
/// How many children [`list_children`] returns per page.
pub const CHILD_PAGE_SIZE: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSegment {
    Key(String),
    Index(i64),
}

impl PathSegment {
    /// Text form of one path array element, as `jsonb_set` / `#>` / `#-` expect it: an object key
    /// verbatim, or an array index as a decimal number (Postgres treats a numeric-looking text[]
    /// element as an index when the current level is an array).
    fn to_sql(&self) -> String {
        match self {
            PathSegment::Key(k) => k.clone(),
            PathSegment::Index(i) => i.to_string(),
        }
    }
}

pub type JsonPath = [PathSegment];

fn path_params(path: &JsonPath) -> Vec<String> {
    path.iter().map(PathSegment::to_sql).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Null,
    Bool,
    Number,
    String,
    Object,
    Array,
}

fn parse_kind(typeof_text: &str) -> Result<NodeKind, UserFacingError> {
    Ok(match typeof_text {
        "null" => NodeKind::Null,
        "boolean" => NodeKind::Bool,
        "number" => NodeKind::Number,
        "string" => NodeKind::String,
        "object" => NodeKind::Object,
        "array" => NodeKind::Array,
        other => {
            return Err(UserFacingError::config(
                "Unrecognised value",
                format!("The server reported an unknown jsonb type `{other}`."),
            ));
        }
    })
}

impl NodeKind {
    pub fn is_container(self) -> bool {
        matches!(self, NodeKind::Object | NodeKind::Array)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeInfo {
    pub kind: NodeKind,
    /// `pg_column_size` of the sub-value: on-disk (often compressed) bytes, not its text length.
    pub stored_bytes: i64,
    /// Key or element count, for `Object` / `Array`.
    pub count: Option<i64>,
    /// The value's JSON text (e.g. `"hi"`, `42`, `true`), present only for a scalar whose stored
    /// size is within [`data::INLINE_STORED_LIMIT`]. `None` for containers, and for an oversized
    /// scalar (read it with [`read_node_text`]).
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeLookup {
    /// The row this path belongs to no longer exists.
    RowMissing,
    /// The path does not exist (or the value there is SQL NULL, e.g. an unset column).
    Missing,
    Found(NodeInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildKey {
    Key(String),
    Index(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Child {
    pub key: ChildKey,
    pub info: NodeInfo,
}

struct KeyPredicate {
    sql: String,
    params: Vec<String>,
}

/// `alias.col1 = CAST($n::text AS type) AND ...`, starting numbering at `start`. Mirrors
/// `edit::key_predicate`; kept separate because this module's statements interleave the key
/// parameters with a path array and other parameters in shapes that vary per function.
fn key_predicate(columns: &[Column], key: &[(String, String)], start: usize) -> Result<KeyPredicate, UserFacingError> {
    let mut sql = String::new();
    let mut params = Vec::with_capacity(key.len());
    for (i, (name, value)) in key.iter().enumerate() {
        if i > 0 {
            sql.push_str(" AND ");
        }
        let c = column(columns, name)?;
        let n = start + params.len();
        sql.push_str(&format!("{}.{} = CAST(${n}::text AS {})", quote_ident(ALIAS), quote_ident(name), c.type_name));
        params.push(value.clone());
    }
    Ok(KeyPredicate { sql, params })
}

fn column<'a>(columns: &'a [Column], name: &str) -> Result<&'a Column, UserFacingError> {
    columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| UserFacingError::config("Unknown column", format!("This table has no column \"{name}\".")))
}

/// The named column, and an error if it is not `jsonb` (the tree and node-path writes only work on
/// `jsonb`; a whole-value replace via [`set_at_path`] with an empty path works on any type).
fn jsonb_column<'a>(columns: &'a [Column], name: &str) -> Result<&'a Column, UserFacingError> {
    let c = column(columns, name)?;
    if c.type_name != "jsonb" {
        return Err(UserFacingError::config(
            "Not a jsonb column",
            format!("\"{name}\" is {}. Only jsonb columns support browsing and editing a nested path.", c.type_name),
        ));
    }
    Ok(c)
}

fn refs<'a>(params: &'a [&'a (dyn ToSql + Sync)]) -> &'a [&'a (dyn ToSql + Sync)] {
    params
}

/// Looks up the node at `path` within `column` (a jsonb value) of the row identified by `key`.
pub async fn describe_node(
    session: &Session,
    schema: &str,
    table: &str,
    columns: &[Column],
    key: &[(String, String)],
    column_name: &str,
    path: &JsonPath,
) -> Result<NodeLookup, UserFacingError> {
    jsonb_column(columns, column_name)?;
    let target = session.info().endpoint.clone();
    let kp = key_predicate(columns, key, 2)?;
    let sql = format!(
        "SELECT jsonb_typeof(v), pg_column_size(v), \
                CASE WHEN jsonb_typeof(v) = 'object' THEN (SELECT count(*) FROM jsonb_object_keys(v)) \
                     WHEN jsonb_typeof(v) = 'array' THEN jsonb_array_length(v) END, \
                CASE WHEN jsonb_typeof(v) IN ('string','number','boolean','null') AND pg_column_size(v) <= {} \
                     THEN v::text END \
         FROM (SELECT {} #> $1::text[] AS v FROM {} AS {} WHERE {}) x",
        data::INLINE_STORED_LIMIT,
        quote_ident(column_name),
        quote_qualified(schema, table),
        ALIAS,
        kp.sql
    );
    let path_arr = path_params(path);
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&path_arr];
    params.extend(kp.params.iter().map(|s| s as &(dyn ToSql + Sync)));
    let rows = session.client().query(&sql, refs(&params)).await.map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
    let Some(row) = rows.first() else { return Ok(NodeLookup::RowMissing) };
    let Some(typeof_text): Option<String> = row.get(0) else { return Ok(NodeLookup::Missing) };
    let stored_bytes: i32 = row.get(1);
    // `count(*)` (bigint) and `jsonb_array_length` (integer) share this CASE, so Postgres widens
    // the whole expression to bigint.
    let count: Option<i64> = row.get(2);
    let preview: Option<String> = row.get(3);
    Ok(NodeLookup::Found(NodeInfo {
        kind: parse_kind(&typeof_text)?,
        stored_bytes: i64::from(stored_bytes),
        count,
        preview,
    }))
}

/// One page of an object's or array's children, plus whether more remain. `kind` must be the
/// container kind already learned from [`describe_node`] (this function does not re-check it: a
/// mismatch surfaces as an ordinary SQL error from `jsonb_each`/`jsonb_array_elements`).
pub async fn list_children(
    session: &Session,
    schema: &str,
    table: &str,
    columns: &[Column],
    key: &[(String, String)],
    column_name: &str,
    path: &JsonPath,
    kind: NodeKind,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Child>, bool), UserFacingError> {
    jsonb_column(columns, column_name)?;
    let target = session.info().endpoint.clone();
    let kp = key_predicate(columns, key, 2)?;
    let off_ix = 2 + kp.params.len();
    let lim_ix = off_ix + 1;
    // Arrays are deliberately left in `jsonb_array_elements`'s own (already-ascending) emission
    // order rather than sorted: `jsonb_array_elements` has no random-access "start at index k", so
    // OFFSET always costs O(offset) regardless, but an explicit `ORDER BY` would additionally force
    // Postgres to materialize and sort *every* element before applying LIMIT/OFFSET — turning even
    // page 1 of a huge array into an O(n) operation instead of O(offset). Objects keep `ORDER BY
    // key`: jsonb's internal key order (by length, then text) is not something a person can predict,
    // and objects big enough for that sort to matter are far rarer than large arrays in practice.
    let (select_key, from_kv, order) = match kind {
        NodeKind::Object => ("kv.key", "jsonb_each(x.v) AS kv(key, value)", Some("kv.key")),
        NodeKind::Array => ("(kv.ordinality - 1)::text", "jsonb_array_elements(x.v) WITH ORDINALITY AS kv(value, ordinality)", None),
        _ => {
            return Err(UserFacingError::config(
                "Not a container",
                "This value has no children to list.",
            ));
        }
    };
    let order_by = order.map(|o| format!(" ORDER BY {o}")).unwrap_or_default();
    let sql = format!(
        "SELECT {select_key}, jsonb_typeof(kv.value), pg_column_size(kv.value), \
                CASE WHEN jsonb_typeof(kv.value) IN ('string','number','boolean','null') AND pg_column_size(kv.value) <= {} \
                     THEN kv.value::text END \
         FROM (SELECT {} #> $1::text[] AS v FROM {} AS {} WHERE {}) x, {from_kv}{order_by} OFFSET ${off_ix} LIMIT ${lim_ix}",
        data::INLINE_STORED_LIMIT,
        quote_ident(column_name),
        quote_qualified(schema, table),
        ALIAS,
        kp.sql
    );
    let path_arr = path_params(path);
    let off = offset as i64;
    let lim = (limit + 1) as i64; // one extra row to detect `has_more` without a separate count
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&path_arr];
    params.extend(kp.params.iter().map(|s| s as &(dyn ToSql + Sync)));
    params.push(&off);
    params.push(&lim);
    let mut rows = session.client().query(&sql, refs(&params)).await.map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let children = rows
        .iter()
        .map(|r| {
            let key_text: String = r.get(0);
            let child_key = match kind {
                NodeKind::Object => ChildKey::Key(key_text),
                _ => ChildKey::Index(key_text.parse().unwrap_or_default()),
            };
            let typeof_text: String = r.get(1);
            let stored_bytes: i32 = r.get(2);
            let preview: Option<String> = r.get(3);
            Ok(Child {
                key: child_key,
                info: NodeInfo { kind: parse_kind(&typeof_text)?, stored_bytes: i64::from(stored_bytes), count: None, preview },
            })
        })
        .collect::<Result<_, UserFacingError>>()?;
    Ok((children, has_more))
}

/// Reads the full JSON text of the node at `path`, capped at `max_bytes` (plus one byte to detect
/// overflow). Use this to open a scalar whose [`NodeInfo::preview`] was `None` because it was over
/// the inline guard.
pub async fn read_node_text(
    session: &Session,
    schema: &str,
    table: &str,
    columns: &[Column],
    key: &[(String, String)],
    column_name: &str,
    path: &JsonPath,
    max_bytes: usize,
) -> Result<CellFetch, UserFacingError> {
    jsonb_column(columns, column_name)?;
    let target = session.info().endpoint.clone();
    let kp = key_predicate(columns, key, 3)?;
    let sql = format!(
        "SELECT substr(v, 1, $1), octet_length(v) \
         FROM (SELECT ({} #> $2::text[])::text AS v FROM {} AS {} WHERE {}) x",
        quote_ident(column_name),
        quote_qualified(schema, table),
        ALIAS,
        kp.sql
    );
    let cap = i32::try_from(max_bytes + 1).unwrap_or(i32::MAX);
    let path_arr = path_params(path);
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&cap, &path_arr];
    params.extend(kp.params.iter().map(|s| s as &(dyn ToSql + Sync)));
    let rows = session.client().query(&sql, refs(&params)).await.map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
    let Some(row) = rows.first() else { return Ok(CellFetch::RowMissing) };
    let prefix: Option<String> = row.get(0);
    let total: Option<i32> = row.get(1);
    let (Some(prefix), Some(total)) = (prefix, total) else { return Ok(CellFetch::Null) };
    if i64::from(total) > max_bytes as i64 {
        Ok(CellFetch::TooLarge { total_bytes: i64::from(total) })
    } else {
        Ok(CellFetch::Full(prefix))
    }
}

/// The row's current `xmin`, for the optimistic-concurrency check in [`set_at_path`] /
/// [`delete_at_path`]. `None` means the row does not exist.
pub async fn read_xmin(
    session: &Session,
    schema: &str,
    table: &str,
    columns: &[Column],
    key: &[(String, String)],
) -> Result<Option<String>, UserFacingError> {
    let target = session.info().endpoint.clone();
    let kp = key_predicate(columns, key, 1)?;
    let sql = format!(
        "SELECT {}.xmin::text FROM {} AS {} WHERE {}",
        ALIAS,
        quote_qualified(schema, table),
        ALIAS,
        kp.sql
    );
    let params: Vec<&(dyn ToSql + Sync)> = kp.params.iter().map(|s| s as &(dyn ToSql + Sync)).collect();
    let rows = session.client().query(&sql, refs(&params)).await.map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
    Ok(rows.first().map(|r| r.get(0)))
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetError {
    /// Rejected before anything was sent (bad column, bad JSON, path on a non-jsonb column, ...).
    Invalid(UserFacingError),
    /// No row matched the key with the expected `xmin`: it was changed or deleted since it was read.
    Conflict,
    Failed(UserFacingError),
}

impl From<SetError> for UserFacingError {
    fn from(err: SetError) -> Self {
        match err {
            SetError::Invalid(e) | SetError::Failed(e) => e,
            SetError::Conflict => UserFacingError {
                kind: crate::error::ErrorKind::Conflict,
                title: "Value changed".into(),
                detail: "This row was changed or deleted by someone else since you opened it.".into(),
                hint: Some("Nothing was saved. Close and reopen the value, then try again.".into()),
                sqlstate: None,
                position: None,
                retryable: false,
                raw: String::new(),
            },
        }
    }
}

fn invalid_json(text: &str) -> Result<(), SetError> {
    if let Err(e) = serde_json::from_str::<serde_json::Value>(text) {
        return Err(SetError::Invalid(UserFacingError::config("Invalid JSON", e.to_string())));
    }
    Ok(())
}

/// Writes `new_json_text` (JSON text, e.g. `"hi"`, `42`, `{"a":1}`) at `path`.
///
/// An empty path replaces the whole column, for any type it holds (not only jsonb — this is also
/// how a large `text`/`bytea`/plain `json` value is replaced): `CAST($1::text AS <column type>)`.
/// A non-empty path requires a `jsonb` column and writes via `jsonb_set(..., create_missing = true)`,
/// so setting a path that doesn't exist yet creates it (including appending to an array by using its
/// current length as the index).
///
/// Guarded by `expected_xmin` (from [`read_xmin`]): if the row changed since then, nothing is
/// written and [`SetError::Conflict`] is returned.
pub async fn set_at_path(
    session: &Session,
    schema: &str,
    table: &str,
    columns: &[Column],
    key: &[(String, String)],
    column_name: &str,
    path: &JsonPath,
    new_json_text: &str,
    expected_xmin: &str,
) -> Result<(), SetError> {
    let c = column(columns, column_name).map_err(SetError::Invalid)?;
    let is_json_type = c.type_name == "json" || c.type_name == "jsonb";
    let target = session.info().endpoint.clone();

    let (sql, kp, path_arr, value_needs_validation) = if path.is_empty() {
        let kp = key_predicate(columns, key, 2).map_err(SetError::Invalid)?;
        let xmin_ix = 2 + kp.params.len();
        let sql = format!(
            "UPDATE {} AS {} SET {} = CAST($1::text AS {}) WHERE {} AND {}.xmin::text = ${xmin_ix}",
            quote_qualified(schema, table),
            ALIAS,
            quote_ident(column_name),
            c.type_name,
            kp.sql,
            ALIAS
        );
        (sql, kp, None, is_json_type)
    } else {
        jsonb_column(columns, column_name).map_err(SetError::Invalid)?;
        let kp = key_predicate(columns, key, 3).map_err(SetError::Invalid)?;
        let xmin_ix = 3 + kp.params.len();
        let qcol = quote_ident(column_name);
        let sql = format!(
            "UPDATE {} AS {} SET {qcol} = jsonb_set({qcol}, $1::text[], CAST($2::text AS jsonb), true) \
             WHERE {} AND {}.xmin::text = ${xmin_ix} AND {qcol} IS NOT NULL",
            quote_qualified(schema, table),
            ALIAS,
            kp.sql,
            ALIAS
        );
        (sql, kp, Some(path_params(path)), true)
    };
    if value_needs_validation {
        invalid_json(new_json_text)?;
    }

    let mut params: Vec<&(dyn ToSql + Sync)> = Vec::new();
    if let Some(path_arr) = &path_arr {
        params.push(path_arr);
    }
    params.push(&new_json_text);
    params.extend(kp.params.iter().map(|s| s as &(dyn ToSql + Sync)));
    params.push(&expected_xmin);

    let n = session
        .client()
        .execute(&sql, refs(&params))
        .await
        .map_err(|e| SetError::Failed(UserFacingError::from_pg(&e, Some(&target))))?;
    match n {
        1 => Ok(()),
        _ => Err(SetError::Conflict),
    }
}

/// Deletes the key or array element at `path` (which must be non-empty and on a `jsonb` column).
pub async fn delete_at_path(
    session: &Session,
    schema: &str,
    table: &str,
    columns: &[Column],
    key: &[(String, String)],
    column_name: &str,
    path: &JsonPath,
    expected_xmin: &str,
) -> Result<(), SetError> {
    if path.is_empty() {
        return Err(SetError::Invalid(UserFacingError::config(
            "Nothing to delete",
            "Choose Set NULL to clear the whole value, or delete the row instead.",
        )));
    }
    jsonb_column(columns, column_name).map_err(SetError::Invalid)?;
    let target = session.info().endpoint.clone();
    let kp = key_predicate(columns, key, 2).map_err(SetError::Invalid)?;
    let xmin_ix = 2 + kp.params.len();
    let qcol = quote_ident(column_name);
    let sql = format!(
        "UPDATE {} AS {} SET {qcol} = {qcol} #- $1::text[] WHERE {} AND {}.xmin::text = ${xmin_ix} AND {qcol} IS NOT NULL",
        quote_qualified(schema, table),
        ALIAS,
        kp.sql,
        ALIAS
    );
    let path_arr = path_params(path);
    let mut params: Vec<&(dyn ToSql + Sync)> = vec![&path_arr];
    params.extend(kp.params.iter().map(|s| s as &(dyn ToSql + Sync)));
    params.push(&expected_xmin);
    let n = session
        .client()
        .execute(&sql, refs(&params))
        .await
        .map_err(|e| SetError::Failed(UserFacingError::from_pg(&e, Some(&target))))?;
    match n {
        1 => Ok(()),
        _ => Err(SetError::Conflict),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    fn col(name: &str, ty: &str, pk: bool) -> Column {
        Column { name: name.into(), type_name: ty.into(), is_primary_key: pk, ..Default::default() }
    }

    fn cols() -> Vec<Column> {
        vec![col("id", "bigint", true), col("doc", "jsonb", false), col("note", "text", false)]
    }

    #[test]
    fn path_segments_become_plain_text_elements() {
        let path = [PathSegment::Key("a".into()), PathSegment::Index(3), PathSegment::Key("b c".into())];
        assert_eq!(path_params(&path), vec!["a", "3", "b c"]);
        assert_eq!(path_params(&[]), Vec::<String>::new());
    }

    #[test]
    fn key_predicate_casts_each_column_to_its_type_and_numbers_from_start() {
        let kp = key_predicate(&cols(), &[("id".into(), "7".into())], 5).unwrap();
        assert_eq!(kp.sql, "\"pgb_t\".\"id\" = CAST($5::text AS bigint)");
        assert_eq!(kp.params, ["7"]);
    }

    #[test]
    fn key_predicate_rejects_an_unknown_column() {
        assert!(key_predicate(&cols(), &[("nope".into(), "1".into())], 1).is_err());
    }

    #[test]
    fn jsonb_column_accepts_only_jsonb() {
        assert!(jsonb_column(&cols(), "doc").is_ok());
        let err = jsonb_column(&cols(), "note").unwrap_err();
        assert_eq!(err.title, "Not a jsonb column");
        assert!(err.detail.contains("text"));
    }

    #[test]
    fn node_kind_parses_every_jsonb_typeof_value() {
        for (text, kind) in [
            ("null", NodeKind::Null), ("boolean", NodeKind::Bool), ("number", NodeKind::Number),
            ("string", NodeKind::String), ("object", NodeKind::Object), ("array", NodeKind::Array),
        ] {
            assert_eq!(parse_kind(text).unwrap(), kind, "{text}");
        }
        assert!(parse_kind("bogus").is_err());
        assert!(NodeKind::Object.is_container() && NodeKind::Array.is_container());
        assert!(!NodeKind::String.is_container());
    }

    #[test]
    fn invalid_json_is_rejected_locally_with_no_network_involved() {
        assert!(invalid_json("{not json").is_err());
        assert!(invalid_json("").is_err());
        for ok in ["\"hi\"", "42", "true", "null", "{\"a\":1}", "[1,2,3]"] {
            assert!(invalid_json(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn conflict_converts_to_a_clear_user_facing_error() {
        let e: UserFacingError = SetError::Conflict.into();
        assert_eq!(e.kind, ErrorKind::Conflict);
        assert!(e.hint.unwrap().contains("Nothing was saved"));
    }
}
