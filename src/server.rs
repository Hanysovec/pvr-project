use axum::{
    Router,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use dashmap::DashMap;
use sqlx::sqlite::SqlitePool;
use std::{env, fs, net::SocketAddr, sync::Arc, time::Instant};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tower_http::services::ServeDir;
use tower_sessions::{
    Expiry, Session, SessionManagerLayer, cookie::time::Duration as SessionDuration,
};
use tower_sessions_sqlx_store::SqliteStore;

use crate::{
    admin,
    api::{
        get_item_icon_only, get_item_tooltip, get_result_data_from_db, get_status_check,
        get_wowhead_tooltip,
    },
    quicksim::{get_quicksim, post_simulation},
    simulate,
    topgear::{
        get_topgear_result_page, post_run_topgear_batch, post_topgear, process_topgear_batch,
    },
    users,
    utils::{self, JobType, SimStatus, SimulationJob, extract_dps_from_json, generate_header},
};

const MAX_RUNNING_SIMS: usize = 2;

pub async fn run_server() -> Result<(), String> {
    dotenvy::dotenv().ok();
    let database_url = env::var("DATABASE_URL").unwrap_or("sqlite:sims.db?mode=rwc".to_string());
    let db = SqlitePool::connect(&database_url)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            role TEXT NOT NULL DEFAULT 'User' CHECK(role IN ('User', 'Premium', 'Admin'))
        );
    ",
    )
    .execute(&db)
    .await
    .map_err(|e| format!("DB Error Users: {}", e))?;

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
    .map_err(|e| e.to_string())?;

    let job_statuses = Arc::new(DashMap::new());

    let (tx_premium, mut rx_premium) = mpsc::channel::<SimulationJob>(100);
    let (tx_standard, mut rx_standard) = mpsc::channel::<SimulationJob>(100);
    let queue_tracker = Arc::new(Mutex::new(Vec::new()));
    let client_id = env::var("BLIZZARD_CLIENT_ID").expect("Error BLIZZARD_CLIENT_ID");
    let client_secret = env::var("BLIZZARD_CLIENT_SECRET").expect("Error BLIZZARD_CLIENT_SECRET");
    let icon_cache = Arc::new(DashMap::new());

    let session_store = SqliteStore::new(db.clone());
    session_store.migrate().await.map_err(|e| e.to_string())?;

    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_expiry(Expiry::OnInactivity(SessionDuration::seconds(3600)));

    let state = utils::AppState {
        premium_queue: tx_premium,
        standard_queue: tx_standard,
        queue_tracker: queue_tracker.clone(),
        job_statuses: job_statuses.clone(),
        http_client: reqwest::Client::new(),
        blizzard_auth: Arc::new(Mutex::new(utils::BlizzardAuth {
            client_id,
            client_secret,
            access_token: None,
            expires_at: Instant::now(),
        })),
        item_icon_cache: icon_cache,
        db: db.clone(),
    };

    tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(MAX_RUNNING_SIMS));
        tracing::info!("Worker loop started with queue size: {}", MAX_RUNNING_SIMS);
        loop {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let statuses = job_statuses.clone();
            let db_conn = db.clone();
            let tracker_clone = queue_tracker.clone();

            let job = tokio::select! {
                biased;
                Some(job) = rx_premium.recv() => Some(job),
                Some(job) = rx_standard.recv() => Some(job),
                else => None,
            };

            if let Some(job) = job {
                {
                    let mut tracker = tracker_clone.lock().await;
                    if let Some(pos) = tracker.iter().position(|x| x.id == job.id) {
                        tracker.remove(pos);
                    }
                }
                tracing::info!(
                    "Starting simulation job: {} (Type: {:?}, User: {:?})",
                    job.id,
                    job.job_type,
                    job.user_id
                );
                tokio::spawn(async move {
                    statuses.insert(job.id.clone(), SimStatus::Processing);
                    let job_id = job.id.clone();
                    let output_path = format!("files/{}.json", job_id);
                    let output_path_clone = output_path.clone();
                    let statuses_clone = statuses.clone();

                    let result = tokio::task::spawn_blocking(move || match job.job_type {
                        JobType::QuickSim { base_simc } => {
                            let simc_path = format!("files/{}.simc", job_id);
                            let _ = fs::write(
                                &simc_path,
                                format!("{}\nmax_time=300\niterations=1000\n", base_simc),
                            );
                            let res = simulate::run_simc(&simc_path, &output_path_clone);
                            let _ = fs::remove_file(simc_path);
                            res.map(|_| "QuickSim".to_string())
                        }
                        JobType::TopGear {
                            base_simc,
                            selected_items,
                        } => process_topgear_batch(
                            &job_id,
                            &base_simc,
                            selected_items,
                            &output_path_clone,
                            statuses_clone,
                        )
                        .map(|_| "TopGear".to_string()),
                    })
                    .await;
                    match result {
                        Ok(Ok(sim_type)) => {
                            if let Ok(json_content) = fs::read_to_string(&output_path) {
                                let dps = extract_dps_from_json(&json_content, &sim_type);
                                if let Err(e) = sqlx::query("INSERT INTO history (id, user_id, sim_type, dps, result_json) VALUES (?, ?, ?, ?, ?)")
                                    .bind(&job.id)
                                    .bind(job.user_id)
                                    .bind(&sim_type)
                                    .bind(dps)
                                    .bind(&json_content)
                                    .execute(&db_conn)
                                    .await {
                                        tracing::error!("DB Insert Error for job {}: {}", job.id, e);
                                    }

                                statuses.insert(job.id.clone(), SimStatus::Finished);
                                tracing::info!(
                                    "Job finished successfully: {} (SimType: {})",
                                    job.id,
                                    sim_type
                                );
                                let _ = fs::remove_file(&output_path);
                            } else {
                                statuses.insert(
                                    job.id.clone(),
                                    SimStatus::Failed("Output missing".into()),
                                );
                                tracing::error!(
                                    "Output missing for job {}: path {}",
                                    job.id,
                                    output_path
                                );
                            }
                        }
                        Ok(Err(e)) => {
                            statuses.insert(
                                job.id.clone(),
                                SimStatus::Failed(format!("Sim Error: {}", e)),
                            );
                            tracing::error!("Sim Error for ID {}: {}", job.id, e);
                        }
                        Err(e) => {
                            statuses.insert(
                                job.id.clone(),
                                SimStatus::Failed(format!("Task Error: {}", e)),
                            );
                            tracing::error!("Task Error for ID {}: {}", job.id, e);
                        }
                    }
                    drop(permit);
                });
            } else {
                break;
            }
        }
    });

    let app = create_router(state, session_layer);

    // let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("Server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Error binding: {}", e))?;

    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| format!("Error server: {}", e))
}

pub fn create_router(
    state: utils::AppState,
    session_layer: SessionManagerLayer<SqliteStore>,
) -> Router {
    Router::new()
        .route("/", get(get_index))
        // QUICKSIM
        .route("/quicksim/run", post(post_simulation))
        .route("/quicksim/{id}", get(get_quicksim))
        .route("/quicksim/{id}/data", get(get_result_data_from_db))
        // TOPGEAR
        .route("/topgear", post(post_topgear))
        .route("/topgear/run", post(post_run_topgear_batch))
        .route("/topgear/{id}", get(get_topgear_result_page))
        .route("/topgear/{id}/data", get(get_result_data_from_db))
        // API
        .route("/api/status/{id}", get(get_status_check))
        .route("/api/wowhead_tooltip", get(get_wowhead_tooltip))
        .route("/api/item_icon/{id}", get(get_item_icon_only))
        .route("/api/item_tooltip/{id}", get(get_item_tooltip))
        .route("/api/user/limits", get(users::get_user_limits))
        // USER
        .route(
            "/user/register",
            get(users::register_page).post(users::register),
        )
        .route("/user/login", post(users::login))
        .route("/user/logout", get(users::logout))
        .route("/user/profile/{username}", get(users::get_user_profile))
        // ADMIN
        .route("/admin", get(admin::admin_dashboard))
        .route("/admin/role", post(admin::update_role))
        .route("/admin/delete", post(admin::delete_user))
        .route("/admin/password", post(admin::reset_password))
        // Config Stuff
        .nest_service("/assets", ServeDir::new("frontend/assets"))
        .fallback(handler_404)
        .layer(session_layer)
        .with_state(state)
}

async fn get_index(session: Session) -> Html<String> {
    let template = fs::read_to_string("frontend/index.html").unwrap_or_default();
    let header_html = generate_header(&session).await;
    let html = template.replace("{{HEADER}}", &header_html);
    Html(html)
}

async fn handler_404(uri: axum::http::Uri) -> (StatusCode, Html<String>) {
    tracing::warn!("404 Not Found: {}", uri.path());
    match fs::read_to_string("frontend/404.html") {
        Ok(content) => (StatusCode::NOT_FOUND, Html(content)),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Html("<h1>404 Not Found</h1>".to_string()),
        ),
    }
}

/* TESTS */
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    #[tokio::test]
    async fn test_handler_404() {
        let uri = "/fart".parse::<Uri>().unwrap();
        let (status, body) = handler_404(uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        assert!(body.0.contains("404"));
    }
}
