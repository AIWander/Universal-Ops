//! SQLite query tool — direct read-only SQLite access
//! Ported from local crate sqlite.rs

use serde_json::{json, Value};

pub fn get_definitions() -> Vec<Value> {
    vec![json!({
        "name": "sqlite_query",
        "description": "Execute a read-only SQL query against a SQLite database. Returns results as JSON array.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "db_path": { "type": "string", "description": "Path to the .db file" },
                "sql": { "type": "string", "description": "SQL query to execute (SELECT only)" },
                "max_rows": { "type": "integer", "description": "Max rows to return (default 100)", "default": 100 }
            },
            "required": ["db_path", "sql"]
        }
    })]
}

pub fn execute(name: &str, args: &Value) -> Value {
    match name {
        "sqlite_query" => execute_query(args),
        _ => json!({"error": format!("Unknown sqlite tool: {}", name)}),
    }
}

fn execute_query(args: &Value) -> Value {
    let db_path = match args.get("db_path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return json!({"error": "db_path required"}),
    };
    let sql = match args.get("sql").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return json!({"error": "sql required"}),
    };
    let max_rows = args.get("max_rows").and_then(|v| v.as_u64()).unwrap_or(100) as usize;

    let sql_upper = sql.trim().to_uppercase();
    if !sql_upper.starts_with("SELECT")
        && !sql_upper.starts_with("PRAGMA")
        && !sql_upper.starts_with("EXPLAIN")
    {
        return json!({"error": "Only SELECT, PRAGMA, and EXPLAIN queries are allowed (read-only)"});
    }

    let conn = match rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("Cannot open {}: {}", db_path, e)}),
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => return json!({"error": format!("SQL error: {}", e)}),
    };

    let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = Vec::new();

    let result = stmt.query_map([], |row| {
        let mut obj = serde_json::Map::new();
        for (i, name) in column_names.iter().enumerate() {
            let val: Value = match row.get_ref(i) {
                Ok(rusqlite::types::ValueRef::Null) => Value::Null,
                Ok(rusqlite::types::ValueRef::Integer(n)) => json!(n),
                Ok(rusqlite::types::ValueRef::Real(f)) => json!(f),
                Ok(rusqlite::types::ValueRef::Text(s)) => {
                    json!(std::str::from_utf8(s).unwrap_or("<invalid utf8>"))
                }
                Ok(rusqlite::types::ValueRef::Blob(b)) => {
                    json!(format!("<blob {} bytes>", b.len()))
                }
                Err(_) => Value::Null,
            };
            obj.insert(name.clone(), val);
        }
        Ok(Value::Object(obj))
    });

    match result {
        Ok(mapped_rows) => {
            for row in mapped_rows {
                if rows.len() >= max_rows {
                    break;
                }
                if let Ok(val) = row {
                    rows.push(val);
                }
            }
        }
        Err(e) => return json!({"error": format!("Query execution failed: {}", e)}),
    }

    json!({
        "columns": column_names,
        "rows": rows,
        "count": rows.len(),
        "db_path": db_path,
        "truncated": rows.len() >= max_rows
    })
}
