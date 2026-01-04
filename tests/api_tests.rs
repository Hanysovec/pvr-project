use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use dashmap::DashMap;
use pvr_project_simulationcraft_sol0123::{server, utils};
use sqlx::Row;
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tower::ServiceExt;
use tower_sessions::{Expiry, SessionManagerLayer, cookie::time::Duration as SessionDuration};
use tower_sessions_sqlx_store::SqliteStore;

// Helper function to create server and in-memory database
async fn spawn_app() -> (axum::Router, SqlitePool, utils::AppState) {
    let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::query(
        "
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            role TEXT NOT NULL DEFAULT 'User'
        );
    ",
    )
    .execute(&db)
    .await
    .unwrap();

    sqlx::query(
        "
        CREATE TABLE IF NOT EXISTS history (
            id TEXT PRIMARY KEY,
            user_id INTEGER,
            sim_type TEXT NOT NULL,
            dps REAL,
            result_json TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
    ",
    )
    .execute(&db)
    .await
    .unwrap();

    let (tx_premium, _) = mpsc::channel(100);
    let (tx_standard, _) = mpsc::channel(100);

    let state = utils::AppState {
        premium_queue: tx_premium,
        standard_queue: tx_standard,
        queue_tracker: Arc::new(Mutex::new(Vec::new())),
        job_statuses: Arc::new(DashMap::new()),
        http_client: reqwest::Client::new(),
        blizzard_auth: Arc::new(Mutex::new(utils::BlizzardAuth {
            client_id: "fake".to_string(),
            client_secret: "fake".to_string(),
            access_token: None,
            expires_at: std::time::Instant::now(),
        })),
        item_icon_cache: Arc::new(DashMap::new()),
        db: db.clone(),
    };

    let session_store = SqliteStore::new(db.clone());
    session_store.migrate().await.unwrap();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_expiry(Expiry::OnInactivity(SessionDuration::seconds(3600)));

    let router = server::create_router(state.clone(), session_layer);

    (router, db, state)
}

#[tokio::test]
async fn test_index_page_loads() {
    let (app, _, _) = spawn_app().await;

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_user_registration() {
    let (app, db, _) = spawn_app().await;

    let body = "username=testuser&password=password123";

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/user/register")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let user_exists = sqlx::query("SELECT username, role FROM users WHERE username = ?")
        .bind("testuser")
        .fetch_optional(&db)
        .await
        .unwrap();

    assert!(user_exists.is_some());
    let row = user_exists.unwrap();
    let username: String = row.try_get("username").unwrap();
    let role: String = row.try_get("role").unwrap();

    assert_eq!(username, "testuser");
    assert_eq!(role, "User");
}

#[tokio::test]
async fn test_login_success() {
    let (app, _, _) = spawn_app().await;

    let register_body = "username=loginuser&password=password123";
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/user/register")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(register_body))
                .unwrap(),
        )
        .await
        .unwrap();

    let login_body = "username=loginuser&password=password123";
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/user/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(login_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let cookie = response.headers().get("set-cookie");
    assert!(
        cookie.is_some(),
        "Response should contain Set-Cookie header"
    );
}

#[tokio::test]
async fn test_duplicate_registration_fails() {
    let (app, _, _) = spawn_app().await;
    let body = "username=uniqueuser&password=pass";
    let response1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/user/register")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response1.status(), StatusCode::SEE_OTHER);

    let response2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/user/register")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_login_failure() {
    let (app, _, _) = spawn_app().await;

    let login_body = "username=.!67!.&password=666";

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/user/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(login_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_quicksim_submission_adds_to_state() {
    let (app, _, state) = spawn_app().await;

    let body = "input_content=warrior=1";

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/quicksim/run")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let location = response
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap();
    let id = location.replace("/quicksim/", "");

    let exists = state.job_statuses.contains_key(&id);
    assert!(exists, "Job ID {} was not found in job_statuses", id);

    let status = state.job_statuses.get(&id).unwrap();
    assert_eq!(
        status.value(),
        &utils::SimStatus::Failed("Queue full".to_string())
    );
}
