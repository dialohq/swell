//! Query analysis pipeline.
//!
//! `Analyzer` owns one `tokio_postgres::Client` and a cached `TypeCatalog`.
//! Each call to `analyze` runs PARSE + DESCRIBE for the supplied SQL, then
//! enriches the result with `pg_attribute.attnotnull`, `EXPLAIN VERBOSE`
//! join nullability, and JSON shape inference.

pub mod describe;
pub mod catalog;
pub mod nullability;
pub mod param_nullability;
pub mod json_shape;
pub mod overrides;
pub mod ts_types;
pub mod query;

pub use query::{InferredColumn, InferredParam, InferredQuery};
pub use ts_types::TypeCatalog;

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use tokio_postgres::{Client, Config, NoTls};

pub struct Analyzer {
    pub client: Client,
    pub catalog: TypeCatalog,
}

pub struct AnalyzerOptions {
    pub database_url: String,
    pub schemas: Vec<String>,
    pub type_overrides: BTreeMap<String, String>,
}

impl Analyzer {
    /// Connect to Postgres and load the type catalog.
    ///
    /// Pins `plan_cache_mode = force_generic_plan` on the session so the
    /// EXPLAIN plans we'll inspect for nullability match what PARSE/DESCRIBE
    /// produces.
    pub async fn connect(opts: AnalyzerOptions) -> Result<Self> {
        let mut cfg: Config = opts.database_url.parse()
            .with_context(|| format!("invalid DATABASE_URL: {}", opts.database_url))?;
        cfg.options("-c plan_cache_mode=force_generic_plan");

        let (client, connection) = cfg.connect(NoTls).await
            .context("connecting to Postgres")?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("postgres connection error: {e}");
            }
        });

        let mut catalog = catalog::load_type_catalog(&client, &opts.schemas).await
            .context("loading pg_catalog")?;
        catalog.by_name = opts.type_overrides;

        Ok(Self { client, catalog })
    }

    /// Cheap fingerprint over the relevant schemas — invalidates caches.
    pub async fn schema_fingerprint(&self, schemas: &[String]) -> Result<String> {
        catalog::schema_fingerprint(&self.client, schemas).await
    }

    /// Run PARSE + DESCRIBE + nullability + JSON shape inference for the
    /// query, returning a fully-typed `InferredQuery`.
    pub async fn analyze(&self, sql: &str) -> Result<InferredQuery> {
        let described = describe::describe(&self.client, sql).await?;

        let pairs: Vec<(u32, i16)> = described.columns.iter()
            .filter(|c| c.table_oid != 0 && c.attnum > 0)
            .map(|c| (c.table_oid, c.attnum))
            .collect();
        let attnotnull = catalog::fetch_attnotnull(&self.client, &pairs).await?;

        let null_hints = nullability::explain_nullability(
            &self.client, sql, &described.params, described.columns.len(),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::debug!("EXPLAIN failed for `{sql}`: {e}");
            nullability::NullabilityHints::unknown(described.columns.len())
        });

        let json_shapes = json_shape::infer_shapes(
            &self.client, &self.catalog, sql, described.columns.len(),
        ).await;

        let param_nullable = param_nullability::infer(
            &self.client, sql, described.params.len(),
        ).await;

        let params = described.params.iter().enumerate()
            .map(|(i, t)| InferredParam {
                oid: t.oid(),
                ts_type: catalog::render_for_oid(&self.catalog, t.oid(), t),
                nullable: param_nullable.get(i).copied().unwrap_or(true),
            })
            .collect();

        let columns = described.columns.iter().enumerate()
            .map(|(i, c)| {
                let inferred_nullable = decide_nullability(
                    c, &attnotnull, null_hints.by_column.get(i).copied()
                        .unwrap_or(nullability::NullVerdict::Unknown),
                );
                let oid_ts = catalog::render_for_oid(&self.catalog, c.type_.oid(), &c.type_);
                let json_ts = json_shapes.by_target.get(i).cloned().flatten();
                let inferred_ts = json_ts.unwrap_or(oid_ts);

                let ov = overrides::parse(&c.name);
                InferredColumn {
                    name: ov.clean_name,
                    oid: c.type_.oid(),
                    nullable: ov.force_nullable.unwrap_or(inferred_nullable),
                    ts_type: ov.force_ts_type.unwrap_or(inferred_ts),
                }
            })
            .collect();

        Ok(InferredQuery { sql: sql.to_string(), params, columns })
    }
}

/// Combine attnotnull and EXPLAIN evidence into a final nullable verdict.
///
/// | base table col? | attnotnull | EXPLAIN          | nullable |
/// |-----------------|------------|------------------|----------|
/// | yes             | NOT NULL   | Nullable         | yes (outer-join trumps) |
/// | yes             | NOT NULL   | otherwise        | no       |
/// | yes             | nullable   | *                | yes      |
/// | no              | n/a        | NotNullable      | no       |
/// | no              | n/a        | otherwise        | yes      |
fn decide_nullability(
    c: &describe::DescribedColumn,
    attnotnull: &std::collections::HashMap<(u32, i16), bool>,
    explain: nullability::NullVerdict,
) -> bool {
    use nullability::NullVerdict::*;
    if c.table_oid != 0 && c.attnum > 0 {
        let base_not_null = attnotnull.get(&(c.table_oid, c.attnum)).copied().unwrap_or(false);
        match (base_not_null, explain) {
            (true, Nullable) => true,
            (true, _)        => false,
            (false, _)       => true,
        }
    } else {
        !matches!(explain, NotNullable)
    }
}
