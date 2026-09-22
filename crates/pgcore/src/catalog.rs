//! Introspection queries. Privilege-aware: objects the user cannot see into are still listed
//! (so the navigator can show them locked) instead of failing the whole refresh.

use crate::error::UserFacingError;
use crate::session::Session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    pub owner: String,
    pub can_use: bool,
    pub is_system: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelationKind {
    fn from_relkind(c: &str) -> Option<Self> {
        Some(match c {
            "r" => Self::Table,
            "p" => Self::PartitionedTable,
            "v" => Self::View,
            "m" => Self::MaterializedView,
            "f" => Self::ForeignTable,
            _ => return None,
        })
    }

    pub fn is_updatable_by_default(self) -> bool {
        matches!(self, Self::Table | Self::PartitionedTable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    pub can_select: bool,
    /// Estimated row count from the planner statistics (`-1` when never analyzed).
    pub estimated_rows: i64,
    pub is_partition: bool,
}

pub async fn list_schemas(session: &Session) -> Result<Vec<Schema>, UserFacingError> {
    let target = session.info().endpoint.clone();
    let rows = session
        .client()
        .query(
            "SELECT n.nspname::text, pg_get_userbyid(n.nspowner)::text, \
                    has_schema_privilege(n.oid, 'USAGE'), \
                    (n.nspname = 'information_schema' OR n.nspname LIKE 'pg\\_%') \
             FROM pg_namespace n \
             WHERE n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%' \
             ORDER BY 4, 1",
            &[],
        )
        .await
        .map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
    Ok(rows
        .iter()
        .map(|r| Schema {
            name: r.get(0),
            owner: r.get(1),
            can_use: r.get(2),
            is_system: r.get(3),
        })
        .collect())
}

pub async fn list_relations(session: &Session, schema: &str) -> Result<Vec<Relation>, UserFacingError> {
    let target = session.info().endpoint.clone();
    let rows = session
        .client()
        .query(
            "SELECT c.relname::text, c.relkind::text, has_table_privilege(c.oid, 'SELECT'), \
                    c.reltuples::bigint, c.relispartition \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','f') \
             ORDER BY c.relname",
            &[&schema],
        )
        .await
        .map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let relkind: String = r.get(1);
            Some(Relation {
                schema: schema.to_string(),
                name: r.get(0),
                kind: RelationKind::from_relkind(&relkind)?,
                can_select: r.get(2),
                estimated_rows: r.get(3),
                is_partition: r.get(4),
            })
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    /// Columns of the referencing (this) table, in constraint order.
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    /// Referenced columns, positionally matching `columns`.
    pub ref_columns: Vec<String>,
}

/// Foreign keys defined on `schema.table` (the outgoing links of its rows).
pub async fn foreign_keys(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<Vec<ForeignKey>, UserFacingError> {
    let target = session.info().endpoint.clone();
    let qualified = crate::sql::quote_qualified(schema, table);
    let rows = session
        .client()
        .query(
            "SELECT c.conname::text, \
                    ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(attnum, ord) \
                          JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord), \
                    rn.nspname::text, rc.relname::text, \
                    ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY k(attnum, ord) \
                          JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.attnum \
                          ORDER BY k.ord) \
             FROM pg_constraint c \
             JOIN pg_class rc ON rc.oid = c.confrelid \
             JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
             WHERE c.contype = 'f' AND c.conrelid = to_regclass($1) \
             ORDER BY c.conname",
            &[&qualified],
        )
        .await
        .map_err(|e| UserFacingError::from_pg(&e, Some(&target)))?;
    Ok(rows
        .iter()
        .map(|r| ForeignKey {
            name: r.get(0),
            columns: r.get(1),
            ref_schema: r.get(2),
            ref_table: r.get(3),
            ref_columns: r.get(4),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relkind_mapping() {
        assert_eq!(RelationKind::from_relkind("r"), Some(RelationKind::Table));
        assert_eq!(RelationKind::from_relkind("m"), Some(RelationKind::MaterializedView));
        assert_eq!(RelationKind::from_relkind("i"), None); // index
        assert!(RelationKind::Table.is_updatable_by_default());
        assert!(!RelationKind::View.is_updatable_by_default());
    }
}
