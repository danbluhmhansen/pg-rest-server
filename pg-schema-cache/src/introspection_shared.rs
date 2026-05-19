use std::collections::{HashMap, HashSet};

use pg_schema_cache_types::*;

// ---------------------------------------------------------------------------
// SQL queries
// ---------------------------------------------------------------------------

pub(crate) const TABLES_QUERY: &str = "
SELECT
    n.nspname::text AS table_schema,
    c.relname::text AS table_name,
    c.relkind::text AS kind,
    obj_description(c.oid)::text AS comment,
    CASE
        WHEN c.relkind IN ('r', 'p', 'f') THEN true
        ELSE COALESCE(
            (SELECT v.is_insertable_into = 'YES'
             FROM information_schema.views v
             WHERE v.table_schema = n.nspname AND v.table_name = c.relname),
            false
        )
    END AS insertable,
    CASE
        WHEN c.relkind IN ('r', 'p', 'f') THEN true
        ELSE COALESCE(
            (SELECT v.is_updatable = 'YES'
             FROM information_schema.views v
             WHERE v.table_schema = n.nspname AND v.table_name = c.relname),
            false
        )
    END AS updatable
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = ANY($1)
  AND c.relkind IN ('r', 'v', 'm', 'f', 'p')
ORDER BY n.nspname, c.relname
";

pub(crate) const COLUMNS_QUERY: &str = "
SELECT
    c.table_schema::text,
    c.table_name::text,
    c.column_name::text,
    c.udt_name::text AS pg_type,
    (c.is_nullable = 'YES') AS nullable,
    (c.column_default IS NOT NULL) AS has_default,
    c.column_default::text AS default_expr,
    c.character_maximum_length::int4 AS max_length,
    (c.is_generated = 'ALWAYS') AS is_generated,
    pgd.description::text AS comment
FROM information_schema.columns c
LEFT JOIN pg_catalog.pg_namespace pn ON pn.nspname = c.table_schema
LEFT JOIN pg_catalog.pg_class pc
    ON pc.relname = c.table_name AND pc.relnamespace = pn.oid
LEFT JOIN pg_catalog.pg_description pgd
    ON pgd.objoid = pc.oid AND pgd.objsubid = c.ordinal_position::int
WHERE c.table_schema = ANY($1)
ORDER BY c.table_schema, c.table_name, c.ordinal_position
";

pub(crate) const PRIMARY_KEYS_QUERY: &str = "
SELECT
    n.nspname::text AS table_schema,
    c.relname::text AS table_name,
    a.attname::text AS column_name
FROM pg_catalog.pg_constraint con
JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
JOIN pg_catalog.pg_attribute a
    ON a.attrelid = c.oid AND a.attnum = ANY(con.conkey)
WHERE n.nspname = ANY($1)
  AND con.contype = 'p'
ORDER BY n.nspname, c.relname, a.attnum
";

pub(crate) const FOREIGN_KEYS_QUERY: &str = "
SELECT
    n1.nspname::text AS from_schema,
    c1.relname::text AS from_table,
    n2.nspname::text AS to_schema,
    c2.relname::text AS to_table,
    con.conname::text AS constraint_name,
    array_agg(a1.attname::text ORDER BY pos.ord) AS from_columns,
    array_agg(a2.attname::text ORDER BY pos.ord) AS to_columns
FROM pg_catalog.pg_constraint con
JOIN pg_catalog.pg_class c1 ON c1.oid = con.conrelid
JOIN pg_catalog.pg_namespace n1 ON n1.oid = c1.relnamespace
JOIN pg_catalog.pg_class c2 ON c2.oid = con.confrelid
JOIN pg_catalog.pg_namespace n2 ON n2.oid = c2.relnamespace
CROSS JOIN LATERAL unnest(con.conkey, con.confkey)
    WITH ORDINALITY AS pos(from_attnum, to_attnum, ord)
JOIN pg_catalog.pg_attribute a1
    ON a1.attrelid = c1.oid AND a1.attnum = pos.from_attnum
JOIN pg_catalog.pg_attribute a2
    ON a2.attrelid = c2.oid AND a2.attnum = pos.to_attnum
WHERE con.contype = 'f'
  AND (n1.nspname = ANY($1) OR n2.nspname = ANY($1))
GROUP BY n1.nspname, c1.relname, n2.nspname, c2.relname, con.conname
";

pub(crate) const FUNCTIONS_QUERY: &str = "
SELECT
    n.nspname::text AS schema_name,
    p.proname::text AS function_name,
    p.provolatile::text AS volatility,
    p.proretset AS returns_set,
    rt.typname::text AS return_type_name,
    obj_description(p.oid)::text AS comment,
    p.prokind::text AS prokind,
    p.pronargs::int4 AS num_args,
    p.pronargdefaults::int4 AS num_defaults,
    COALESCE(
        (SELECT array_agg(COALESCE(n, '')) FROM unnest(p.proargnames) AS n),
        ARRAY[]::text[]
    ) AS arg_names,
    COALESCE(p.proargmodes::text[], ARRAY[]::text[]) AS arg_modes,
    COALESCE(
        (SELECT array_agg(t.typname::text ORDER BY u.ord)
         FROM unnest(COALESCE(p.proallargtypes, p.proargtypes::oid[]))
              WITH ORDINALITY AS u(type_oid, ord)
         JOIN pg_catalog.pg_type t ON t.oid = u.type_oid),
        ARRAY[]::text[]
    ) AS arg_type_names
FROM pg_catalog.pg_proc p
JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
JOIN pg_catalog.pg_type rt ON rt.oid = p.prorettype
WHERE n.nspname = ANY($1)
  AND p.prokind IN ('f', 'p')
ORDER BY n.nspname, p.proname
";

pub(crate) const ENUMS_QUERY: &str = "
SELECT
    t.typname::text AS type_name,
    e.enumlabel::text AS enum_value
FROM pg_catalog.pg_type t
JOIN pg_catalog.pg_enum e ON e.enumtypid = t.oid
ORDER BY t.typname, e.enumsortorder
";

// Shared row type for foreign keys, used by both backends.
pub(crate) struct RawForeignKey {
    pub(crate) from_schema: String,
    pub(crate) from_table: String,
    pub(crate) to_schema: String,
    pub(crate) to_table: String,
    pub(crate) constraint_name: String,
    pub(crate) from_columns: Vec<String>,
    pub(crate) to_columns: Vec<String>,
}

// ---------------------------------------------------------------------------
// Builder: assembles a SchemaCache from pre-loaded data
// ---------------------------------------------------------------------------

pub(crate) fn build_from_data(
    mut tables: Vec<Table>,
    columns: HashMap<QualifiedName, Vec<Column>>,
    primary_keys: HashMap<QualifiedName, Vec<String>>,
    raw_fks: Vec<RawForeignKey>,
    functions: HashMap<QualifiedName, Function>,
    enums: HashMap<String, Vec<String>>,
) -> SchemaCache {
    for table in &mut tables {
        if let Some(cols) = columns.get(&table.name) {
            table.columns = cols.clone();
        }
        if let Some(pk_cols) = primary_keys.get(&table.name) {
            table.primary_key = pk_cols.clone();
            for col in &mut table.columns {
                col.is_pk = pk_cols.contains(&col.name);
            }
        }
        for col in &mut table.columns {
            if let Some(vals) = enums.get(&col.pg_type) {
                col.enum_values = Some(vals.clone());
            }
        }
    }

    let table_map: HashMap<QualifiedName, Table> = tables
        .into_iter()
        .map(|mut t| {
            t.rebuild_column_index();
            (t.name.clone(), t)
        })
        .collect();

    let relationships = build_relationships(&raw_fks, &table_map);

    SchemaCache {
        tables: table_map,
        relationships,
        functions,
    }
}

// ---------------------------------------------------------------------------
// Relationship builder
// ---------------------------------------------------------------------------

fn build_relationships(
    fks: &[RawForeignKey],
    tables: &HashMap<QualifiedName, Table>,
) -> Vec<Relationship> {
    let mut rels = Vec::new();

    for fk in fks {
        let from = QualifiedName::new(&fk.from_schema, &fk.from_table);
        let to = QualifiedName::new(&fk.to_schema, &fk.to_table);
        let col_pairs: Vec<(String, String)> = fk
            .from_columns
            .iter()
            .zip(&fk.to_columns)
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();

        rels.push(Relationship {
            from_table: from.clone(),
            to_table: to.clone(),
            columns: col_pairs.clone(),
            rel_type: RelType::ManyToOne,
            join_table: None,
            constraint_name: fk.constraint_name.clone(),
        });

        let reverse_pairs: Vec<(String, String)> = col_pairs
            .iter()
            .map(|(a, b)| (b.clone(), a.clone()))
            .collect();
        rels.push(Relationship {
            from_table: to,
            to_table: from,
            columns: reverse_pairs,
            rel_type: RelType::OneToMany,
            join_table: None,
            constraint_name: fk.constraint_name.clone(),
        });
    }

    rels.extend(infer_m2m(fks, tables));

    rels
}

/// A join table is a table with exactly two FK constraints where every column
/// is either part of a FK or part of the primary key (e.g. `post_tags(post_id, tag_id)`).
fn infer_m2m(fks: &[RawForeignKey], tables: &HashMap<QualifiedName, Table>) -> Vec<Relationship> {
    let mut fks_by_table: HashMap<QualifiedName, Vec<&RawForeignKey>> = HashMap::new();
    for fk in fks {
        let qn = QualifiedName::new(&fk.from_schema, &fk.from_table);
        fks_by_table.entry(qn).or_default().push(fk);
    }

    let mut m2m = Vec::new();

    for (table_qn, table_fks) in &fks_by_table {
        if table_fks.len() != 2 {
            continue;
        }
        let table = match tables.get(table_qn) {
            Some(t) => t,
            None => continue,
        };

        let fk_columns: HashSet<&str> = table_fks
            .iter()
            .flat_map(|fk| fk.from_columns.iter().map(String::as_str))
            .collect();

        let is_join_table = table
            .columns
            .iter()
            .all(|col| fk_columns.contains(col.name.as_str()) || col.is_pk);

        if !is_join_table {
            continue;
        }

        let fk_a = &table_fks[0];
        let fk_b = &table_fks[1];

        let a = QualifiedName::new(&fk_a.to_schema, &fk_a.to_table);
        let b = QualifiedName::new(&fk_b.to_schema, &fk_b.to_table);

        m2m.push(Relationship {
            from_table: a.clone(),
            to_table: b.clone(),
            columns: Vec::new(),
            rel_type: RelType::ManyToMany,
            join_table: Some(table_qn.clone()),
            constraint_name: format!("{}_{}", fk_a.constraint_name, fk_b.constraint_name),
        });

        m2m.push(Relationship {
            from_table: b,
            to_table: a,
            columns: Vec::new(),
            rel_type: RelType::ManyToMany,
            join_table: Some(table_qn.clone()),
            constraint_name: format!("{}_{}", fk_b.constraint_name, fk_a.constraint_name),
        });
    }

    m2m
}
