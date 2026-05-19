use pg_schema_cache::{Column, Function, ReturnType, SchemaCache, Table, Volatility};
use serde_json::{json, Map, Value};

use crate::config::AppConfig;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate an OpenAPI 2.0 (Swagger) spec from the schema cache.
pub fn generate_v2(cache: &SchemaCache, config: &AppConfig) -> Value {
    let schemas = &config.database.schemas;
    let tables = tables_in_schemas(cache, schemas);
    let functions = functions_in_schemas(cache, schemas);

    let mut definitions = Map::new();
    let mut paths = Map::new();

    for table in &tables {
        let name = &table.name.name;
        definitions.insert(name.clone(), build_schema_object(table));
        paths.insert(format!("/{name}"), build_paths_v2(table, name));
    }

    for func in &functions {
        let name = &func.name.name;
        paths.insert(format!("/rpc/{name}"), build_rpc_path_v2(func));
    }

    json!({
        "swagger": "2.0",
        "info": {
            "title": "PostgREST API",
            "description": "Automatic REST API for PostgreSQL",
            "version": "0.1.0"
        },
        "host": format!("{}:{}", config.server.host, config.server.port),
        "basePath": "/",
        "schemes": ["http"],
        "consumes": ["application/json", "text/csv"],
        "produces": ["application/json", "text/csv"],
        "paths": Value::Object(paths),
        "definitions": Value::Object(definitions)
    })
}

/// Generate an OpenAPI 3.0 spec from the schema cache.
pub fn generate_v3(cache: &SchemaCache, config: &AppConfig) -> Value {
    let schemas = &config.database.schemas;
    let tables = tables_in_schemas(cache, schemas);
    let functions = functions_in_schemas(cache, schemas);

    let mut component_schemas = Map::new();
    let mut paths = Map::new();

    for table in &tables {
        let name = &table.name.name;
        component_schemas.insert(name.clone(), build_schema_object(table));
        paths.insert(format!("/{name}"), build_paths_v3(table, name));
    }

    for func in &functions {
        let name = &func.name.name;
        paths.insert(format!("/rpc/{name}"), build_rpc_path_v3(func));
    }

    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "PostgREST API",
            "description": "Automatic REST API for PostgreSQL",
            "version": "0.1.0"
        },
        "servers": [
            { "url": format!("http://{}:{}", config.server.host, config.server.port) }
        ],
        "paths": Value::Object(paths),
        "components": {
            "schemas": Value::Object(component_schemas)
        }
    })
}

// ---------------------------------------------------------------------------
// Schema object (shared between v2 and v3)
// ---------------------------------------------------------------------------

fn build_schema_object(table: &Table) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();

    for col in &table.columns {
        properties.insert(col.name.clone(), column_to_json_schema(col));
        if !col.nullable && !col.has_default {
            required.push(Value::String(col.name.clone()));
        }
    }

    let mut schema = json!({
        "type": "object",
        "properties": Value::Object(properties),
    });

    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }

    if let Some(comment) = &table.comment {
        schema["description"] = Value::String(comment.clone());
    }

    schema
}

/// Map a PostgreSQL column type to a JSON Schema type + format.
fn column_to_json_schema(col: &Column) -> Value {
    let mut schema = pg_type_to_json_schema(&col.pg_type);

    if col.nullable {
        // In JSON Schema, nullable is expressed differently in v2 vs v3,
        // but using x-nullable works for both as a practical convention.
        // For strict v3, we'd use "nullable: true".
        schema["x-nullable"] = json!(true);
    }

    if let Some(max_len) = col.max_length {
        schema["maxLength"] = json!(max_len);
    }

    if let Some(values) = &col.enum_values {
        schema["enum"] = json!(values);
    }

    if let Some(default) = &col.default_expr {
        schema["default"] = Value::String(default.clone());
    }

    if let Some(comment) = &col.comment {
        schema["description"] = Value::String(comment.clone());
    }

    if col.is_pk {
        schema["x-primary-key"] = json!(true);
    }

    schema
}

fn pg_type_to_json_schema(pg_type: &str) -> Value {
    // Handle array types (leading underscore).
    if let Some(element_type) = pg_type.strip_prefix('_') {
        return json!({
            "type": "array",
            "items": pg_type_to_json_schema(element_type)
        });
    }

    match pg_type {
        // Integer types
        "int2" | "smallint" => json!({"type": "integer", "format": "int16"}),
        "int4" | "integer" | "serial" => json!({"type": "integer", "format": "int32"}),
        "int8" | "bigint" | "bigserial" => json!({"type": "integer", "format": "int64"}),

        // Floating point
        "float4" | "real" => json!({"type": "number", "format": "float"}),
        "float8" | "double precision" => json!({"type": "number", "format": "double"}),
        "numeric" | "decimal" | "money" => json!({"type": "number"}),

        // Boolean
        "bool" | "boolean" => json!({"type": "boolean"}),

        // String types
        "text" | "varchar" | "char" | "bpchar" | "name" | "citext" => {
            json!({"type": "string"})
        }

        // Date/time
        "date" => json!({"type": "string", "format": "date"}),
        "time" | "timetz" => json!({"type": "string", "format": "time"}),
        "timestamp" => json!({"type": "string", "format": "date-time"}),
        "timestamptz" => json!({"type": "string", "format": "date-time"}),
        "interval" => json!({"type": "string"}),

        // UUID
        "uuid" => json!({"type": "string", "format": "uuid"}),

        // JSON
        "json" | "jsonb" => json!({}), // any type

        // Binary
        "bytea" => json!({"type": "string", "format": "binary"}),

        // Network
        "inet" | "cidr" | "macaddr" | "macaddr8" => json!({"type": "string"}),

        // Geometric / range / other
        _ => json!({"type": "string"}),
    }
}

// ---------------------------------------------------------------------------
// OpenAPI 2.0 path generation
// ---------------------------------------------------------------------------

fn build_paths_v2(table: &Table, name: &str) -> Value {
    let mut methods = Map::new();
    let ref_schema = format!("#/definitions/{name}");

    // GET — read rows
    methods.insert(
        "get".into(),
        json!({
            "tags": [name],
            "summary": format!("Read rows from {name}"),
            "parameters": build_query_params_v2(table),
            "responses": {
                "200": {
                    "description": "OK",
                    "schema": {
                        "type": "array",
                        "items": { "$ref": &ref_schema }
                    }
                }
            }
        }),
    );

    // POST — insert rows
    if table.insertable {
        methods.insert(
            "post".into(),
            json!({
                "tags": [name],
                "summary": format!("Insert rows into {name}"),
                "parameters": [{
                    "name": "body",
                    "in": "body",
                    "required": true,
                    "schema": { "$ref": &ref_schema }
                }],
                "responses": {
                    "201": { "description": "Created" }
                }
            }),
        );
    }

    // PATCH — update rows
    if table.updatable {
        methods.insert(
            "patch".into(),
            json!({
                "tags": [name],
                "summary": format!("Update rows in {name}"),
                "parameters": patch_params_v2(table, &ref_schema),
                "responses": {
                    "200": { "description": "OK" },
                    "204": { "description": "No Content" }
                }
            }),
        );
    }

    // DELETE — delete rows
    if table.deletable {
        methods.insert(
            "delete".into(),
            json!({
                "tags": [name],
                "summary": format!("Delete rows from {name}"),
                "parameters": build_filter_params_v2(table),
                "responses": {
                    "200": { "description": "OK" },
                    "204": { "description": "No Content" }
                }
            }),
        );
    }

    Value::Object(methods)
}

fn build_query_params_v2(table: &Table) -> Value {
    let mut params = vec![
        json!({"name": "select", "in": "query", "type": "string",
               "description": "Columns to select (comma-separated)"}),
        json!({"name": "order", "in": "query", "type": "string",
               "description": "Column ordering (e.g. name.asc,age.desc)"}),
        json!({"name": "limit", "in": "query", "type": "integer",
               "description": "Maximum number of rows to return"}),
        json!({"name": "offset", "in": "query", "type": "integer",
               "description": "Number of rows to skip"}),
    ];

    params.extend(filter_params_for_columns_v2(table));
    Value::Array(params)
}

fn build_filter_params_v2(table: &Table) -> Value {
    Value::Array(filter_params_for_columns_v2(table))
}

fn filter_params_for_columns_v2(table: &Table) -> Vec<Value> {
    table
        .columns
        .iter()
        .map(|col| {
            json!({
                "name": &col.name,
                "in": "query",
                "type": "string",
                "description": format!("Filter by {} (e.g. eq.value, gt.value)", col.name)
            })
        })
        .collect()
}

fn patch_params_v2(table: &Table, ref_schema: &str) -> Value {
    let mut params: Vec<Value> = filter_params_for_columns_v2(table);
    params.push(json!({
        "name": "body",
        "in": "body",
        "required": true,
        "schema": { "$ref": ref_schema }
    }));
    Value::Array(params)
}

fn build_rpc_path_v2(func: &Function) -> Value {
    let name = &func.name.name;

    let mut param_props = Map::new();
    for p in &func.params {
        param_props.insert(p.name.clone(), pg_type_to_json_schema(&p.pg_type));
    }

    let mut methods = Map::new();

    let operation = json!({
        "tags": ["(rpc)"],
        "summary": func.comment.as_deref().unwrap_or(name),
        "parameters": [{
            "name": "body",
            "in": "body",
            "required": true,
            "schema": {
                "type": "object",
                "properties": Value::Object(param_props.clone())
            }
        }],
        "responses": {
            "200": {
                "description": "OK",
                "schema": return_type_schema_v2(&func.return_type)
            }
        }
    });

    methods.insert("post".into(), operation);

    // Immutable/stable functions also available via GET.
    if func.volatility != Volatility::Volatile {
        let get_params: Vec<Value> = func
            .params
            .iter()
            .map(|p| {
                json!({
                    "name": &p.name,
                    "in": "query",
                    "type": "string"
                })
            })
            .collect();

        methods.insert(
            "get".into(),
            json!({
                "tags": ["(rpc)"],
                "summary": func.comment.as_deref().unwrap_or(name),
                "parameters": get_params,
                "responses": {
                    "200": {
                        "description": "OK",
                        "schema": return_type_schema_v2(&func.return_type)
                    }
                }
            }),
        );
    }

    Value::Object(methods)
}

fn return_type_schema_v2(ret: &ReturnType) -> Value {
    match ret {
        ReturnType::Void => json!({}),
        ReturnType::Scalar(t) => pg_type_to_json_schema(t),
        ReturnType::SetOf(t) => {
            json!({"type": "array", "items": pg_type_to_json_schema(t)})
        }
        ReturnType::Table(cols) => {
            let mut props = Map::new();
            for col in cols {
                props.insert(col.name.clone(), column_to_json_schema(col));
            }
            json!({"type": "array", "items": {"type": "object", "properties": Value::Object(props)}})
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAPI 3.0 path generation
// ---------------------------------------------------------------------------

fn build_paths_v3(table: &Table, name: &str) -> Value {
    let mut methods = Map::new();
    let ref_schema = format!("#/components/schemas/{name}");

    // GET
    methods.insert(
        "get".into(),
        json!({
            "tags": [name],
            "summary": format!("Read rows from {name}"),
            "parameters": build_query_params_v3(table),
            "responses": {
                "200": {
                    "description": "OK",
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "array",
                                "items": { "$ref": &ref_schema }
                            }
                        }
                    }
                }
            }
        }),
    );

    // POST
    if table.insertable {
        methods.insert(
            "post".into(),
            json!({
                "tags": [name],
                "summary": format!("Insert rows into {name}"),
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": {
                            "schema": {
                                "oneOf": [
                                    { "$ref": &ref_schema },
                                    { "type": "array", "items": { "$ref": &ref_schema } }
                                ]
                            }
                        }
                    }
                },
                "responses": {
                    "201": { "description": "Created" }
                }
            }),
        );
    }

    // PATCH
    if table.updatable {
        methods.insert(
            "patch".into(),
            json!({
                "tags": [name],
                "summary": format!("Update rows in {name}"),
                "parameters": build_filter_params_v3(table),
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": {
                            "schema": { "$ref": &ref_schema }
                        }
                    }
                },
                "responses": {
                    "200": { "description": "OK" },
                    "204": { "description": "No Content" }
                }
            }),
        );
    }

    // DELETE
    if table.deletable {
        methods.insert(
            "delete".into(),
            json!({
                "tags": [name],
                "summary": format!("Delete rows from {name}"),
                "parameters": build_filter_params_v3(table),
                "responses": {
                    "200": { "description": "OK" },
                    "204": { "description": "No Content" }
                }
            }),
        );
    }

    Value::Object(methods)
}

fn build_query_params_v3(table: &Table) -> Value {
    let mut params = vec![
        json!({"name": "select", "in": "query",
               "schema": {"type": "string"},
               "description": "Columns to select (comma-separated)"}),
        json!({"name": "order", "in": "query",
               "schema": {"type": "string"},
               "description": "Column ordering (e.g. name.asc,age.desc)"}),
        json!({"name": "limit", "in": "query",
               "schema": {"type": "integer"},
               "description": "Maximum number of rows to return"}),
        json!({"name": "offset", "in": "query",
               "schema": {"type": "integer"},
               "description": "Number of rows to skip"}),
    ];

    params.extend(filter_params_for_columns_v3(table));
    Value::Array(params)
}

fn build_filter_params_v3(table: &Table) -> Value {
    Value::Array(filter_params_for_columns_v3(table))
}

fn filter_params_for_columns_v3(table: &Table) -> Vec<Value> {
    table
        .columns
        .iter()
        .map(|col| {
            json!({
                "name": &col.name,
                "in": "query",
                "schema": {"type": "string"},
                "description": format!("Filter by {} (e.g. eq.value, gt.value)", col.name)
            })
        })
        .collect()
}

fn build_rpc_path_v3(func: &Function) -> Value {
    let name = &func.name.name;

    let mut param_props = Map::new();
    for p in &func.params {
        param_props.insert(p.name.clone(), pg_type_to_json_schema(&p.pg_type));
    }
    let body_schema = json!({
        "type": "object",
        "properties": Value::Object(param_props)
    });

    let mut methods = Map::new();

    methods.insert(
        "post".into(),
        json!({
            "tags": ["(rpc)"],
            "summary": func.comment.as_deref().unwrap_or(name),
            "requestBody": {
                "required": true,
                "content": {
                    "application/json": {
                        "schema": body_schema
                    }
                }
            },
            "responses": {
                "200": {
                    "description": "OK",
                    "content": {
                        "application/json": {
                            "schema": return_type_schema_v3(&func.return_type)
                        }
                    }
                }
            }
        }),
    );

    if func.volatility != Volatility::Volatile {
        let get_params: Vec<Value> = func
            .params
            .iter()
            .map(|p| {
                json!({
                    "name": &p.name,
                    "in": "query",
                    "schema": {"type": "string"}
                })
            })
            .collect();

        methods.insert(
            "get".into(),
            json!({
                "tags": ["(rpc)"],
                "summary": func.comment.as_deref().unwrap_or(name),
                "parameters": get_params,
                "responses": {
                    "200": {
                        "description": "OK",
                        "content": {
                            "application/json": {
                                "schema": return_type_schema_v3(&func.return_type)
                            }
                        }
                    }
                }
            }),
        );
    }

    Value::Object(methods)
}

fn return_type_schema_v3(ret: &ReturnType) -> Value {
    match ret {
        ReturnType::Void => json!({}),
        ReturnType::Scalar(t) => pg_type_to_json_schema(t),
        ReturnType::SetOf(t) => {
            json!({"type": "array", "items": pg_type_to_json_schema(t)})
        }
        ReturnType::Table(cols) => {
            let mut props = Map::new();
            for col in cols {
                props.insert(col.name.clone(), column_to_json_schema(col));
            }
            json!({"type": "array", "items": {"type": "object", "properties": Value::Object(props)}})
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tables_in_schemas<'a>(cache: &'a SchemaCache, schemas: &[String]) -> Vec<&'a Table> {
    let mut tables: Vec<&Table> = cache
        .tables
        .values()
        .filter(|t| schemas.contains(&t.name.schema))
        .collect();
    tables.sort_by_key(|t| (&t.name.schema, &t.name.name));
    tables
}

fn functions_in_schemas<'a>(cache: &'a SchemaCache, schemas: &[String]) -> Vec<&'a Function> {
    let mut funcs: Vec<&Function> = cache
        .functions
        .values()
        .filter(|f| schemas.contains(&f.name.schema))
        .collect();
    funcs.sort_by_key(|f| (&f.name.schema, &f.name.name));
    funcs
}
