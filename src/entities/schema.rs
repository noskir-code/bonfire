//! Automatic database schema: every entity declares the table it needs, and at
//! startup `sync` compares it with the real database and fixes the differences.
//!
//! Done automatically:
//! - missing tables, columns and indexes are created,
//! - `NULL` / `NOT NULL` and default values are aligned,
//! - indexes and unique constraints not declared anymore are dropped
//!   (they hold no data, to change an index give it a new name).
//!
//! Never done automatically, only logged as warnings because data could be lost:
//! - dropping a column that is not declared anymore,
//! - changing the type of a column.
//!
//! Foreign keys (`references`) are set when the column is created or added,
//! changing them later on an existing column is not detected.

use std::collections::{HashMap, HashSet};

use sqlx::{AssertSqlSafe, PgConnection, PgPool, Row};

/// Any number, it only has to be the same for every Bonfire server
/// sharing the database so they don't change the schema at the same time.
const SCHEMA_LOCK_ID: i64 = 0x0B0F_1CE5;

#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub name: &'static str,
    /// Postgres type, `SERIAL` and `BIGSERIAL` are only allowed for the primary key.
    pub sql_type: &'static str,
    pub nullable: bool,
    /// SQL expression, like `''` or `NOW()`.
    pub default: Option<&'static str>,
    pub primary_key: bool,
    /// Foreign key target with its rules, like `users(id) ON DELETE CASCADE`.
    pub references: Option<&'static str>,
}

impl Column {
    /// `NOT NULL` without default. Use the methods below to change it.
    pub const fn new(name: &'static str, sql_type: &'static str) -> Self {
        Self {
            name,
            sql_type,
            nullable: false,
            default: None,
            primary_key: false,
            references: None,
        }
    }

    pub const fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    pub const fn default(mut self, expression: &'static str) -> Self {
        self.default = Some(expression);
        self
    }

    pub const fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self
    }

    pub const fn references(mut self, target: &'static str) -> Self {
        self.references = Some(target);
        self
    }

    fn definition(&self) -> String {
        let mut definition = format!("{} {}", self.name, self.sql_type);
        if self.primary_key {
            definition.push_str(" PRIMARY KEY");
        } else if !self.nullable {
            definition.push_str(" NOT NULL");
        }
        if let Some(default) = self.default {
            definition.push_str(" DEFAULT ");
            definition.push_str(default);
        }
        if let Some(target) = self.references {
            definition.push_str(" REFERENCES ");
            definition.push_str(target);
        }
        definition
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Index {
    pub name: &'static str,
    pub unique: bool,
    /// What is indexed, like `LOWER(username)` or `community_id, created_at`.
    pub on: &'static str,
}

impl Index {
    pub const fn new(name: &'static str, on: &'static str) -> Self {
        Self {
            name,
            unique: false,
            on,
        }
    }

    pub const fn unique(name: &'static str, on: &'static str) -> Self {
        Self {
            name,
            unique: true,
            on,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Table {
    pub name: &'static str,
    pub columns: &'static [Column],
    pub indexes: &'static [Index],
}

/// Column as it currently is in the database.
struct ExistingColumn {
    sql_type: String,
    not_null: bool,
    default: Option<String>,
}

/// Index as it currently is in the database.
struct ExistingIndex {
    name: String,
    primary: bool,
    /// Set when the index belongs to a constraint, which must be dropped instead.
    constraint: Option<String>,
}

/// Brings every table to its declared state. Tables are handled in order,
/// so a table must come after the tables it references.
pub async fn sync(pool: &PgPool, tables: &[&Table]) -> Result<(), sqlx::Error> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SCHEMA_LOCK_ID)
        .execute(&mut *conn)
        .await?;

    let mut result = Ok(());
    for table in tables {
        if let Err(message) = check_identifiers(table) {
            tracing::error!("schema: table {} skipped: {}", table.name, message);
            continue;
        }
        if let Err(err) = sync_table(&mut conn, table).await {
            result = Err(err);
            break;
        }
    }

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SCHEMA_LOCK_ID)
        .execute(&mut *conn)
        .await?;
    result
}

/// Names are put directly in the SQL, so only simple identifiers are accepted.
fn check_identifiers(table: &Table) -> Result<(), String> {
    let names = std::iter::once(table.name)
        .chain(table.columns.iter().map(|column| column.name))
        .chain(table.indexes.iter().map(|index| index.name));
    for name in names {
        let valid = !name.is_empty()
            && !name.starts_with(|first: char| first.is_ascii_digit())
            && name
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_lowercase() || byte.is_ascii_digit());
        if !valid {
            return Err(format!("invalid name '{name}', use only a-z, 0-9 and _"));
        }
    }
    Ok(())
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Runs one change and logs it. A failed change is logged and the sync goes on.
async fn apply(conn: &mut PgConnection, sql: String, description: &str) -> bool {
    match sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await {
        Ok(_) => {
            tracing::info!("schema: {}", description);
            true
        }
        Err(err) => {
            tracing::error!("schema: failed to {}: {}", description, err);
            false
        }
    }
}

async fn sync_table(conn: &mut PgConnection, table: &Table) -> Result<(), sqlx::Error> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(table.name)
        .fetch_one(&mut *conn)
        .await?;

    if !exists {
        let columns = table
            .columns
            .iter()
            .map(Column::definition)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("CREATE TABLE {} ({columns})", table.name);
        apply(conn, sql, &format!("create table {}", table.name)).await;
    } else {
        sync_columns(conn, table).await?;
    }
    sync_indexes(conn, table).await
}

async fn existing_columns(
    conn: &mut PgConnection,
    table: &str,
) -> Result<HashMap<String, ExistingColumn>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT a.attname AS name,
                format_type(a.atttypid, a.atttypmod) AS sql_type,
                a.attnotnull AS not_null,
                pg_get_expr(d.adbin, d.adrelid) AS default_expression
         FROM pg_attribute a
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;

    rows.iter()
        .map(|row| {
            Ok((
                row.try_get("name")?,
                ExistingColumn {
                    sql_type: row.try_get("sql_type")?,
                    not_null: row.try_get("not_null")?,
                    default: row.try_get("default_expression")?,
                },
            ))
        })
        .collect()
}

/// The name Postgres uses for a type, `TIMESTAMPTZ` is `timestamp with time zone` for example.
async fn canonical_type(conn: &mut PgConnection, column: &Column) -> Result<String, sqlx::Error> {
    if column.sql_type.eq_ignore_ascii_case("SERIAL") {
        return Ok("integer".to_string());
    }
    if column.sql_type.eq_ignore_ascii_case("BIGSERIAL") {
        return Ok("bigint".to_string());
    }
    sqlx::query_scalar("SELECT format_type($1::text::regtype, NULL)")
        .bind(column.sql_type)
        .fetch_one(&mut *conn)
        .await
}

/// Postgres stores defaults rewritten, `''` becomes `''::text` and `NOW()` becomes `now()`.
/// Lowercases and removes the `::type` casts to compare them.
fn normalize_default(expression: &str) -> String {
    let lower = expression.trim().to_lowercase();
    let mut normalized = String::with_capacity(lower.len());
    let mut rest = lower.as_str();
    while let Some(position) = rest.find("::") {
        normalized.push_str(&rest[..position]);
        rest = rest[position + 2..]
            .trim_start_matches(|letter: char| letter.is_ascii_alphanumeric() || letter == '_' || letter == ' ');
    }
    normalized.push_str(rest);
    normalized.trim().to_string()
}

/// Postgres version of the declared default, read back from a temporary table.
/// `None` when it can't be computed.
async fn canonical_default(conn: &mut PgConnection, column: &Column, default: &str) -> Option<String> {
    let create = format!(
        "CREATE TEMP TABLE bonfire_default_probe (value {} DEFAULT {default})",
        column.sql_type
    );
    if let Err(err) = sqlx::query(AssertSqlSafe(create)).execute(&mut *conn).await {
        tracing::warn!("schema: invalid default for {}: {}", column.name, err);
        return None;
    }
    let canonical = sqlx::query_scalar(
        "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef
         WHERE adrelid = 'bonfire_default_probe'::regclass",
    )
    .fetch_one(&mut *conn)
    .await
    .ok();
    let _ = sqlx::query("DROP TABLE bonfire_default_probe")
        .execute(&mut *conn)
        .await;
    canonical
}

/// Quick comparison first, then the exact one done by Postgres for
/// expressions it rewrites in a way `normalize_default` can't match.
async fn same_default(conn: &mut PgConnection, column: &Column, current: Option<&str>) -> bool {
    match (current, column.default) {
        (None, None) => true,
        (Some(current), Some(expected)) => {
            normalize_default(current) == normalize_default(expected)
                || canonical_default(conn, column, expected).await.as_deref() == Some(current)
        }
        _ => false,
    }
}

async fn sync_columns(conn: &mut PgConnection, table: &Table) -> Result<(), sqlx::Error> {
    let existing = existing_columns(conn, table.name).await?;

    for column in table.columns {
        let target = format!("{}.{}", table.name, column.name);

        let Some(current) = existing.get(column.name) else {
            // A NOT NULL column without default can't be added to a table with rows:
            // add it nullable, then try to make it NOT NULL.
            let needs_values = !column.nullable && column.default.is_none() && !column.primary_key;
            let definition = if needs_values {
                Column { nullable: true, ..*column }.definition()
            } else {
                column.definition()
            };
            let sql = format!("ALTER TABLE {} ADD COLUMN {definition}", table.name);
            if apply(conn, sql, &format!("add column {target}")).await && needs_values {
                let sql = format!("ALTER TABLE {} ALTER COLUMN {} SET NOT NULL", table.name, column.name);
                if !apply(conn, sql, &format!("make {target} NOT NULL")).await {
                    tracing::warn!("schema: {target} stays nullable until every row has a value");
                }
            }
            continue;
        };

        let expected_type = canonical_type(conn, column).await?;
        if current.sql_type != expected_type {
            tracing::warn!(
                "schema: {target} is {} but the code declares {}, change it manually",
                current.sql_type,
                expected_type
            );
        }

        // The primary key's NOT NULL and default (its sequence) are managed by Postgres.
        if column.primary_key {
            continue;
        }

        if column.nullable && current.not_null {
            let sql = format!("ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL", table.name, column.name);
            apply(conn, sql, &format!("make {target} nullable")).await;
        } else if !column.nullable && !current.not_null {
            let sql = format!("ALTER TABLE {} ALTER COLUMN {} SET NOT NULL", table.name, column.name);
            if !apply(conn, sql, &format!("make {target} NOT NULL")).await {
                tracing::warn!("schema: {target} stays nullable until every row has a value");
            }
        }

        if !same_default(conn, column, current.default.as_deref()).await {
            let sql = match column.default {
                Some(default) => format!(
                    "ALTER TABLE {} ALTER COLUMN {} SET DEFAULT {default}",
                    table.name, column.name
                ),
                None => format!("ALTER TABLE {} ALTER COLUMN {} DROP DEFAULT", table.name, column.name),
            };
            apply(conn, sql, &format!("update default of {target}")).await;
        }
    }

    let declared: HashSet<&str> = table.columns.iter().map(|column| column.name).collect();
    let mut extra: Vec<&String> = existing
        .keys()
        .filter(|name| !declared.contains(name.as_str()))
        .collect();
    extra.sort();
    for name in extra {
        tracing::warn!(
            "schema: {}.{} is not used by the code anymore, drop it manually if its data is not needed",
            table.name,
            name
        );
    }
    Ok(())
}

async fn existing_indexes(conn: &mut PgConnection, table: &str) -> Result<Vec<ExistingIndex>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT i.relname AS name, x.indisprimary AS is_primary, c.conname AS constraint_name
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         LEFT JOIN pg_constraint c ON c.conindid = x.indexrelid AND c.conrelid = x.indrelid
         WHERE x.indrelid = to_regclass($1)",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(ExistingIndex {
                name: row.try_get("name")?,
                primary: row.try_get("is_primary")?,
                constraint: row.try_get("constraint_name")?,
            })
        })
        .collect()
}

async fn sync_indexes(conn: &mut PgConnection, table: &Table) -> Result<(), sqlx::Error> {
    let existing = existing_indexes(conn, table.name).await?;
    let existing_names: HashSet<&str> = existing.iter().map(|index| index.name.as_str()).collect();

    // Create the new ones first, so a replaced unique index never leaves a gap.
    for index in table.indexes {
        if existing_names.contains(index.name) {
            continue;
        }
        let unique = if index.unique { "UNIQUE " } else { "" };
        let sql = format!("CREATE {unique}INDEX {} ON {} ({})", index.name, table.name, index.on);
        if !apply(conn, sql, &format!("create index {}", index.name)).await && index.unique {
            tracing::warn!("schema: {} can't be created while rows have duplicate values", index.name);
        }
    }

    let declared: HashSet<&str> = table.indexes.iter().map(|index| index.name).collect();
    for index in existing.iter().filter(|index| !index.primary && !declared.contains(index.name.as_str())) {
        // These names come from the database, not the code: quote them.
        let sql = match &index.constraint {
            Some(constraint) => format!("ALTER TABLE {} DROP CONSTRAINT {}", table.name, quote(constraint)),
            None => format!("DROP INDEX {}", quote(&index.name)),
        };
        apply(conn, sql, &format!("drop index {} not declared anymore", index.name)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_compared_without_casts() {
        assert_eq!(normalize_default("''::text"), normalize_default("''"));
        assert_eq!(normalize_default("now()"), normalize_default("NOW()"));
        assert_eq!(
            normalize_default("'x'::timestamp with time zone"),
            normalize_default("'x'")
        );
        assert_ne!(normalize_default("0"), normalize_default("1"));
    }

    #[test]
    fn identifiers_are_checked() {
        const BAD: Table = Table {
            name: "users; DROP TABLE users",
            columns: &[],
            indexes: &[],
        };
        assert!(check_identifiers(&BAD).is_err());
    }
}
