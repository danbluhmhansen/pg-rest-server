//! Integration tests against a real PostgreSQL instance.
//!
//! Requires:
//!   docker compose up -d
//!
//! Run with:
//!   cargo test -p pg-rest-server-tokio-postgres-pg-wired --test integration

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::watch;
use tower::ServiceExt; // for oneshot

use pg_rest_server_tokio_postgres_pg_wired::config::AppConfig;
use pg_rest_server_tokio_postgres_pg_wired::state::AppState;

const DB_URI: &str = "postgres://authenticator:authenticator@localhost:54322/postgrest_test";
const JWT_SECRET: &str = "reallyreallyreallyreallyverysafe";

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

async fn setup() -> axum::Router {
    let config = AppConfig {
        database: pg_rest_server_tokio_postgres_pg_wired::config::DatabaseConfig {
            uri: DB_URI.to_string(),
            schemas: vec!["api".to_string()],
            anon_role: "web_anon".to_string(),
            pool_size: 5,
            prepared_statements: true,
        },
        server: pg_rest_server_tokio_postgres_pg_wired::config::ServerConfig::default(),
        jwt: pg_rest_server_tokio_postgres_pg_wired::config::JwtConfig {
            secret: JWT_SECRET.to_string(),
        },
    };

    // One-off tokio-postgres connection for schema cache build.
    let (client, conn) = tokio_postgres::connect(&config.database.uri, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        conn.await.ok();
    });
    let cache =
        pg_schema_cache::tokio_postgres::build_schema_cache(&client, &config.database.schemas)
            .await
            .unwrap();
    drop(client);

    let (cache_tx, cache_rx) = watch::channel(Arc::new(cache));

    let jwt_decoding_key = jsonwebtoken::DecodingKey::from_secret(config.jwt.secret.as_bytes());
    let mut jwt_validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    jwt_validation.required_spec_claims = Default::default();

    let mut pool_cfg = pg_pool::ConnPoolConfig::default();
    pool_cfg.addr = "127.0.0.1:54322".to_string();
    pool_cfg.user = "authenticator".to_string();
    pool_cfg.password = "authenticator".to_string();
    pool_cfg.database = "postgrest_test".to_string();
    pool_cfg.min_idle = 1;
    pool_cfg.max_size = 5;
    let conn_pool = pg_pool::ConnPool::<pg_pool::wire::WirePoolable>::new(
        pool_cfg,
        pg_pool::LifecycleHooks::default(),
    )
    .await
    .unwrap();

    let async_pool = pg_wired::AsyncPool::connect(
        "127.0.0.1:54322",
        "authenticator",
        "authenticator",
        "postgrest_test",
        4,
    )
    .await
    .unwrap();

    let state = Arc::new(AppState {
        conn_pool,
        async_pool,
        schema_cache: cache_rx,
        schema_cache_tx: cache_tx,
        openapi_cache: tokio::sync::RwLock::new(("".into(), "".into())),
        config,
        jwt_decoding_key,
        jwt_validation,
        jwt_cache: pg_rest_server_tokio_postgres_pg_wired::auth::JwtCache::new(),
        anon_setup_sql: "BEGIN; SET LOCAL ROLE \"web_anon\"".to_string(),
    });

    // Build OpenAPI cache.
    {
        let specs = state.rebuild_openapi_cache();
        *state.openapi_cache.write().await = specs;
    }

    pg_rest_server_tokio_postgres_pg_wired::build_router(state)
}

fn make_jwt(role: &str) -> String {
    let claims = serde_json::json!({ "role": role });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}

async fn body_string(body: Body) -> String {
    let bytes = body.collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", make_jwt("web_anon")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = body_string(resp.into_body()).await;
    if !status.is_success() {
        eprintln!("[{status}] {uri} → {body}");
    }
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body));
    (status, json)
}

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    role: &str,
    body: Option<serde_json::Value>,
    extra_headers: Vec<(&str, &str)>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", make_jwt(role)))
        .header(header::CONTENT_TYPE, "application/json");

    for (k, v) in &extra_headers {
        builder = builder.header(*k, *v);
    }

    let req_body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };

    let resp = app
        .clone()
        .oneshot(builder.body(req_body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let text = body_string(resp.into_body()).await;
    (status, text)
}

// ===========================================================================
// Schema cache tests
// ===========================================================================

#[tokio::test]
async fn test_schema_cache_loads_tables() {
    let app = setup().await;
    // If setup succeeds, the schema cache loaded without error.
    // Verify via the OpenAPI spec which lists all tables.
    let (status, spec) = get_json(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    let paths = spec.get("paths").unwrap().as_object().unwrap();
    assert!(paths.contains_key("/authors"));
    assert!(paths.contains_key("/books"));
    assert!(paths.contains_key("/tags"));
    assert!(paths.contains_key("/articles"));
    assert!(paths.contains_key("/settings"));
    assert!(paths.contains_key("/rpc/add"));
    assert!(paths.contains_key("/rpc/search_books"));
}

// ===========================================================================
// Read (GET) tests
// ===========================================================================

#[tokio::test]
async fn test_read_all_authors() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.len() >= 3); // seed data has 3, tests may add more
}

#[tokio::test]
async fn test_read_select_columns() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?select=name").await;
    assert_eq!(status, StatusCode::OK);
    let first = &json.as_array().unwrap()[0];
    assert!(first.get("name").is_some());
    assert!(first.get("id").is_none());
}

#[tokio::test]
async fn test_read_filter_eq() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?name=eq.Alice").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Alice");
}

#[tokio::test]
async fn test_read_filter_gt() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/books?pages=gt.400").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.iter().all(|b| b["pages"].as_i64().unwrap() > 400));
}

#[tokio::test]
async fn test_read_filter_in() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?id=in.(1,2)").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_read_filter_is_null() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?bio=is.null").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(!arr.is_empty());
    // Carol is always in the result (seed data).
    assert!(arr.iter().any(|a| a["name"] == "Carol"));
}

#[tokio::test]
async fn test_read_order() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?order=name.desc&id=in.(1,2,3)").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Carol", "Bob", "Alice"]);
}

#[tokio::test]
async fn test_read_limit_offset() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?order=id.asc&limit=2&offset=1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Bob");
}

#[tokio::test]
async fn test_read_count_exact() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::GET,
        "/authors",
        "web_anon",
        None,
        vec![("prefer", "count=exact")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Alice")); // data is returned
                                     // Check Content-Range header was set — we test indirectly via the response
                                     // (headers are in the response, but our helper only returns the body).
}

#[tokio::test]
async fn test_read_count_exact_content_range() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors?limit=2&offset=0&order=id.asc&id=in.(1,2,3)")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", make_jwt("web_anon")),
                )
                .header("prefer", "count=exact")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // 206 Partial Content because 2 of 3 rows returned (PostgREST compat).
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let range = resp
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(range, "0-1/3");
}

#[tokio::test]
async fn test_read_csv() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors?select=id,name&order=id.asc&id=in.(1,2,3)")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", make_jwt("web_anon")),
                )
                .header(header::ACCEPT, "text/csv")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/csv"
    );
    let body = body_string(resp.into_body()).await;
    let lines: Vec<&str> = body.trim().lines().collect();
    assert_eq!(lines[0], "id,name"); // header row
    assert_eq!(lines.len(), 4); // header + 3 seed authors
    assert!(lines[1].contains("Alice"));
}

#[tokio::test]
async fn test_read_nonexistent_table() {
    let app = setup().await;
    let (status, _) = get_json(&app, "/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Embedding tests
// ===========================================================================

#[tokio::test]
async fn test_embed_one_to_many() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?select=name,books(title)&name=eq.Alice").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let books = arr[0]["books"].as_array().unwrap();
    assert_eq!(books.len(), 2);
}

#[tokio::test]
async fn test_embed_many_to_one() {
    let app = setup().await;
    let (status, json) = get_json(
        &app,
        "/books?select=title,authors(name)&title=eq.Learning%20Rust",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["authors"]["name"], "Alice");
}

// ===========================================================================
// Insert (POST) tests
// ===========================================================================

#[tokio::test]
async fn test_insert_and_return() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::POST,
        "/tags",
        "test_user",
        Some(serde_json::json!({"name": format!("test-tag-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos())})),
        vec![("prefer", "return=representation")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert!(arr[0]["name"].as_str().unwrap().starts_with("test-tag-"));
}

#[tokio::test]
async fn test_insert_minimal() {
    let app = setup().await;
    let (status, _) = request(
        &app,
        Method::POST,
        "/tags",
        "test_user",
        Some(serde_json::json!({"name": format!("eph-tag-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos())})),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

// ===========================================================================
// Update (PATCH) tests
// ===========================================================================

#[tokio::test]
async fn test_update_with_filter() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::PATCH,
        "/settings?key=eq.theme",
        "test_user",
        Some(serde_json::json!({"value": "light"})),
        vec![("prefer", "return=representation")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json.as_array().unwrap()[0]["value"], "light");
}

// ===========================================================================
// Delete (DELETE) tests
// ===========================================================================

#[tokio::test]
async fn test_delete_with_filter() {
    let app = setup().await;
    // Insert a row to delete.
    request(
        &app,
        Method::POST,
        "/tags",
        "test_user",
        Some(serde_json::json!({"name": "to-delete"})),
        vec![],
    )
    .await;

    let (status, _) = request(
        &app,
        Method::DELETE,
        "/tags?name=eq.to-delete",
        "test_user",
        None,
        vec![],
    )
    .await;
    assert!(status == StatusCode::NO_CONTENT || status == StatusCode::OK);
}

// ===========================================================================
// Upsert tests
// ===========================================================================

#[tokio::test]
async fn test_upsert_merge_duplicates() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::POST,
        "/settings",
        "test_user",
        Some(serde_json::json!({"key": "site_name", "value": "Updated Site"})),
        vec![(
            "prefer",
            "return=representation,resolution=merge-duplicates",
        )],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json.as_array().unwrap()[0]["value"], "Updated Site");
}

// ===========================================================================
// RPC (function call) tests
// ===========================================================================

#[tokio::test]
async fn test_rpc_scalar() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::POST,
        "/rpc/add",
        "web_anon",
        Some(serde_json::json!({"a": 3, "b": 4})),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Result should contain 7
    assert!(body.contains('7'), "expected 7 in: {body}");
}

#[tokio::test]
async fn test_rpc_setof() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::POST,
        "/rpc/search_books",
        "web_anon",
        Some(serde_json::json!({"query": "Rust"})),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2); // Learning Rust, Advanced Rust
}

#[tokio::test]
async fn test_rpc_default_param() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::POST,
        "/rpc/greet",
        "web_anon",
        Some(serde_json::json!({})),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Hello, world!"), "got: {body}");
}

#[tokio::test]
async fn test_rpc_get_immutable() {
    let app = setup().await;
    let (status, body) = get_json(&app, "/rpc/add?a=10&b=20").await;
    assert_eq!(status, StatusCode::OK);
    let text = body.to_string();
    assert!(text.contains("30"), "expected 30 in: {text}");
}

// ===========================================================================
// RLS tests
// ===========================================================================

#[tokio::test]
async fn test_rls_anon_sees_only_published() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/articles").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["status"], "published");
}

#[tokio::test]
async fn test_rls_user_sees_all() {
    let app = setup().await;
    let (status, body) = request(&app, Method::GET, "/articles", "test_user", None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

// ===========================================================================
// Health endpoints
// ===========================================================================

#[tokio::test]
async fn test_live() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/live").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_ready() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// OpenAPI spec tests
// ===========================================================================

#[tokio::test]
async fn test_openapi_v2() {
    let app = setup().await;
    let (status, spec) = get_json(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["swagger"], "2.0");
    // Required top-level Swagger 2.0 fields
    assert!(spec["info"].is_object());
    assert!(spec["paths"].is_object());
    assert!(spec["definitions"].is_object());
    assert!(spec["basePath"].is_string());
    // Has our tables as definitions
    assert!(spec["definitions"].get("authors").is_some());
    assert!(spec["definitions"].get("books").is_some());
    // Has paths for tables
    assert!(spec["paths"].get("/authors").is_some());
    assert!(spec["paths"].get("/books").is_some());
    // Has RPC paths
    assert!(spec["paths"].get("/rpc/add").is_some());
    // Table definitions have properties
    let authors = &spec["definitions"]["authors"];
    assert_eq!(authors["type"], "object");
    assert!(authors["properties"].is_object());
    assert!(authors["properties"]["name"].is_object());
    // Property types map correctly
    assert_eq!(authors["properties"]["name"]["type"], "string");
}

#[tokio::test]
async fn test_openapi_v3() {
    let app = setup().await;
    let (status, spec) = get_json(&app, "/?openapi-version=3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["openapi"], "3.0.3");
    // Required top-level OpenAPI 3.0 fields
    assert!(spec["info"].is_object());
    assert!(spec["paths"].is_object());
    assert!(spec["components"].is_object());
    assert!(spec["components"]["schemas"].is_object());
    assert!(spec["servers"].is_array());
    // Has our tables as schemas
    assert!(spec["components"]["schemas"].get("authors").is_some());
    assert!(spec["components"]["schemas"].get("books").is_some());
    // Has paths
    assert!(spec["paths"].get("/authors").is_some());
    assert!(spec["paths"].get("/rpc/add").is_some());
    // Schema structure
    let authors = &spec["components"]["schemas"]["authors"];
    assert_eq!(authors["type"], "object");
    assert!(authors["properties"]["name"]["type"].is_string());
    // POST has requestBody (v3 style, not v2 parameters)
    let post = &spec["paths"]["/authors"]["post"];
    assert!(post["requestBody"].is_object());
}

// ===========================================================================
// Logical operators (or/and)
// ===========================================================================

#[tokio::test]
async fn test_filter_or() {
    let app = setup().await;
    let (status, json) = get_json(
        &app,
        "/authors?or=(name.eq.Alice,name.eq.Carol)&order=id.asc",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Alice");
    assert_eq!(arr[1]["name"], "Carol");
}

#[tokio::test]
async fn test_filter_nested_and_or() {
    let app = setup().await;
    // Authors named Alice OR (named Bob AND have a bio)
    let (status, json) = get_json(
        &app,
        "/authors?or=(name.eq.Alice,and(name.eq.Bob,bio.not.is.null))&order=id.asc",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Alice");
    assert_eq!(arr[1]["name"], "Bob");
}

// ===========================================================================
// not.is.null
// ===========================================================================

#[tokio::test]
async fn test_filter_not_is_null() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?bio=not.is.null&order=id.asc").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    // Alice and Bob have bios, Carol does not.
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Alice");
    assert_eq!(arr[1]["name"], "Bob");
}

// ===========================================================================
// Select type cast
// ===========================================================================

#[tokio::test]
async fn test_select_cast() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?select=id::text,name&id=eq.1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    // id should be returned as a string due to ::text cast
    assert_eq!(arr[0]["id"], "1");
}

// ===========================================================================
// Singular response
// ===========================================================================

#[tokio::test]
async fn test_singular_response() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors?id=eq.1")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", make_jwt("web_anon")),
                )
                .header(header::ACCEPT, "application/vnd.pgrst.object+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("application/vnd.pgrst.object+json"));
    let body = body_string(resp.into_body()).await;
    let obj: serde_json::Value = serde_json::from_str(&body).unwrap();
    // Should be a single object, not an array.
    assert!(obj.is_object());
    assert_eq!(obj["name"], "Alice");
}

#[tokio::test]
async fn test_singular_response_406_multiple() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", make_jwt("web_anon")),
                )
                .header(header::ACCEPT, "application/vnd.pgrst.object+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_ACCEPTABLE);
}

// ===========================================================================
// Spread embed
// ===========================================================================

#[tokio::test]
async fn test_spread_embed() {
    let app = setup().await;
    let (status, json) = get_json(
        &app,
        "/books?select=title,...authors(name)&title=eq.Learning%20Rust",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    // Author name should be spread into the book row.
    assert_eq!(arr[0]["title"], "Learning Rust");
    assert_eq!(arr[0]["name"], "Alice");
    // Should NOT have an "authors" nested object.
    assert!(arr[0].get("authors").is_none());
}

// ===========================================================================
// EXPLAIN
// ===========================================================================

#[tokio::test]
async fn test_explain() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/authors")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", make_jwt("web_anon")),
                )
                .header(header::ACCEPT, "application/vnd.pgrst.plan+json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("application/vnd.pgrst.plan+json"));
    let body = body_string(resp.into_body()).await;
    // EXPLAIN output should contain query plan info.
    assert!(
        body.contains("Plan") || body.contains("plan"),
        "expected plan in: {body}"
    );
}

// ===========================================================================
// Generated columns
// ===========================================================================

#[tokio::test]
async fn test_generated_column_excluded_from_insert() {
    let app = setup().await;
    // Insert a product — the `tax` column is generated and should be excluded.
    let (status, body) = request(
        &app,
        Method::POST,
        "/products",
        "test_user",
        Some(serde_json::json!({"name": "Doohickey", "price": 50.0})),
        vec![("prefer", "return=representation")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = json.as_array().unwrap();
    // tax should be auto-computed as price * 0.1 = 5.0
    let tax = arr[0]["tax"].as_f64().unwrap();
    assert!((tax - 5.0).abs() < 0.01, "expected tax=5.0, got {tax}");
}

// ===========================================================================
// on_conflict with specific columns
// ===========================================================================

#[tokio::test]
async fn test_on_conflict_specific_columns() {
    let app = setup().await;
    // Upsert using the `name` unique constraint on tags (not PK).
    let (status, body) = request(
        &app,
        Method::POST,
        "/tags?on_conflict=name",
        "test_user",
        Some(serde_json::json!({"name": "programming"})),
        vec![(
            "prefer",
            "return=representation,resolution=merge-duplicates",
        )],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr[0]["name"], "programming");
}

// ===========================================================================
// Edge cases
// ===========================================================================

#[tokio::test]
async fn test_empty_table_returns_empty_array() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/products?name=eq.nonexistent").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_special_characters_in_filter_value() {
    let app = setup().await;
    // Filter with special characters — should be safely parameterized.
    let (status, json) = get_json(&app, "/authors?name=eq.O'Brien%20%22The%22").await;
    assert_eq!(status, StatusCode::OK);
    // No match expected, but no SQL injection.
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_select_nonexistent_column_still_works() {
    // PostgreSQL will error if we select a column that doesn't exist.
    let app = setup().await;
    let (status, _) = get_json(&app, "/authors?select=id,fake_column").await;
    // Should be a database error (42703: column does not exist).
    assert!(status == StatusCode::INTERNAL_SERVER_ERROR || status == StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_filter_like_with_percent() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?name=like.A*").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Alice");
}

#[tokio::test]
async fn test_filter_ilike() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors?name=ilike.alice").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Alice");
}

#[tokio::test]
async fn test_multiple_filters_anded() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/books?pages=gt.200&pages=lt.400").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.iter().all(|b| {
        let p = b["pages"].as_i64().unwrap();
        p > 200 && p < 400
    }));
}

#[tokio::test]
async fn test_insert_with_null_value() {
    let app = setup().await;
    let (status, body) = request(
        &app,
        Method::POST,
        "/authors",
        "test_user",
        Some(serde_json::json!({"name": "NullBio", "bio": null})),
        vec![("prefer", "return=representation")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr[0]["name"], "NullBio");
    assert!(arr[0]["bio"].is_null());
}

#[tokio::test]
async fn test_read_view() {
    let app = setup().await;
    let (status, json) = get_json(&app, "/authors_with_books?order=id.asc").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.len() >= 3);
    // Alice has 2 books.
    assert_eq!(arr[0]["book_count"], 2);
}

#[tokio::test]
async fn test_reload_endpoint() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/reload")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp.into_body()).await;
    assert!(body.contains("schema cache reloaded"));
}

#[tokio::test]
async fn test_metrics_endpoint() {
    let app = setup().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp.into_body()).await;
    assert!(body.contains("pg_rest_pool_size"));
    assert!(body.contains("pg_rest_schema_tables"));
}
