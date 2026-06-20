//! Drives the Postgres extended-query protocol: PARSE + DESCRIBE on each
//! query, returning param OIDs and result `RowDescription` columns.

use postgres_types::Type;
use tokio_postgres::Client;

#[derive(Debug, Clone)]
pub struct DescribedQuery {
    pub params: Vec<Type>,
    pub columns: Vec<DescribedColumn>,
}

#[derive(Debug, Clone)]
pub struct DescribedColumn {
    pub name: String,
    pub type_: Type,
    /// `pg_class.oid` of the underlying base table, or 0 if not a direct
    /// column reference.
    pub table_oid: u32,
    /// `pg_attribute.attnum`, or 0 if not a direct column reference.
    pub attnum: i16,
}

pub async fn describe(client: &Client, sql: &str) -> anyhow::Result<DescribedQuery> {
    let stmt = client.prepare(sql).await.map_err(|e| {
        anyhow::anyhow!(
            "PARSE/DESCRIBE failed for query:\n  {}\n  → {}",
            sql.trim(),
            format_pg_error(&e)
        )
    })?;

    Ok(DescribedQuery {
        params: stmt.params().to_vec(),
        columns: stmt
            .columns()
            .iter()
            .map(|c| DescribedColumn {
                name: c.name().to_string(),
                type_: c.type_().clone(),
                table_oid: c.table_oid().unwrap_or(0),
                attnum: c.column_id().unwrap_or(0),
            })
            .collect(),
    })
}

/// Unwrap a `tokio_postgres::Error` to surface the actual server message.
/// The default Display impl is unhelpfully terse ("db error").
pub(crate) fn format_pg_error(e: &tokio_postgres::Error) -> String {
    let Some(db) = e.as_db_error() else {
        return e.to_string();
    };
    let mut out = db.message().to_string();
    if let Some(d) = db.detail() {
        out.push_str("\n  detail: ");
        out.push_str(d);
    }
    if let Some(h) = db.hint() {
        out.push_str("\n  hint: ");
        out.push_str(h);
    }
    out.push_str(&format!(" (SQLSTATE {})", db.code().code()));
    out
}
