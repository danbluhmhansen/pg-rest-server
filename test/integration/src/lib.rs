use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

pub const DB_URI: &str = "postgres://authenticator:authenticator@localhost:54322/postgrest_test";
pub const JWT_SECRET: &str = "reallyreallyreallyreallyverysafe";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn make_jwt(role: &str) -> String {
    let claims = serde_json::json!({ "role": role });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}

pub async fn body_string(body: Body) -> String {
    let bytes = body.collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

pub async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
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

pub async fn request(
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

pub async fn test_schema_cache_loads_tables(app: &axum::Router) {
    let (status, spec) = get_json(app, "/").await;
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

pub async fn test_read_all_authors(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.len() >= 3);
}

pub async fn test_read_select_columns(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?select=name").await;
    assert_eq!(status, StatusCode::OK);
    let first = &json.as_array().unwrap()[0];
    assert!(first.get("name").is_some());
    assert!(first.get("id").is_none());
}

pub async fn test_read_filter_eq(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?name=eq.Alice").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Alice");
}

pub async fn test_read_filter_gt(app: &axum::Router) {
    let (status, json) = get_json(app, "/books?pages=gt.400").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.iter().all(|b| b["pages"].as_i64().unwrap() > 400));
}

pub async fn test_read_filter_in(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?id=in.(1,2)").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.as_array().unwrap().len(), 2);
}

pub async fn test_read_filter_is_null(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?bio=is.null").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(!arr.is_empty());
    assert!(arr.iter().any(|a| a["name"] == "Carol"));
}

pub async fn test_read_order(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?order=name.desc&id=in.(1,2,3)").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Carol", "Bob", "Alice"]);
}

pub async fn test_read_limit_offset(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?order=id.asc&limit=2&offset=1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Bob");
}

pub async fn test_read_count_exact(app: &axum::Router) {
    let (status, body) = request(
        app,
        Method::GET,
        "/authors",
        "web_anon",
        None,
        vec![("prefer", "count=exact")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Alice"));
}

pub async fn test_read_count_exact_content_range(app: &axum::Router) {
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
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let range = resp
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(range, "0-1/3");
}

pub async fn test_read_csv(app: &axum::Router) {
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
    assert_eq!(lines[0], "id,name");
    assert_eq!(lines.len(), 4);
    assert!(lines[1].contains("Alice"));
}

pub async fn test_read_nonexistent_table(app: &axum::Router) {
    let (status, _) = get_json(app, "/nonexistent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ===========================================================================
// Embedding tests
// ===========================================================================

pub async fn test_embed_one_to_many(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?select=name,books(title)&name=eq.Alice").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let books = arr[0]["books"].as_array().unwrap();
    assert_eq!(books.len(), 2);
}

pub async fn test_embed_many_to_one(app: &axum::Router) {
    let (status, json) = get_json(
        app,
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

pub async fn test_insert_and_return(app: &axum::Router) {
    let (status, body) = request(
        app,
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

pub async fn test_insert_minimal(app: &axum::Router) {
    let (status, _) = request(
        app,
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

pub async fn test_update_with_filter(app: &axum::Router) {
    let (status, body) = request(
        app,
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

pub async fn test_delete_with_filter(app: &axum::Router) {
    request(
        app,
        Method::POST,
        "/tags",
        "test_user",
        Some(serde_json::json!({"name": "to-delete"})),
        vec![],
    )
    .await;

    let (status, _) = request(
        app,
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

pub async fn test_upsert_merge_duplicates(app: &axum::Router) {
    let (status, body) = request(
        app,
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

pub async fn test_rpc_scalar(app: &axum::Router) {
    let (status, body) = request(
        app,
        Method::POST,
        "/rpc/add",
        "web_anon",
        Some(serde_json::json!({"a": 3, "b": 4})),
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains('7'), "expected 7 in: {body}");
}

pub async fn test_rpc_setof(app: &axum::Router) {
    let (status, body) = request(
        app,
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
    assert_eq!(arr.len(), 2);
}

pub async fn test_rpc_default_param(app: &axum::Router) {
    let (status, body) = request(
        app,
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

pub async fn test_rpc_get_immutable(app: &axum::Router) {
    let (status, body) = get_json(app, "/rpc/add?a=10&b=20").await;
    assert_eq!(status, StatusCode::OK);
    let text = body.to_string();
    assert!(text.contains("30"), "expected 30 in: {text}");
}

// ===========================================================================
// RLS tests
// ===========================================================================

pub async fn test_rls_anon_sees_only_published(app: &axum::Router) {
    let (status, json) = get_json(app, "/articles").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["status"], "published");
}

pub async fn test_rls_user_sees_all(app: &axum::Router) {
    let (status, body) = request(app, Method::GET, "/articles", "test_user", None, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

// ===========================================================================
// Health endpoints
// ===========================================================================

pub async fn test_live(app: &axum::Router) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/live").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

pub async fn test_ready(app: &axum::Router) {
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

pub async fn test_openapi_v2(app: &axum::Router) {
    let (status, spec) = get_json(app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["swagger"], "2.0");
    assert!(spec["info"].is_object());
    assert!(spec["paths"].is_object());
    assert!(spec["definitions"].is_object());
    assert!(spec["basePath"].is_string());
    assert!(spec["definitions"].get("authors").is_some());
    assert!(spec["definitions"].get("books").is_some());
    assert!(spec["paths"].get("/authors").is_some());
    assert!(spec["paths"].get("/books").is_some());
    assert!(spec["paths"].get("/rpc/add").is_some());
    let authors = &spec["definitions"]["authors"];
    assert_eq!(authors["type"], "object");
    assert!(authors["properties"].is_object());
    assert!(authors["properties"]["name"].is_object());
    assert_eq!(authors["properties"]["name"]["type"], "string");
}

pub async fn test_openapi_v3(app: &axum::Router) {
    let (status, spec) = get_json(app, "/?openapi-version=3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["openapi"], "3.0.3");
    assert!(spec["info"].is_object());
    assert!(spec["paths"].is_object());
    assert!(spec["components"].is_object());
    assert!(spec["components"]["schemas"].is_object());
    assert!(spec["servers"].is_array());
    assert!(spec["components"]["schemas"].get("authors").is_some());
    assert!(spec["components"]["schemas"].get("books").is_some());
    assert!(spec["paths"].get("/authors").is_some());
    assert!(spec["paths"].get("/rpc/add").is_some());
    let authors = &spec["components"]["schemas"]["authors"];
    assert_eq!(authors["type"], "object");
    assert!(authors["properties"]["name"]["type"].is_string());
    let post = &spec["paths"]["/authors"]["post"];
    assert!(post["requestBody"].is_object());
}

// ===========================================================================
// Logical operators (or/and)
// ===========================================================================

pub async fn test_filter_or(app: &axum::Router) {
    let (status, json) = get_json(
        app,
        "/authors?or=(name.eq.Alice,name.eq.Carol)&order=id.asc",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Alice");
    assert_eq!(arr[1]["name"], "Carol");
}

pub async fn test_filter_nested_and_or(app: &axum::Router) {
    let (status, json) = get_json(
        app,
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

pub async fn test_filter_not_is_null(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?bio=not.is.null&order=id.asc").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "Alice");
    assert_eq!(arr[1]["name"], "Bob");
}

// ===========================================================================
// Select type cast
// ===========================================================================

pub async fn test_select_cast(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?select=id::text,name&id=eq.1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "1");
}

// ===========================================================================
// Singular response
// ===========================================================================

pub async fn test_singular_response(app: &axum::Router) {
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
    assert!(obj.is_object());
    assert_eq!(obj["name"], "Alice");
}

pub async fn test_singular_response_406_multiple(app: &axum::Router) {
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

pub async fn test_spread_embed(app: &axum::Router) {
    let (status, json) = get_json(
        app,
        "/books?select=title,...authors(name)&title=eq.Learning%20Rust",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "Learning Rust");
    assert_eq!(arr[0]["name"], "Alice");
    assert!(arr[0].get("authors").is_none());
}

// ===========================================================================
// EXPLAIN
// ===========================================================================

pub async fn test_explain(app: &axum::Router) {
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
    assert!(
        body.contains("Plan") || body.contains("plan"),
        "expected plan in: {body}"
    );
}

// ===========================================================================
// Generated columns
// ===========================================================================

pub async fn test_generated_column_excluded_from_insert(app: &axum::Router) {
    let (status, body) = request(
        app,
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
    let tax = arr[0]["tax"].as_f64().unwrap();
    assert!((tax - 5.0).abs() < 0.01, "expected tax=5.0, got {tax}");
}

// ===========================================================================
// on_conflict with specific columns
// ===========================================================================

pub async fn test_on_conflict_specific_columns(app: &axum::Router) {
    let (status, body) = request(
        app,
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

pub async fn test_empty_table_returns_empty_array(app: &axum::Router) {
    let (status, json) = get_json(app, "/products?name=eq.nonexistent").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.as_array().unwrap().len(), 0);
}

pub async fn test_special_characters_in_filter_value(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?name=eq.O'Brien%20%22The%22").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.as_array().unwrap().len(), 0);
}

pub async fn test_select_nonexistent_column_still_works(app: &axum::Router) {
    let (status, _) = get_json(app, "/authors?select=id,fake_column").await;
    assert!(status == StatusCode::INTERNAL_SERVER_ERROR || status == StatusCode::BAD_REQUEST);
}

pub async fn test_filter_like_with_percent(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?name=like.A*").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Alice");
}

pub async fn test_filter_ilike(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors?name=ilike.alice").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "Alice");
}

pub async fn test_multiple_filters_anded(app: &axum::Router) {
    let (status, json) = get_json(app, "/books?pages=gt.200&pages=lt.400").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.iter().all(|b| {
        let p = b["pages"].as_i64().unwrap();
        p > 200 && p < 400
    }));
}

pub async fn test_insert_with_null_value(app: &axum::Router) {
    let (status, body) = request(
        app,
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

pub async fn test_read_view(app: &axum::Router) {
    let (status, json) = get_json(app, "/authors_with_books?order=id.asc").await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert!(arr.len() >= 3);
    assert_eq!(arr[0]["book_count"], 2);
}

pub async fn test_reload_endpoint(app: &axum::Router) {
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

pub async fn test_metrics_endpoint(app: &axum::Router) {
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
