use axum::{
    Router,
    extract::{Form, Path, Query, State},
    response::{Html, Json, Redirect},
    routing::{get, post},
};
use dashmap::DashMap;
use itertools::Itertools;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_sessions::{Expiry, Session, SessionManagerLayer, cookie::time::Duration as SessionDuration};
use tower_sessions_sqlx_store::SqliteStore;
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tower_http::services::ServeDir;
use uuid::Uuid;
use futures::future::join_all;
use sqlx::{sqlite::SqlitePool, Pool, Sqlite};

use crate::{admin, simulate, users::{self, UserRole, LIMIT_COMBINATIONS_FREE, LIMIT_COMBINATIONS_PREMIUM}};

#[derive(Clone, Debug, Serialize, PartialEq)]
enum SimStatus {
    Queued,
    Processing,
    TopGearProcessing { current: usize, total: usize }, 
    Finished,
    Failed(String),
}

#[derive(Debug, Clone)]
struct QueueItem {
    id: String,
    is_premium: bool,
    created_at: SystemTime,
}

#[derive(Clone)]
pub struct AppState {
    premium_queue: mpsc::Sender<SimulationJob>,
    standard_queue: mpsc::Sender<SimulationJob>,
    queue_tracker: Arc<Mutex<Vec<QueueItem>>>,
    job_statuses: Arc<DashMap<String, SimStatus>>,
    http_client: reqwest::Client,
    blizzard_auth: Arc<Mutex<BlizzardAuth>>,
    item_icon_cache: Arc<DashMap<String, String>>, // OPTIMALIZATION to not reach Blizzard API limit so fast (100x/60secs)
    pub db: Pool<Sqlite>,
}
enum JobType {
    QuickSim { base_simc: String },
    TopGear { 
        base_simc: String, 
        selected_items: HashMap<String, Vec<String>> 
    },
}
struct SimulationJob {
    id: String,
    job_type: JobType,
    user_id: Option<i64>,
}

#[derive(Deserialize)]
struct SimulationInput {
    input_content: String,
}

#[derive(Debug, Clone)]
struct GearItem {
    name: String,
    id: String,
    ilvl: String,
    slot: String,
    bonus_ids: String,
    gem_ids: Vec<String>,
    is_equipped: bool,
    raw_line: String,
}

#[derive(Clone)]
struct BlizzardAuth {
    client_id: String,
    client_secret: String,
    access_token: Option<String>,
    expires_at: Instant,
}
#[derive(Deserialize, Debug)]
struct BlizzardMediaResponse {
    assets: Option<Vec<BlizzardAsset>>,
}

#[derive(Deserialize, Debug)]
struct BlizzardAsset {
    key: String,
    value: String,
}

#[derive(Deserialize, Debug)]
struct BlizzardTokenResponse {
    access_token: String,
    expires_in: u64,
}
#[derive(Deserialize, Debug)]
struct BlizzardItemDataResponse {
    inventory_type: BlizzardInventoryType,
    preview_item: BlizzardPreviewItem,
}

#[derive(Deserialize, Debug)]
struct BlizzardInventoryType {
    name: String,
}

#[derive(Deserialize, Debug)]
struct BlizzardPreviewItem {
    stats: Option<Vec<BlizzardStat>>,
}

#[derive(Deserialize, Debug)]
struct BlizzardStat {
    #[serde(rename = "type")]
    stat_type: BlizzardStatType,
    value: i32,
    display: Option<BlizzardStatDisplay>,
}

#[derive(Deserialize, Debug)]
struct BlizzardStatType {
    name: String,
}

#[derive(Deserialize, Debug)]
struct BlizzardStatDisplay {
    display_string: String,
}


#[derive(Serialize)]
struct ItemIconResponse {
    icon_url: String,
}

#[derive(Serialize)]
struct ItemTooltipResponse {
    stats_html: String,
    weapon_type: String,
    gems: Vec<GemDetail>,
}

#[derive(Serialize)]
struct GemDetail {
    name: String,
    icon_url: String,
}

#[derive(Deserialize)]
struct ItemDetailsQuery {
    bonus: Option<String>,
    gems: Option<String>,
}

#[derive(Deserialize)]
struct TopGearRunRequest {
    base_simc: String,
    selected_items: HashMap<String, Vec<String>>, 
}

#[derive(Serialize, Clone)]
struct TopGearResultEntry {
    dps: f64,
    items: Vec<String>, 
}

#[derive(Serialize)]
struct TopGearFinalResult {
    results: Vec<TopGearResultEntry>,
}

#[derive(Deserialize)]
struct WowheadQuery {
    item_id: String,
    bonus: Option<String>,
    enchant: Option<String>,
    gems: Option<String>,
    ilvl: Option<String>,
}

const MAX_RUNNING_SIMS: usize = 2;

pub async fn run_server() -> Result<(), String> {
    dotenvy::dotenv().ok();
    let database_url = env::var("DATABASE_URL").unwrap_or("sqlite:sims.db?mode=rwc".to_string());
    let db = SqlitePool::connect(&database_url).await.map_err(|e| e.to_string())?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            role TEXT NOT NULL DEFAULT 'User' CHECK(role IN ('User', 'Premium', 'Admin'))
        );
    ").execute(&db).await.map_err(|e| format!("DB Error Users: {}", e))?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS history (
            id TEXT PRIMARY KEY,
            user_id INTEGER,
            sim_type TEXT NOT NULL,
            dps REAL,
            result_json TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(user_id) REFERENCES users(id)
        );
    ").execute(&db).await.map_err(|e| e.to_string())?;

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

    let state = AppState {
        premium_queue: tx_premium,
        standard_queue: tx_standard,
        queue_tracker: queue_tracker.clone(),
        job_statuses: job_statuses.clone(),
        http_client: reqwest::Client::new(),
        blizzard_auth: Arc::new(Mutex::new(BlizzardAuth {
            client_id,
            client_secret,
            access_token: None,
            expires_at: Instant::now(),
        })),
        item_icon_cache: icon_cache,
        db: db.clone()
    };

    tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(MAX_RUNNING_SIMS));

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
                tokio::spawn(async move {
                    statuses.insert(job.id.clone(), SimStatus::Processing);
                    let job_id = job.id.clone();
                    let output_path = format!("files/{}.json", job_id);
                    let output_path_clone = output_path.clone();
                    let statuses_clone = statuses.clone();

                    let result = tokio::task::spawn_blocking(move || {
                        match job.job_type {
                            JobType::QuickSim { base_simc } => {
                                let simc_path = format!("files/{}.simc", job_id);
                                let _ = fs::write(&simc_path, format!("{}\nmax_time=300\niterations=1000\n", base_simc));
                                let res = simulate::run_simc(&simc_path, &output_path_clone);
                                let _ = fs::remove_file(simc_path);
                                res.map(|_| "QuickSim".to_string())
                            },
                            JobType::TopGear { base_simc, selected_items } => {
                                process_topgear_batch(
                                    &job_id, 
                                    &base_simc, 
                                    selected_items, 
                                    &output_path_clone, 
                                    statuses_clone
                                ).map(|_| "TopGear".to_string())
                            }
                        }
                    }).await;
                    match result {
                        Ok(Ok(sim_type)) => {
                            if let Ok(json_content) = fs::read_to_string(&output_path) {
                                let dps = extract_dps_from_json(&json_content, &sim_type);
                                let _ = sqlx::query("INSERT INTO history (id, user_id, sim_type, dps, result_json) VALUES (?, ?, ?, ?, ?)")
                                    .bind(&job.id)
                                    .bind(job.user_id)
                                    .bind(&sim_type)
                                    .bind(dps)
                                    .bind(&json_content)
                                    .execute(&db_conn)
                                    .await;
                                
                                statuses.insert(job.id.clone(), SimStatus::Finished);
                                println!("Finished job: {}", job.id);
                                let _ = fs::remove_file(&output_path); 
                            } else {
                                statuses.insert(job.id.clone(), SimStatus::Failed("Output missing".into()));
                            }
                        }
                        Ok(Err(e)) => { statuses.insert(job.id.clone(), SimStatus::Failed(format!("Sim Error: {}", e))); }
                        Err(e) => { statuses.insert(job.id.clone(), SimStatus::Failed(format!("Task Error: {}", e))); }
                    }
                    drop(permit);
                });
            } else {
                break;
            }
        }
    });

    let app = Router::new()
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
        .route("/user/register", get(users::register_page).post(users::register))
        .route("/user/login", post(users::login))
        .route("/user/logout", get(users::logout))
        .route("/user/profile/{username}", get(users::get_user_profile))
        // ADMIN
        .route("/admin", get(admin::admin_dashboard))
        .route("/admin/role", post(admin::update_role))
        .route("/admin/delete", post(admin::delete_user))
        .route("/admin/password", post(admin::reset_password))
        .fallback_service(ServeDir::new("frontend"))
        .layer(session_layer)
        .with_state(state);

    // let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Error binding: {}", e))?;

    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| format!("Error server: {}", e))
}

async fn generate_header(session: &Session) -> String {
    let username: Option<String> = session.get("username").await.unwrap_or(None);
    
    if let Some(user) = username {
        format!(
            r#"
            <div class="user-menu">
                <button class="dropbtn" style="background-color: #ffcc00; color: black;">{} ▼</button>
                <div class="dropdown-content">
                    <a href="/user/profile/{}" style="color:#ffcc00; border-bottom:1px solid #444;">My Profile</a>
                    <a href="/user/logout" style="color:#ff5555;">Log Out</a>
                </div>
            </div>
            "#,
            user, user
        )
    } else {
        r#"
        <div class="user-menu">
            <button class="dropbtn">Log In ▼</button>
            <div class="dropdown-content" style="padding: 10px;">
                <form action="/user/login" method="post">
                    <input type="text" name="username" placeholder="Username" required style="width: 90%; margin-bottom: 5px; padding: 5px; background: #111; border: 1px solid #555; color: white;">
                    <input type="password" name="password" placeholder="Password" required style="width: 90%; margin-bottom: 5px; padding: 5px; background: #111; border: 1px solid #555; color: white;">
                    <button type="submit" style="width: 100%; cursor: pointer; background: #4CAF50; border: none; color: white; padding: 5px;">Log In</button>
                </form>
                <div style="margin-top:5px; text-align:center;">
                    <a href="/user/register" style="font-size: 0.8em; padding: 5px;">Register</a>
                </div>
            </div>
        </div>
        "#.to_string()
    }
}

async fn get_index(session: Session) -> Html<String> {
    let template = fs::read_to_string("frontend/index.html").unwrap_or_default();
    let header_html = generate_header(&session).await;
    let html = template.replace("{{HEADER}}", &header_html);
    Html(html)
}

async fn post_simulation(
    State(state): State<AppState>,
    session: Session,
    Form(input): Form<SimulationInput>,
) -> Redirect {
    let id = Uuid::new_v4().to_string();
    let _ = fs::create_dir_all("files");

    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    let role_str: String = session.get("role").await.unwrap_or(Some("User".to_string())).unwrap_or("User".to_string());
    let role = UserRole::from(role_str);
    let is_premium = role >= UserRole::Premium;

    state.job_statuses.insert(id.clone(), SimStatus::Queued);

    {
        let mut tracker = state.queue_tracker.lock().await;
        tracker.push(QueueItem {
            id: id.clone(),
            is_premium,
            created_at: SystemTime::now(),
        });
    }

    let job = SimulationJob {
        id: id.clone(),
        job_type: JobType::QuickSim { base_simc: input.input_content },
        user_id,
    };

    let queue = if is_premium { &state.premium_queue } else { &state.standard_queue };
    if let Err(e) = queue.send(job).await {
        eprintln!("Failed to queue job: {}", e);
        state.job_statuses.insert(id.clone(), SimStatus::Failed("Queue full".into()));
    }

    Redirect::to(&format!("/quicksim/{}", id))
}

async fn get_quicksim(
    session: Session, 
    Path(id): Path<String>
) -> Html<String> {
    match fs::read_to_string("frontend/quicksim_result.html") {
        Ok(template) => {
            let header_html = generate_header(&session).await;
            let html = template
                .replace("{{ID}}", &id)
                .replace("{{HEADER}}", &header_html);
            
            Html(html)
        }
        Err(_) => Html("<h1>Error: Template not found</h1>".to_string()),
    }
}

fn extract_dps_from_json(json_str: &str, sim_type: &str) -> Option<f64> {
    let v: Value = serde_json::from_str(json_str).ok()?;
    if sim_type == "TopGear" {
        return v.get("results")?.get(0)?.get("dps")?.as_f64();
    } else {
        return v.get("sim")?
            .get("players")?.get(0)?
            .get("collected_data")?.get("dps")?
            .get("mean")?.as_f64();
    }
}

async fn get_status_check(
    State(state): State<AppState>, 
    Path(id): Path<String>
) -> Json<Value> {
    if let Some(status) = state.job_statuses.get(&id) {
        match status.value() {
            SimStatus::Queued => {
                let tracker = state.queue_tracker.lock().await;
                if let Some(my_item) = tracker.iter().find(|x| x.id == id) {
                    let total_in_queue = tracker.len();
                    
                    let position = tracker.iter().filter(|other| {
                        if my_item.is_premium {
                            other.is_premium && other.created_at < my_item.created_at
                        } else {
                            if other.is_premium {
                                true
                            } else {
                                other.created_at < my_item.created_at
                            }
                        }
                    }).count() + 1;

                    return Json(serde_json::json!({
                        "status": "Queued",
                        "queue_position": position,
                        "queue_total": total_in_queue
                    }));
                } else {
                    return Json(serde_json::json!({ "status": "Queued", "queue_position": 1, "queue_total": 1 }));
                }
            },
            SimStatus::Processing => return Json(serde_json::json!({ "status": "Processing" })),
            SimStatus::TopGearProcessing { current, total } => return Json(serde_json::json!({ 
                "status": "Processing", "progress_current": current, "progress_total": total 
            })),
            SimStatus::Finished => return Json(serde_json::json!({ "status": "Finished" })),
            SimStatus::Failed(e) => return Json(serde_json::json!({ "status": "Failed", "error": e })),
        }
    }
    
    let exists = sqlx::query("SELECT id FROM history WHERE id = ?").bind(&id).fetch_optional(&state.db).await.unwrap_or(None);
    if exists.is_some() { return Json(serde_json::json!({ "status": "Finished" })); }
    Json(serde_json::json!({ "status": "Unknown" }))
}

async fn get_result_data_from_db(
    State(state): State<AppState>,
    Path(id): Path<String>
) -> Json<Value> {
    let row: Option<(String,)> = sqlx::query_as("SELECT result_json FROM history WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

    if let Some((json_str,)) = row {
        if let Ok(val) = serde_json::from_str::<Value>(&json_str) {
            return Json(val);
        }
    }

    Json(serde_json::json!({ "error": "Result not found in database" }))
}

fn parse_simc_input(input: &str) -> BTreeMap<String, Vec<GearItem>> {
    let mut items_by_slot: BTreeMap<String, Vec<GearItem>> = BTreeMap::new();

    // Used to catch equipped item name
    // # Augur's Ephemeral Wide-Brim (704)
    let re_name_ilvl = Regex::new(r"^#\s+(.*?)\s+\((\d+)\)\s*$").unwrap();

    // Used to catch equipped item stats
    // head=,id=237718,gem_id=...
    let re_equipped = Regex::new(r"^([a-z0-9_]+)=,id=(\d+)(.*)$").unwrap();

    // Used to catch bagged item stats
    // # head=,id=223294,bonus_id=...
    let re_bag = Regex::new(r"^#\s+([a-z0-9_]+)=,id=(\d+)(.*)$").unwrap();

    let lines: Vec<&str> = input.lines().collect();

    for i in 1..lines.len() {
        let current_line = lines[i].trim();
        let prev_line = lines[i - 1].trim();

        let mut item: Option<GearItem> = None;
        
        let parse_params = |rest: &str| -> (String, Vec<String>) {
            let mut bonus = String::new();
            let mut gems = Vec::new();

            for part in rest.split(',') {
                let part = part.trim();
                if part.starts_with("bonus_id=") {
                    bonus = part.replace("bonus_id=", "");
                } else if part.starts_with("gem_id=") {
                    let gem_str = part.replace("gem_id=", "");
                    gems = gem_str.split('/').map(|s| s.to_string()).collect();
                }
            }
            (bonus, gems)
        };

        if let Some(caps) = re_equipped.captures(current_line) {
            if let Some(name_caps) = re_name_ilvl.captures(prev_line) {
                let (bonus, gems) = parse_params(&caps[3]);
                item = Some(GearItem {
                    name: name_caps[1].to_string(),
                    ilvl: name_caps[2].to_string(),
                    slot: caps[1].to_string(),
                    id: caps[2].to_string(),
                    bonus_ids: bonus,
                    gem_ids: gems,
                    is_equipped: true,
                    raw_line: current_line.to_string(),
                });
            }
        } else if let Some(caps) = re_bag.captures(current_line) {
            if let Some(name_caps) = re_name_ilvl.captures(prev_line) {
                let (bonus, gems) = parse_params(&caps[3]);
                item = Some(GearItem {
                    name: name_caps[1].to_string(),
                    ilvl: name_caps[2].to_string(),
                    slot: caps[1].to_string(),
                    id: caps[2].to_string(),
                    bonus_ids: bonus,
                    gem_ids: gems,
                    is_equipped: false,
                    raw_line: current_line.to_string(),
                });
            }
        }

        if let Some(final_item) = item {
             let display_slot = match final_item.slot.as_str() {
                "finger1" | "finger2" => "Finger".to_string(),
                "trinket1" | "trinket2" => "Trinket".to_string(),
                "main_hand" => "Main Hand".to_string(),
                "off_hand" => "Off Hand".to_string(),
                s => {
                    let mut c = s.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                }
            };
            items_by_slot.entry(display_slot).or_insert_with(Vec::new).push(final_item);
        }
    }
    items_by_slot
}

async fn get_blizzard_token(state: &AppState) -> Result<String, String> {
    let mut auth = state.blizzard_auth.lock().await;

    if let Some(token) = &auth.access_token {
        if Instant::now() < auth.expires_at {
            return Ok(token.clone());
        }
    }

    println!("fetching new token..");
    let params = [("grant_type", "client_credentials")];

    let response = state
        .http_client
        .post("https://oauth.battle.net/token")
        .basic_auth(&auth.client_id, Some(&auth.client_secret))
        .form(&params)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("Failed to auth: {}", response.status()));
    }

    let token_data: BlizzardTokenResponse = response.json().await.map_err(|e| e.to_string())?;

    auth.access_token = Some(token_data.access_token.clone());
    auth.expires_at = Instant::now() + Duration::from_secs(token_data.expires_in - 60);

    Ok(token_data.access_token)
}

async fn get_gem_detail(client: &reqwest::Client, token: &str, gem_id: &str) -> Option<GemDetail> {
    let item_url = format!("https://eu.api.blizzard.com/data/wow/item/{}?namespace=static-eu&locale=en_US", gem_id);
    let media_url = format!("https://eu.api.blizzard.com/data/wow/media/item/{}?namespace=static-eu&locale=en_US", gem_id);

    let (item_res, media_res) = tokio::join!(
        client.get(&item_url).bearer_auth(token).send(),
        client.get(&media_url).bearer_auth(token).send()
    );

    let mut name = "Unknown Gem".to_string();
    let mut icon_url = "".to_string();

    if let Ok(res) = item_res {
        #[derive(Deserialize)] struct SimpleItem { name: String }
        if let Ok(data) = res.json::<SimpleItem>().await {
            name = data.name;
        }
    }

    if let Ok(res) = media_res {
        if let Ok(media) = res.json::<BlizzardMediaResponse>().await {
            if let Some(assets) = media.assets {
                if let Some(icon) = assets.iter().find(|a| a.key == "icon") {
                    icon_url = icon.value.clone();
                }
            }
        }
    }

    Some(GemDetail { name, icon_url })
}

async fn get_item_icon_only(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<ItemIconResponse> {
    if let Some(cached_url) = state.item_icon_cache.get(&id) {
        return Json(ItemIconResponse { icon_url: cached_url.value().clone() });
    }
    let token = match get_blizzard_token(&state).await {
        Ok(t) => t,
        Err(_) => return Json(ItemIconResponse { icon_url: "".into() }),
    };

    let media_url = format!("https://eu.api.blizzard.com/data/wow/media/item/{}?namespace=static-eu&locale=en_US", id);
    let mut icon_url = "https://render.worldofwarcraft.com/eu/icons/56/inv_misc_questionmark.jpg".to_string();

    if let Ok(res) = state.http_client.get(&media_url).bearer_auth(&token).send().await {
        if let Ok(media) = res.json::<BlizzardMediaResponse>().await {
            if let Some(assets) = media.assets {
                if let Some(icon) = assets.iter().find(|a| a.key == "icon") {
                    icon_url = icon.value.clone();
                }
            }
        }
    }
    state.item_icon_cache.insert(id, icon_url.clone());
    Json(ItemIconResponse { icon_url })
}

async fn get_item_tooltip(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ItemDetailsQuery>,
) -> Json<ItemTooltipResponse> {
    let token = match get_blizzard_token(&state).await {
        Ok(t) => t,
        Err(e) => return Json(ItemTooltipResponse {
            stats_html: format!("Error auth: {}", e),
            weapon_type: "".into(),
            gems: vec![],
        }),
    };

    let client = &state.http_client;
    let mut item_url = format!("https://eu.api.blizzard.com/data/wow/item/{}?namespace=static-eu&locale=en_US", id);

    if let Some(bonus_str) = &params.bonus {
        if !bonus_str.is_empty() {
            let api_bonus = bonus_str.replace("/", ",");
            item_url.push_str(&format!("&bonusList={}", api_bonus));
        }
    }

    let item_future = client.get(&item_url).bearer_auth(&token).send();

    let gems_future = async {
        if let Some(gems_str) = &params.gems {
            if !gems_str.is_empty() {
                let futures = gems_str.split(',')
                    .filter(|s| !s.is_empty())
                    .map(|gem_id| get_gem_detail(client, &token, gem_id));
                return join_all(futures).await;
            }
        }
        vec![]
    };

    let (item_res, gems_res) = tokio::join!(
        item_future,
        gems_future
    );

    let mut stats_html = String::new();
    let mut weapon_type = String::new();

    if let Ok(res) = item_res {
        if let Ok(data) = res.json::<BlizzardItemDataResponse>().await {
            let type_name = data.inventory_type.name.as_str();
            weapon_type = match type_name {
                "Two-Hand" | "Ranged" | "Polearms" | "Staves" => "2H".to_string(),
                "One-Hand" | "Main Hand" | "Daggers" | "Maces" | "Axes" | "Swords" | "Warglaives" => "1H".to_string(),
                "Off Hand" | "Shields" | "Held In Off-hand" => "OH".to_string(),
                _ => "".to_string(),
            };

            if let Some(stats) = data.preview_item.stats {
                for stat in stats {
                    let text = if let Some(d) = stat.display { d.display_string } else { format!("+{} {}", stat.value, stat.stat_type.name) };
                    stats_html.push_str(&format!("<div class='stat-line'>{}</div>", text));
                }
            }
        }
    }

    let gems: Vec<GemDetail> = gems_res.into_iter().filter_map(|g| g).collect();

    Json(ItemTooltipResponse {
        stats_html,
        weapon_type,
        gems,
    })
}

async fn get_topgear_result_page(
    session: Session, 
    Path(id): Path<String>
) -> Html<String> {
    match fs::read_to_string("frontend/topgear_result.html") {
        Ok(template) => {
            let header_html = generate_header(&session).await;
            let html = template
                .replace("{{ID}}", &id)
                .replace("{{HEADER}}", &header_html);
            
            Html(html)
        }
        Err(_) => Html("<h1>Error: Template not found</h1>".to_string()),
    }
}

async fn post_topgear(
    session: Session, 
    Form(input): Form<SimulationInput>
) -> Html<String> {
    let items_map = parse_simc_input(&input.input_content);
    let mut grid_html = String::new();
    let sorted_keys = vec![
        "Head", "Neck", "Shoulder", "Back", "Chest", "Wrist", 
        "Hands", "Waist", "Legs", "Feet", "Finger", "Trinket", 
        "Main Hand", "Off Hand"
    ];
    

    for key in sorted_keys {
        if let Some(items) = items_map.get(key) {
            grid_html.push_str(&format!(r#"<div class="slot-group"><h3>{}</h3><div class="items-container">"#, key));
            
            for item in items {
                let selected_class = if item.is_equipped { "selected" } else { "" };
                let raw_line_attr = item.raw_line.replace("\"", "&quot;");
                let gems_attr = item.gem_ids.join(",");

                grid_html.push_str(&format!(
                    r#"
                    <div class="item-card {}"
                         data-id="{}"
                         data-slot="{}"
                         data-bonus="{}"
                         data-gems="{}"
                         data-simc-line="{}"
                         onclick="toggleItem(this)">
                         
                        <div class="item-header">
                            <img class="item-icon"
                                 src="https://render.worldofwarcraft.com/eu/icons/56/inv_misc_questionmark.jpg"
                                 alt="" 
                                 data-id="{}">
                            <div class="item-info">
                                <div class="item-name">{}</div>
                                <div style="display:flex; align-items:center; width:100%">
                                    <div class="item-ilvl">ilvl: {}</div>
                                    <div class="weapon-tag" style="display:none"></div>
                                </div>
                            </div>
                        </div>
                        
                        <div class="tooltip">Loading stats...</div>
                        <div class="item-id" style="display:none">ID: {}</div>
                    </div>
                    "#,
                    selected_class, 
                    item.id, 
                    item.slot,
                    item.bonus_ids,
                    gems_attr,
                    raw_line_attr,
                    item.id,
                    item.name, 
                    item.ilvl, 
                    item.id
                ));
            }
            grid_html.push_str("</div></div>");
        }
    }

    let template = fs::read_to_string("frontend/topgear.html").unwrap_or_default();
    let header_html = generate_header(&session).await;
    let final_html = template
        .replace("{{ITEMS_GRID}}", &grid_html)
        .replace("{{HEADER}}", &header_html)
        .replace("// {{BASE_SIMC_INJECT}}", &format!("const BASE_SIMC = `{}`;", input.input_content));
        
    Html(final_html)
}

fn generate_combinations(items_map: HashMap<String, Vec<String>>) -> Vec<Vec<String>> {
    let mut all_slots_variants: Vec<Vec<Vec<String>>> = Vec::new();

    for (slot_name, lines) in items_map {
        let mut variants_for_this_slot: Vec<Vec<String>> = Vec::new();

        if slot_name == "Finger" || slot_name == "Trinket" {
            if lines.len() >= 2 {
                for pair in lines.into_iter().combinations(2) {
                    variants_for_this_slot.push(pair);
                }
            } else if lines.len() == 1 {
                 variants_for_this_slot.push(vec![lines[0].clone()]);
            }
        } else {
            for item in lines {
                variants_for_this_slot.push(vec![item]);
            }
        }
        
        if !variants_for_this_slot.is_empty() {
            all_slots_variants.push(variants_for_this_slot);
        }
    }
    // 2. Cartesian Product
    // all_slots_variants = [  
    //    [ [Head1], [Head2] ], 
    //    [ [R1, R2], [R1, R3] ] 
    // ]
    let prod = all_slots_variants.into_iter().multi_cartesian_product();
    
    // 1. [ [Head1], [R1, R2] ]
    // 2. [ [Head1], [R1, R3] ]
    // ...
    let mut result = Vec::new();
    for combination in prod {
        let flat: Vec<String> = combination.into_iter().flatten().collect();
        result.push(flat);
    }
    result
}

fn process_topgear_batch(
    id: &str, 
    base_simc: &str, 
    items: HashMap<String, Vec<String>>,
    output_path: &str,
    job_statuses: Arc<DashMap<String, SimStatus>>
) -> std::io::Result<()> {
    
    let combinations = generate_combinations(items);
    let total = combinations.len();
    println!("TopGear {}: Generated {} combinations", id, total);

    job_statuses.insert(id.to_string(), SimStatus::TopGearProcessing { current: 0, total });

    let mut results = Vec::new();

    for (i, combination) in combinations.iter().enumerate() {
        job_statuses.insert(id.to_string(), SimStatus::TopGearProcessing { current: i + 1, total });

        let mut content = base_simc.to_string();
        content.push_str("\n\n# --- Top Gear Combination ---\n");
        for line in combination {
            content.push_str(line);
            content.push('\n');
        }
        content.push_str("max_time=300\niterations=1000\n");

        let temp_simc_path = format!("files/{}_{}.simc", id, i);
        let temp_json_path = format!("files/{}_{}.json", id, i);

        if let Err(e) = fs::write(&temp_simc_path, &content) { 
            eprintln!("Write err: {}", e); 
            continue; 
        }
        
        if let Ok(_) = simulate::run_simc(&temp_simc_path, &temp_json_path) {
             if let Ok(json_str) = fs::read_to_string(&temp_json_path) {
                if let Ok(v) = serde_json::from_str::<Value>(&json_str) {
                    if let Some(dps) = v["sim"]["players"][0]["collected_data"]["dps"]["mean"].as_f64() {
                        results.push(TopGearResultEntry {
                            dps,
                            items: combination.clone(),
                        });
                    }
                }
            }
            let _ = fs::remove_file(temp_json_path);
        }
        let _ = fs::remove_file(temp_simc_path);
    }

    results.sort_by(|a, b| b.dps.partial_cmp(&a.dps).unwrap());

    let final_json = TopGearFinalResult { results };
    let json_str = serde_json::to_string(&final_json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(output_path, json_str)?;
    Ok(())
}

async fn post_run_topgear_batch(
    State(state): State<AppState>,
    session: Session,
    Json(payload): Json<TopGearRunRequest>,
) -> Result<Redirect, String> {
    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    
    let role_str: String = session.get("role").await.unwrap_or(Some("User".to_string())).unwrap_or("User".to_string());
    let role = UserRole::from(role_str);
    let is_premium = role >= UserRole::Premium;

    let combinations = generate_combinations(payload.selected_items.clone());
    let count = combinations.len();
    let limit = if is_premium { LIMIT_COMBINATIONS_PREMIUM } else { LIMIT_COMBINATIONS_FREE };

    if count > limit {
        return Err(format!("Too many combinations ({}). Your limit is {}. Upgrade to Premium!", count, limit));
    }

    let id = Uuid::new_v4().to_string();
    let _ = fs::create_dir_all("files");
    
    state.job_statuses.insert(id.clone(), SimStatus::Queued);

    {
        let mut tracker = state.queue_tracker.lock().await;
        tracker.push(QueueItem {
            id: id.clone(),
            is_premium,
            created_at: SystemTime::now(),
        });
    }

    let job = SimulationJob {
        id: id.clone(),
        job_type: JobType::TopGear { 
            base_simc: payload.base_simc,
            selected_items: payload.selected_items 
        },
        user_id,
    };

    let queue = if is_premium { &state.premium_queue } else { &state.standard_queue };

    if let Err(e) = queue.send(job).await {
        eprintln!("Queue error: {}", e);
        return Err("Queue full".into());
    }

    Ok(Redirect::to(&format!("/topgear/{}", id)))
}

async fn get_wowhead_tooltip(
    State(state): State<AppState>,
    Query(params): Query<WowheadQuery>,
) -> Json<Value> {
    // https://nether.wowhead.com/tooltip/item/
    let mut url = format!("https://nether.wowhead.com/tooltip/item/{}?dataEnv=1&locale=0", params.item_id);

    if let Some(ench) = params.enchant {
        url.push_str(&format!("&ench={}", ench));
    }
    if let Some(bonus) = params.bonus {
        let wowhead_bonus = bonus.replace("/", ":");
        url.push_str(&format!("&bonus={}", wowhead_bonus));
    }
    if let Some(gems) = params.gems {
        let wowhead_gems = gems.replace("/", ":");
        url.push_str(&format!("&gems={}", wowhead_gems));
    }
    if let Some(ilvl) = params.ilvl {
        url.push_str(&format!("&ilvl={}", ilvl));
    }
    match state.http_client.get(&url).send().await {
        Ok(res) => {
            if let Ok(json) = res.json::<Value>().await {
                return Json(json);
            }
        },
        Err(e) => eprintln!("Wowhead proxy error: {}", e),
    }

    Json(serde_json::json!({ "error": "Failed to fetch tooltip" }))
}