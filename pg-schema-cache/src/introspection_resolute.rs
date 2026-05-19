use std::collections::HashMap;

use resolute::{Executor, FromRow};

use crate::error::SchemaCacheError;
use crate::introspection_shared::{self, RawForeignKey};
use pg_schema_cache_types::*;

// ---------------------------------------------------------------------------
// Row mappings (FromRow-derived)
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct TableRow {
    table_schema: String,
    table_name: String,
    kind: String,
    comment: Option<String>,
    insertable: bool,
    updatable: bool,
}

#[derive(FromRow)]
struct ColumnRow {
    table_schema: String,
    table_name: String,
    column_name: String,
    pg_type: String,
    nullable: bool,
    has_default: bool,
    default_expr: Option<String>,
    max_length: Option<i32>,
    is_generated: bool,
    comment: Option<String>,
}

#[derive(FromRow)]
struct PrimaryKeyRow {
    table_schema: String,
    table_name: String,
    column_name: String,
}

#[derive(FromRow)]
struct ForeignKeyRow {
    from_schema: String,
    from_table: String,
    to_schema: String,
    to_table: String,
    constraint_name: String,
    from_columns: Vec<String>,
    to_columns: Vec<String>,
}

#[derive(FromRow)]
struct FunctionRow {
    schema_name: String,
    function_name: String,
    volatility: String,
    returns_set: bool,
    return_type_name: String,
    comment: Option<String>,
    prokind: String,
    num_args: i32,
    num_defaults: i32,
    arg_names: Vec<String>,
    arg_modes: Vec<String>,
    arg_type_names: Vec<String>,
}

#[derive(FromRow)]
struct EnumRow {
    type_name: String,
    enum_value: String,
}

// ---------------------------------------------------------------------------
// Public builder
// ---------------------------------------------------------------------------

pub async fn build_schema_cache(
    db: &impl Executor,
    schemas: &[String],
) -> Result<SchemaCache, SchemaCacheError> {
    let schemas_vec = schemas.to_vec();

    let tables = load_tables(db, &schemas_vec).await?;
    let columns = load_columns(db, &schemas_vec).await?;
    let primary_keys = load_primary_keys(db, &schemas_vec).await?;
    let raw_fks = load_foreign_keys(db, &schemas_vec).await?;
    let functions = load_functions(db, &schemas_vec).await?;
    let enums = load_enums(db).await?;

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
// Query loaders
// ---------------------------------------------------------------------------

async fn load_tables(
    db: &impl Executor,
    schemas: &Vec<String>,
) -> Result<Vec<Table>, SchemaCacheError> {
    let rows = db
        .query(introspection_shared::TABLES_QUERY, &[schemas])
        .await?;
    let mut tables = Vec::with_capacity(rows.len());
    for row in &rows {
        let r = TableRow::from_row(row)?;
        let is_view = r.kind == "v" || r.kind == "m";
        tables.push(Table {
            name: QualifiedName::new(r.table_schema, r.table_name),
            columns: Vec::new(),
            column_index: HashMap::new(),
            primary_key: Vec::new(),
            is_view,
            insertable: r.insertable,
            updatable: r.updatable,
            deletable: r.updatable,
            comment: r.comment,
        });
    }
    Ok(tables)
}

async fn load_columns(
    db: &impl Executor,
    schemas: &Vec<String>,
) -> Result<HashMap<QualifiedName, Vec<Column>>, SchemaCacheError> {
    let rows = db
        .query(introspection_shared::COLUMNS_QUERY, &[schemas])
        .await?;
    let mut map: HashMap<QualifiedName, Vec<Column>> = HashMap::new();
    for row in &rows {
        let r = ColumnRow::from_row(row)?;
        let qn = QualifiedName::new(r.table_schema, r.table_name);
        map.entry(qn).or_default().push(Column {
            name: r.column_name,
            pg_type: r.pg_type,
            nullable: r.nullable,
            has_default: r.has_default,
            default_expr: r.default_expr,
            max_length: r.max_length,
            is_pk: false,
            is_generated: r.is_generated,
            comment: r.comment,
            enum_values: None,
        });
    }
    Ok(map)
}

async fn load_primary_keys(
    db: &impl Executor,
    schemas: &Vec<String>,
) -> Result<HashMap<QualifiedName, Vec<String>>, SchemaCacheError> {
    let rows = db
        .query(introspection_shared::PRIMARY_KEYS_QUERY, &[schemas])
        .await?;
    let mut map: HashMap<QualifiedName, Vec<String>> = HashMap::new();
    for row in &rows {
        let r = PrimaryKeyRow::from_row(row)?;
        let qn = QualifiedName::new(r.table_schema, r.table_name);
        map.entry(qn).or_default().push(r.column_name);
    }
    Ok(map)
}

async fn load_foreign_keys(
    db: &impl Executor,
    schemas: &Vec<String>,
) -> Result<Vec<RawForeignKey>, SchemaCacheError> {
    let rows = db
        .query(introspection_shared::FOREIGN_KEYS_QUERY, &[schemas])
        .await?;
    let mut fks = Vec::with_capacity(rows.len());
    for row in &rows {
        let r = ForeignKeyRow::from_row(row)?;
        fks.push(RawForeignKey {
            from_schema: r.from_schema,
            from_table: r.from_table,
            to_schema: r.to_schema,
            to_table: r.to_table,
            constraint_name: r.constraint_name,
            from_columns: r.from_columns,
            to_columns: r.to_columns,
        });
    }
    Ok(fks)
}

async fn load_functions(
    db: &impl Executor,
    schemas: &Vec<String>,
) -> Result<HashMap<QualifiedName, Function>, SchemaCacheError> {
    let rows = db
        .query(introspection_shared::FUNCTIONS_QUERY, &[schemas])
        .await?;
    let mut map = HashMap::new();

    for row in &rows {
        let r = FunctionRow::from_row(row)?;

        let volatility = match r.volatility.as_str() {
            "i" => Volatility::Immutable,
            "s" => Volatility::Stable,
            _ => Volatility::Volatile,
        };

        let return_type = if r.return_type_name == "void" {
            ReturnType::Void
        } else if r.returns_set {
            ReturnType::SetOf(r.return_type_name)
        } else {
            ReturnType::Scalar(r.return_type_name)
        };

        let has_modes = !r.arg_modes.is_empty();
        let mut params = Vec::new();
        let mut in_count: i32 = 0;

        for (i, type_name) in r.arg_type_names.iter().enumerate() {
            let mode = if has_modes {
                r.arg_modes.get(i).map(|s| s.as_str()).unwrap_or("i")
            } else {
                "i"
            };
            if mode == "i" || mode == "b" || mode == "v" {
                in_count += 1;
                params.push(FuncParam {
                    name: r.arg_names.get(i).cloned().unwrap_or_default(),
                    pg_type: type_name.clone(),
                    has_default: in_count > (r.num_args - r.num_defaults),
                });
            }
        }

        let qn = QualifiedName::new(r.schema_name, r.function_name);
        map.insert(
            qn.clone(),
            Function {
                name: qn,
                params,
                return_type,
                volatility,
                is_procedure: r.prokind == "p",
                comment: r.comment,
            },
        );
    }

    Ok(map)
}

async fn load_enums(db: &impl Executor) -> Result<HashMap<String, Vec<String>>, SchemaCacheError> {
    let rows = db.query(introspection_shared::ENUMS_QUERY, &[]).await?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in &rows {
        let r = EnumRow::from_row(row)?;
        map.entry(r.type_name).or_default().push(r.enum_value);
    }
    Ok(map)
}
