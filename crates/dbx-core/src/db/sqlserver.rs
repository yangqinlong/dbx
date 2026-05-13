use rust_decimal::Decimal;
use std::time::Instant;
use tiberius::{AuthMethod, Client, ColumnData, Config, FromSql};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{connection_timeout, CONNECTION_TIMEOUT_SECS};
use crate::query::MAX_ROWS;
use crate::sql::starts_with_executable_sql_keyword;
use crate::types::{ColumnInfo, DatabaseInfo, ForeignKeyInfo, IndexInfo, QueryResult, TableInfo, TriggerInfo};

pub type SqlServerClient = Client<Compat<TcpStream>>;
const SIMPLE_QUERY_MODULE_KEYWORDS: &[&str] = &["FUNCTION", "PROC", "PROCEDURE", "TRIGGER", "VIEW"];

pub async fn connect(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    database: Option<&str>,
) -> Result<SqlServerClient, String> {
    match try_connect(host, port, user, pass, database, true).await {
        Ok(client) => Ok(client),
        Err(_) => try_connect(host, port, user, pass, database, false).await,
    }
}

async fn try_connect(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    database: Option<&str>,
    use_encryption: bool,
) -> Result<SqlServerClient, String> {
    let mut config = Config::new();
    config.host(host);
    config.port(port);
    config.authentication(AuthMethod::sql_server(user, pass));
    if let Some(db) = database {
        config.database(db);
    }
    config.trust_cert();
    if !use_encryption {
        config.encryption(tiberius::EncryptionLevel::NotSupported);
    }

    let tcp = tokio::time::timeout(connection_timeout(), TcpStream::connect(config.get_addr()))
        .await
        .map_err(|_| format!("SQL Server connection timed out ({CONNECTION_TIMEOUT_SECS}s)"))?
        .map_err(|e| format!("SQL Server connection failed: {e}"))?;
    tokio::time::timeout(connection_timeout(), Client::connect(config, tcp.compat_write()))
        .await
        .map_err(|_| format!("SQL Server handshake timed out ({CONNECTION_TIMEOUT_SECS}s)"))?
        .map_err(|e| format!("SQL Server connection failed: {e}"))
}

fn row_to_json(row: &tiberius::Row) -> Vec<serde_json::Value> {
    row.cells().map(|(_, cell)| sqlserver_cell_to_json(cell)).collect()
}

fn sqlserver_cell_to_json(cell: &ColumnData<'static>) -> serde_json::Value {
    if let Ok(Some(v)) = <&str as FromSql>::from_sql(cell) {
        return serde_json::Value::String(v.to_string());
    }
    if let Ok(Some(v)) = <chrono::NaiveDateTime as FromSql>::from_sql(cell) {
        return serde_json::Value::String(v.to_string());
    }
    if let Ok(Some(v)) = <chrono::NaiveDate as FromSql>::from_sql(cell) {
        return serde_json::Value::String(v.to_string());
    }
    if let Ok(Some(v)) = <chrono::NaiveTime as FromSql>::from_sql(cell) {
        return serde_json::Value::String(v.to_string());
    }
    if let Ok(Some(v)) = <chrono::DateTime<chrono::FixedOffset> as FromSql>::from_sql(cell) {
        return serde_json::Value::String(v.to_rfc3339());
    }
    if let Ok(Some(v)) = <Decimal as FromSql>::from_sql(cell) {
        return serde_json::Value::String(v.to_string());
    }
    if let Ok(Some(v)) = <u8 as FromSql>::from_sql(cell) {
        return serde_json::Value::Number(v.into());
    }
    if let Ok(Some(v)) = <i16 as FromSql>::from_sql(cell) {
        return serde_json::Value::Number(v.into());
    }
    if let Ok(Some(v)) = <i32 as FromSql>::from_sql(cell) {
        return serde_json::Value::Number(v.into());
    }
    if let Ok(Some(v)) = <i64 as FromSql>::from_sql(cell) {
        return super::safe_i64_to_json(v);
    }
    if let Ok(Some(v)) = <f32 as FromSql>::from_sql(cell) {
        return serde_json::Number::from_f64(v as f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }
    if let Ok(Some(v)) = <f64 as FromSql>::from_sql(cell) {
        return serde_json::Number::from_f64(v).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null);
    }
    if let Ok(Some(v)) = <bool as FromSql>::from_sql(cell) {
        return serde_json::Value::Bool(v);
    }
    if let Ok(Some(v)) = <uuid::Uuid as FromSql>::from_sql(cell) {
        return serde_json::Value::String(v.to_string());
    }
    if let Ok(Some(v)) = <Vec<u8> as tiberius::FromSqlOwned>::from_sql_owned(cell.clone()) {
        return serde_json::Value::String(format!("0x{}", hex_encode(&v)));
    }
    serde_json::Value::Null
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub async fn list_databases(client: &mut SqlServerClient) -> Result<Vec<DatabaseInfo>, String> {
    let stream = client
        .query(
            "SELECT name \
             FROM sys.databases \
             WHERE database_id > 4 \
               AND state = 0 \
             ORDER BY name",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|row| DatabaseInfo { name: row.get::<&str, _>(0).unwrap_or("").to_string() }).collect())
}

pub async fn list_schemas(client: &mut SqlServerClient) -> Result<Vec<String>, String> {
    let stream = client
        .query(
            "SELECT s.name \
         FROM sys.schemas s \
         WHERE s.name NOT IN ('guest','INFORMATION_SCHEMA','sys') \
           AND EXISTS ( \
             SELECT 1 FROM sys.objects o \
             WHERE o.schema_id = s.schema_id \
               AND o.type IN ('U','V') \
               AND o.is_ms_shipped = 0 \
           ) \
         ORDER BY CASE WHEN s.name = 'dbo' THEN 0 ELSE 1 END, s.name",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|row| row.get::<&str, _>(0).unwrap_or("").to_string()).collect())
}

pub async fn list_tables(
    client: &mut SqlServerClient,
    schema: &str,
    filter: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<TableInfo>, String> {
    let top = limit.map(|value| format!("TOP ({}) ", value.min(1000))).unwrap_or_default();
    let filter_clause = filter
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(" AND o.name LIKE '%{}%' ESCAPE '\\' ", escape_like_literal(value.trim())))
        .unwrap_or_default();
    let sql = format!(
        "SELECT {top}o.name, CASE WHEN o.type = 'V' THEN 'VIEW' ELSE 'BASE TABLE' END \
         FROM sys.objects o \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE s.name = '{}' \
           AND o.type IN ('U','V') \
           AND o.is_ms_shipped = 0 \
           {filter_clause}\
         ORDER BY o.name",
        schema.replace('\'', "''")
    );
    let stream = client.query(&*sql, &[]).await.map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| TableInfo {
            name: row.get::<&str, _>(0).unwrap_or("").to_string(),
            table_type: row.get::<&str, _>(1).unwrap_or("BASE TABLE").to_string(),
            comment: None,
        })
        .collect())
}

fn escape_like_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "''").replace('%', "\\%").replace('_', "\\_").replace('[', "\\[")
}

pub async fn list_objects(client: &mut SqlServerClient, schema: &str) -> Result<Vec<crate::types::ObjectInfo>, String> {
    let sql = format!(
        "SELECT o.name, \
         CASE o.type \
           WHEN 'U' THEN 'TABLE' \
           WHEN 'V' THEN 'VIEW' \
           WHEN 'P' THEN 'PROCEDURE' \
           WHEN 'FN' THEN 'FUNCTION' \
           WHEN 'IF' THEN 'FUNCTION' \
           WHEN 'TF' THEN 'FUNCTION' \
           WHEN 'FS' THEN 'FUNCTION' \
           WHEN 'FT' THEN 'FUNCTION' \
           ELSE o.type_desc \
         END AS object_type \
         FROM sys.objects o \
         JOIN sys.schemas s ON s.schema_id = o.schema_id \
         WHERE s.name = '{}' \
           AND o.type IN ('U','V','P','FN','IF','TF','FS','FT') \
           AND o.is_ms_shipped = 0 \
         ORDER BY CASE o.type \
           WHEN 'U' THEN 0 \
           WHEN 'V' THEN 1 \
           WHEN 'P' THEN 2 \
           ELSE 3 \
         END, o.name",
        schema.replace('\'', "''")
    );
    let stream = client.query(&*sql, &[]).await.map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| crate::types::ObjectInfo {
            name: row.get::<&str, _>(0).unwrap_or("").to_string(),
            object_type: row.get::<&str, _>(1).unwrap_or("TABLE").to_string(),
            schema: Some(schema.to_string()),
            comment: None,
        })
        .collect())
}

pub async fn get_columns(client: &mut SqlServerClient, schema: &str, table: &str) -> Result<Vec<ColumnInfo>, String> {
    let sql = format!(
        "SELECT c.COLUMN_NAME, c.DATA_TYPE, c.IS_NULLABLE, c.COLUMN_DEFAULT, \
         CASE WHEN kcu.COLUMN_NAME IS NOT NULL THEN 1 ELSE 0 END AS IS_PK, \
         c.NUMERIC_PRECISION, c.NUMERIC_SCALE, c.CHARACTER_MAXIMUM_LENGTH, c.DATETIME_PRECISION \
         FROM INFORMATION_SCHEMA.COLUMNS c \
         LEFT JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
           ON c.TABLE_SCHEMA = kcu.TABLE_SCHEMA AND c.TABLE_NAME = kcu.TABLE_NAME AND c.COLUMN_NAME = kcu.COLUMN_NAME \
           AND kcu.CONSTRAINT_NAME IN (SELECT CONSTRAINT_NAME FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS WHERE CONSTRAINT_TYPE = 'PRIMARY KEY' AND TABLE_SCHEMA = '{s}' AND TABLE_NAME = '{t}') \
         WHERE c.TABLE_SCHEMA = '{s}' AND c.TABLE_NAME = '{t}' \
         ORDER BY c.ORDINAL_POSITION",
        s = schema.replace('\'', "''"), t = table.replace('\'', "''")
    );
    let stream = client.query(&*sql, &[]).await.map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| {
            let base = row.get::<&str, _>(1).unwrap_or("").to_string();
            let max_len = row
                .try_get::<i32, _>(7)
                .ok()
                .flatten()
                .or_else(|| row.try_get::<i16, _>(7).ok().flatten().map(|v| v as i32));
            let dt_prec = row
                .try_get::<i32, _>(8)
                .ok()
                .flatten()
                .or_else(|| row.try_get::<i16, _>(8).ok().flatten().map(|v| v as i32));
            let num_prec = row
                .try_get::<i32, _>(5)
                .ok()
                .flatten()
                .or_else(|| row.try_get::<i16, _>(5).ok().flatten().map(|v| v as i32));
            let num_scale = row
                .try_get::<i32, _>(6)
                .ok()
                .flatten()
                .or_else(|| row.try_get::<i16, _>(6).ok().flatten().map(|v| v as i32));
            let data_type = match base.to_lowercase().as_str() {
                "varchar" => match max_len {
                    Some(-1) => "varchar(max)".to_string(),
                    Some(n) => format!("varchar({n})"),
                    None => "varchar".to_string(),
                },
                "nvarchar" => match max_len {
                    Some(-1) => "nvarchar(max)".to_string(),
                    Some(n) => format!("nvarchar({n})"),
                    None => "nvarchar".to_string(),
                },
                "varbinary" => match max_len {
                    Some(-1) => "varbinary(max)".to_string(),
                    Some(n) if n > 0 => format!("varbinary({n})"),
                    _ => "varbinary".to_string(),
                },
                "char" | "nchar" | "binary" => match max_len {
                    Some(n) if n > 0 => format!("{base}({n})"),
                    _ => base,
                },
                "decimal" | "numeric" => match (num_prec, num_scale) {
                    (Some(p), Some(s)) => format!("{base}({p},{s})"),
                    _ => base,
                },
                "datetime2" | "datetimeoffset" | "time" => match dt_prec {
                    Some(p) => format!("{base}({p})"),
                    _ => base,
                },
                _ => base,
            };
            ColumnInfo {
                name: row.get::<&str, _>(0).unwrap_or("").to_string(),
                data_type,
                is_nullable: row.get::<&str, _>(2).unwrap_or("NO") == "YES",
                column_default: row.get::<&str, _>(3).map(|s| s.to_string()),
                is_primary_key: row.get::<i32, _>(4).unwrap_or(0) == 1,
                extra: None,
                comment: None,
                numeric_precision: num_prec,
                numeric_scale: num_scale,
                character_maximum_length: max_len,
            }
        })
        .collect())
}

pub async fn list_indexes(client: &mut SqlServerClient, schema: &str, table: &str) -> Result<Vec<IndexInfo>, String> {
    let sql = format!(
        "SELECT i.name, \
         STRING_AGG(CASE WHEN ic.is_included_column = 0 THEN c.name END, ',') WITHIN GROUP (ORDER BY ic.key_ordinal) AS columns, \
         i.is_unique, i.is_primary_key, i.type_desc, \
         STRING_AGG(CASE WHEN ic.is_included_column = 1 THEN c.name END, ',') AS included_cols, \
         i.filter_definition \
         FROM sys.indexes i \
         JOIN sys.index_columns ic ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
         JOIN sys.columns c ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
         WHERE i.object_id = OBJECT_ID('{s}.{t}') AND i.name IS NOT NULL \
         GROUP BY i.name, i.is_unique, i.is_primary_key, i.type_desc, i.filter_definition \
         ORDER BY i.name",
        s = schema.replace('\'', "''"), t = table.replace('\'', "''")
    );
    let stream = client.query(&*sql, &[]).await.map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| {
            let cols_str = row.get::<&str, _>(1).unwrap_or("");
            let inc_str = row.get::<&str, _>(5).unwrap_or("");
            IndexInfo {
                name: row.get::<&str, _>(0).unwrap_or("").to_string(),
                columns: cols_str.split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect(),
                is_unique: row.get::<bool, _>(2).unwrap_or(false),
                is_primary: row.get::<bool, _>(3).unwrap_or(false),
                filter: row.get::<&str, _>(6).map(|s| s.to_string()),
                index_type: row.get::<&str, _>(4).map(|s| s.to_string()),
                included_columns: if inc_str.is_empty() {
                    None
                } else {
                    Some(inc_str.split(',').map(|s| s.to_string()).collect())
                },
                comment: None,
            }
        })
        .collect())
}

pub async fn list_foreign_keys(
    client: &mut SqlServerClient,
    schema: &str,
    table: &str,
) -> Result<Vec<ForeignKeyInfo>, String> {
    let sql = format!(
        "SELECT fk.name, c.name, rt.name, rc.name \
         FROM sys.foreign_keys fk \
         JOIN sys.foreign_key_columns fkc ON fk.object_id = fkc.constraint_object_id \
         JOIN sys.columns c ON fkc.parent_object_id = c.object_id AND fkc.parent_column_id = c.column_id \
         JOIN sys.tables rt ON fkc.referenced_object_id = rt.object_id \
         JOIN sys.columns rc ON fkc.referenced_object_id = rc.object_id AND fkc.referenced_column_id = rc.column_id \
         WHERE fk.parent_object_id = OBJECT_ID('{s}.{t}') \
         ORDER BY fk.name",
        s = schema.replace('\'', "''"),
        t = table.replace('\'', "''")
    );
    let stream = client.query(&*sql, &[]).await.map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| ForeignKeyInfo {
            name: row.get::<&str, _>(0).unwrap_or("").to_string(),
            column: row.get::<&str, _>(1).unwrap_or("").to_string(),
            ref_table: row.get::<&str, _>(2).unwrap_or("").to_string(),
            ref_column: row.get::<&str, _>(3).unwrap_or("").to_string(),
        })
        .collect())
}

pub async fn list_triggers(
    client: &mut SqlServerClient,
    schema: &str,
    table: &str,
) -> Result<Vec<TriggerInfo>, String> {
    let sql = format!(
        "SELECT t.name, te.type_desc, CASE WHEN t.is_instead_of_trigger = 1 THEN 'INSTEAD OF' ELSE 'AFTER' END \
         FROM sys.triggers t \
         JOIN sys.trigger_events te ON t.object_id = te.object_id \
         WHERE t.parent_id = OBJECT_ID('{s}.{t}') \
         ORDER BY t.name",
        s = schema.replace('\'', "''"),
        t = table.replace('\'', "''")
    );
    let stream = client.query(&*sql, &[]).await.map_err(|e| e.to_string())?;
    let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| TriggerInfo {
            name: row.get::<&str, _>(0).unwrap_or("").to_string(),
            event: row.get::<&str, _>(1).unwrap_or("").to_string(),
            timing: row.get::<&str, _>(2).unwrap_or("AFTER").to_string(),
        })
        .collect())
}

pub async fn execute_query(client: &mut SqlServerClient, sql: &str) -> Result<QueryResult, String> {
    let start = Instant::now();

    if starts_with_executable_sql_keyword(sql, &["SELECT", "EXEC", "WITH", "TABLE"]) {
        let mut stream = client.query(sql, &[]).await.map_err(|e| e.to_string())?;
        let columns_meta = stream
            .columns()
            .await
            .map_err(|e| e.to_string())?
            .map(|cols| cols.iter().map(|c| c.name().to_string()).collect::<Vec<_>>())
            .unwrap_or_default();

        let rows = stream.into_first_result().await.map_err(|e| e.to_string())?;
        let result_rows: Vec<Vec<serde_json::Value>> = rows.iter().map(|row| row_to_json(row)).collect();

        Ok(QueryResult {
            columns: columns_meta,
            rows: result_rows,
            affected_rows: 0,
            execution_time_ms: start.elapsed().as_millis(),
            truncated: false,
        })
    } else if requires_simple_query_batch(sql) {
        client.simple_query(sql).await.map_err(|e| e.to_string())?.into_results().await.map_err(|e| e.to_string())?;
        Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            affected_rows: 0,
            execution_time_ms: start.elapsed().as_millis(),
            truncated: false,
        })
    } else {
        let result = client.execute(sql, &[]).await.map_err(|e| e.to_string())?;
        Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            affected_rows: result.rows_affected().iter().sum::<u64>(),
            execution_time_ms: start.elapsed().as_millis(),
            truncated: false,
        })
    }
}

pub async fn execute_batch(client: &mut SqlServerClient, sql: &str) -> Result<Vec<QueryResult>, String> {
    let start = Instant::now();
    let stream = client.simple_query(sql).await.map_err(|e| e.to_string())?;
    let result_sets = stream.into_results().await.map_err(|e| e.to_string())?;

    let mut results = Vec::new();
    for rows in &result_sets {
        if rows.is_empty() {
            continue;
        }
        let columns: Vec<String> = rows[0].columns().iter().map(|c| c.name().to_string()).collect();
        let truncated = rows.len() > MAX_ROWS;
        let result_rows: Vec<Vec<serde_json::Value>> = rows.iter().take(MAX_ROWS).map(|row| row_to_json(row)).collect();
        results.push(QueryResult {
            columns,
            rows: result_rows,
            affected_rows: 0,
            execution_time_ms: start.elapsed().as_millis(),
            truncated,
        });
    }

    if results.is_empty() {
        results.push(QueryResult {
            columns: vec![],
            rows: vec![],
            affected_rows: result_sets.len() as u64,
            execution_time_ms: start.elapsed().as_millis(),
            truncated: false,
        });
    }

    Ok(results)
}

fn requires_simple_query_batch(sql: &str) -> bool {
    let tokens = first_sql_tokens(sql, 4);
    if tokens.len() >= 4
        && tokens[0].eq_ignore_ascii_case("CREATE")
        && tokens[1].eq_ignore_ascii_case("OR")
        && tokens[2].eq_ignore_ascii_case("ALTER")
    {
        return SIMPLE_QUERY_MODULE_KEYWORDS.iter().any(|keyword| tokens[3].eq_ignore_ascii_case(keyword));
    }

    if tokens.len() >= 2 && (tokens[0].eq_ignore_ascii_case("CREATE") || tokens[0].eq_ignore_ascii_case("ALTER")) {
        return SIMPLE_QUERY_MODULE_KEYWORDS.iter().any(|keyword| tokens[1].eq_ignore_ascii_case(keyword));
    }

    false
}

fn first_sql_tokens(sql: &str, limit: usize) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() && tokens.len() < limit {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }

        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }

        if i > start {
            tokens.push(sql[start..i].to_string());
        } else {
            i += 1;
        }
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::{requires_simple_query_batch, sqlserver_cell_to_json};
    use chrono::NaiveDate;
    use tiberius::{ColumnData, IntoSql};

    #[test]
    fn sqlserver_module_definitions_require_simple_query_batch() {
        assert!(requires_simple_query_batch("CREATE FUNCTION dbo.fn_demo() RETURNS INT AS BEGIN RETURN 1; END;"));
        assert!(requires_simple_query_batch("ALTER PROCEDURE dbo.usp_demo AS SELECT 1;"));
        assert!(requires_simple_query_batch("CREATE OR ALTER VIEW dbo.vw_demo AS SELECT 1 AS id;"));
        assert!(requires_simple_query_batch(
            "-- comment\nALTER TRIGGER dbo.tr_demo ON dbo.t AFTER INSERT AS SELECT 1;"
        ));
    }

    #[test]
    fn sqlserver_regular_ddl_can_use_execute() {
        assert!(!requires_simple_query_batch("ALTER TABLE dbo.t ADD name NVARCHAR(20);"));
        assert!(!requires_simple_query_batch("CREATE TABLE dbo.t(id INT);"));
        assert!(!requires_simple_query_batch("UPDATE dbo.t SET id = 1;"));
    }

    #[test]
    fn sqlserver_tinyint_cells_are_json_numbers() {
        assert_eq!(sqlserver_cell_to_json(&ColumnData::U8(Some(7))), serde_json::json!(7));
    }

    #[test]
    fn sqlserver_datetime2_cells_are_json_strings() {
        let datetime = NaiveDate::from_ymd_opt(2026, 5, 13).unwrap().and_hms_milli_opt(9, 8, 7, 123).unwrap();
        let cell: ColumnData<'static> = datetime.into_sql();

        assert_eq!(sqlserver_cell_to_json(&cell), serde_json::json!("2026-05-13 09:08:07.123"));
    }
}
