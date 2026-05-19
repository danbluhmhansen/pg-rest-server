use std::collections::HashMap;

use tokio_postgres::Client;

use crate::error::SchemaCacheError;
use crate::introspection_shared::{self, RawForeignKey};
use pg_schema_cache_types::*;

// ---------------------------------------------------------------------------
// Public builder
// ---------------------------------------------------------------------------

pub async fn build_schema_cache(
    client: &Client,
    schemas: &[String],
) -> Result<SchemaCache, SchemaCacheError> {
    let tables = load_tables(client, schemas).await?;
    let columns = load_columns(client, schemas).await?;
    let primary_keys = load_primary_keys(client, schemas).await?;
    let raw_fks = load_foreign_keys(client, schemas).await?;
    let functions = load_functions(client, schemas).await?;
    let enums = load_enums(client).await?;

    Ok(introspection_shared::build_from_data(
        tables,
        columns,
        primary_keys,
        raw_fks,
        functions,
        enums,
    ))
}

// ---------------------------------------------------------------------------
// Query executors
// ---------------------------------------------------------------------------

async fn load_tables(client: &Client, schemas: &[String]) -> Result<Vec<Table>, SchemaCacheError> {
    let rows = client
        .query(introspection_shared::TABLES_QUERY, &[&schemas])
        .await?;
    let mut tables = Vec::with_capacity(rows.len());
    for row in &rows {
        let kind: String = row.get("kind");
        let is_view = kind == "v" || kind == "m";
        let insertable: bool = row.get("insertable");
        let updatable: bool = row.get("updatable");
        tables.push(Table {
            name: QualifiedName::new(
                row.get::<_, String>("table_schema"),
                row.get::<_, String>("table_name"),
            ),
            columns: Vec::new(),
            column_index: HashMap::new(),
            primary_key: Vec::new(),
            is_view,
            insertable,
            updatable,
            deletable: updatable,
            comment: row.get("comment"),
        });
    }
    Ok(tables)
}

async fn load_columns(
    client: &Client,
    schemas: &[String],
) -> Result<HashMap<QualifiedName, Vec<Column>>, SchemaCacheError> {
    let rows = client
        .query(introspection_shared::COLUMNS_QUERY, &[&schemas])
        .await?;
    let mut map: HashMap<QualifiedName, Vec<Column>> = HashMap::new();
    for row in &rows {
        let qn = QualifiedName::new(
            row.get::<_, String>("table_schema"),
            row.get::<_, String>("table_name"),
        );
        map.entry(qn).or_default().push(Column {
            name: row.get("column_name"),
            pg_type: row.get("pg_type"),
            nullable: row.get("nullable"),
            has_default: row.get("has_default"),
            default_expr: row.get("default_expr"),
            max_length: row.get("max_length"),
            is_pk: false,
            is_generated: row.get("is_generated"),
            comment: row.get("comment"),
            enum_values: None,
        });
    }
    Ok(map)
}

async fn load_primary_keys(
    client: &Client,
    schemas: &[String],
) -> Result<HashMap<QualifiedName, Vec<String>>, SchemaCacheError> {
    let rows = client
        .query(introspection_shared::PRIMARY_KEYS_QUERY, &[&schemas])
        .await?;
    let mut map: HashMap<QualifiedName, Vec<String>> = HashMap::new();
    for row in &rows {
        let qn = QualifiedName::new(
            row.get::<_, String>("table_schema"),
            row.get::<_, String>("table_name"),
        );
        map.entry(qn).or_default().push(row.get("column_name"));
    }
    Ok(map)
}

async fn load_foreign_keys(
    client: &Client,
    schemas: &[String],
) -> Result<Vec<RawForeignKey>, SchemaCacheError> {
    let rows = client
        .query(introspection_shared::FOREIGN_KEYS_QUERY, &[&schemas])
        .await?;
    let mut fks = Vec::with_capacity(rows.len());
    for row in &rows {
        fks.push(RawForeignKey {
            from_schema: row.get("from_schema"),
            from_table: row.get("from_table"),
            to_schema: row.get("to_schema"),
            to_table: row.get("to_table"),
            constraint_name: row.get("constraint_name"),
            from_columns: row.get("from_columns"),
            to_columns: row.get("to_columns"),
        });
    }
    Ok(fks)
}

async fn load_functions(
    client: &Client,
    schemas: &[String],
) -> Result<HashMap<QualifiedName, Function>, SchemaCacheError> {
    let rows = client
        .query(introspection_shared::FUNCTIONS_QUERY, &[&schemas])
        .await?;
    let mut map = HashMap::new();

    for row in &rows {
        let schema_name: String = row.get("schema_name");
        let function_name: String = row.get("function_name");
        let prokind: String = row.get("prokind");
        let volatility_char: String = row.get("volatility");
        let returns_set: bool = row.get("returns_set");
        let return_type_name: String = row.get("return_type_name");
        let num_args: i32 = row.get("num_args");
        let num_defaults: i32 = row.get("num_defaults");
        let arg_names: Vec<Option<String>> = row.get("arg_names");
        let arg_modes: Vec<String> = row.get("arg_modes");
        let arg_type_names: Vec<String> = row.get("arg_type_names");

        let volatility = match volatility_char.as_str() {
            "i" => Volatility::Immutable,
            "s" => Volatility::Stable,
            _ => Volatility::Volatile,
        };

        let return_type = if return_type_name == "void" {
            ReturnType::Void
        } else if returns_set {
            ReturnType::SetOf(return_type_name)
        } else {
            ReturnType::Scalar(return_type_name)
        };

        let has_modes = !arg_modes.is_empty();
        let mut params = Vec::new();
        let mut in_count: i32 = 0;

        for (i, type_name) in arg_type_names.iter().enumerate() {
            let mode = if has_modes {
                arg_modes.get(i).map(|s| s.as_str()).unwrap_or("i")
            } else {
                "i"
            };
            if mode == "i" || mode == "b" || mode == "v" {
                in_count += 1;
                params.push(FuncParam {
                    name: arg_names.get(i).and_then(|n| n.clone()).unwrap_or_default(),
                    pg_type: type_name.clone(),
                    has_default: in_count > (num_args - num_defaults),
                });
            }
        }

        let qn = QualifiedName::new(schema_name, function_name);
        map.insert(
            qn.clone(),
            Function {
                name: qn,
                params,
                return_type,
                volatility,
                is_procedure: prokind == "p",
                comment: row.get("comment"),
            },
        );
    }

    Ok(map)
}

async fn load_enums(client: &Client) -> Result<HashMap<String, Vec<String>>, SchemaCacheError> {
    let rows = client.query(introspection_shared::ENUMS_QUERY, &[]).await?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in &rows {
        let type_name: String = row.get("type_name");
        let value: String = row.get("enum_value");
        map.entry(type_name).or_default().push(value);
    }
    Ok(map)
}
