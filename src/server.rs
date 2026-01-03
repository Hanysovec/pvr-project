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
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tower_http::services::ServeDir;
use uuid::Uuid;
use futures::future::join_all;
use sqlx::{sqlite::SqlitePool, Pool, Sqlite};

use crate::simulate;

#[derive(Clone, Debug, Serialize, PartialEq)]
enum SimStatus {
    Queued,
    Processing,
    TopGearProcessing { current: usize, total: usize }, 
    Finished,
    Failed(String),
}

#[derive(Clone)]
struct AppState {
    queue_sender: mpsc::Sender<SimulationJob>,
    job_statuses: Arc<DashMap<String, SimStatus>>,
    http_client: reqwest::Client,
    blizzard_auth: Arc<Mutex<BlizzardAuth>>,
    item_icon_cache: Arc<DashMap<String, String>>, // OPTIMALIZATION to not reach Blizzard API limit so fast (100x/60secs)
    db: Pool<Sqlite>,
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
        CREATE TABLE IF NOT EXISTS history (
            id TEXT PRIMARY KEY,
            sim_type TEXT NOT NULL,
            dps REAL,
            result_json TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );
    ").execute(&db).await.map_err(|e| e.to_string())?;

    let job_statuses = Arc::new(DashMap::new());
    let (tx, mut rx) = mpsc::channel::<SimulationJob>(100);
    let client_id = env::var("BLIZZARD_CLIENT_ID").expect("Error BLIZZARD_CLIENT_ID");
    let client_secret = env::var("BLIZZARD_CLIENT_SECRET").expect("Error BLIZZARD_CLIENT_SECRET");
    let icon_cache = Arc::new(DashMap::new());

    let state = AppState {
        queue_sender: tx,
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

        while let Some(job) = rx.recv().await {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let statuses = job_statuses.clone();
            let db_conn = db.clone();

            tokio::spawn(async move {
                statuses.insert(job.id.clone(), SimStatus::Processing);
                println!("Starting job: {}", job.id);
                let statuses_clone = statuses.clone();
                let job_id = job.id.clone();
                let output_path = format!("files/{}.json", job_id);
                let output_path_clone = output_path.clone();
                let result = tokio::task::spawn_blocking(move || {
                    match job.job_type {
                        JobType::QuickSim { base_simc } => {
                            let simc_path = format!("files/{}.simc", job_id);
                            fs::write(&simc_path, format!("{}\nmax_time=300\niterations=1000\n", base_simc)).unwrap();
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
                    Ok(Ok(sim_type)) => { // OK
                        if let Ok(json_content) = fs::read_to_string(&output_path) {
                            let dps = extract_dps_from_json(&json_content, &sim_type);
                            let query_res = sqlx::query("INSERT INTO history (id, sim_type, dps, result_json) VALUES (?, ?, ?, ?)")
                                .bind(&job.id)
                                .bind(&sim_type)
                                .bind(dps)
                                .bind(&json_content)
                                .execute(&db_conn)
                                .await;

                            if let Err(e) = query_res {
                                eprintln!("DB Insert Error: {}", e);
                                statuses.insert(job.id.clone(), SimStatus::Failed("Database write error".into()));
                            } else {
                                statuses.insert(job.id.clone(), SimStatus::Finished);
                                println!("Finished & Saved job: {}", job.id);
                                let _ = fs::remove_file(&output_path); 
                            }
                        } else {
                            statuses.insert(job.id.clone(), SimStatus::Failed("Output file missing".into()));
                        }
                    }
                    Ok(Err(e)) => { // SimC Error
                        let err_msg = format!("Simulation Error: {}", e);
                        statuses.insert(job.id.clone(), SimStatus::Failed(err_msg));
                    }
                    Err(e) => { // Task Panic
                        let err_msg = format!("Task Execution Error: {}", e);
                        statuses.insert(job.id.clone(), SimStatus::Failed(err_msg));
                    }
                }
                drop(permit);
            });
        }
    });

    let app = Router::new()
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
        .fallback_service(ServeDir::new("frontend"))
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

async fn post_simulation(
    State(state): State<AppState>,
    Form(input): Form<SimulationInput>,
) -> Redirect {
    let id = Uuid::new_v4().to_string();
    let _ = fs::create_dir_all("files");

    state.job_statuses.insert(id.clone(), SimStatus::Queued);

    let job_type = JobType::QuickSim { 
        base_simc: input.input_content 
    };
    
    let job = SimulationJob {
        id: id.clone(),
        job_type,
    };

    if let Err(e) = state.queue_sender.send(job).await {
        eprintln!("Failed to queue job: {}", e);
        state
            .job_statuses
            .insert(id.clone(), SimStatus::Failed("Queue full or closed".into()));
    }

    Redirect::to(&format!("/quicksim/{}", id))
}

async fn get_quicksim(Path(id): Path<String>) -> Html<String> {
    match fs::read_to_string("frontend/quicksim_result.html") {
        Ok(template) => {
            let html = template.replace("{{ID}}", &id);
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
            SimStatus::Queued => return Json(serde_json::json!({ "status": "Queued" })),
            SimStatus::Processing => return Json(serde_json::json!({ "status": "Processing" })),
            SimStatus::TopGearProcessing { current, total } => return Json(serde_json::json!({ 
                "status": "Processing", "progress_current": current, "progress_total": total 
            })),
            SimStatus::Finished => return Json(serde_json::json!({ "status": "Finished" })),
            SimStatus::Failed(e) => return Json(serde_json::json!({ "status": "Failed", "error": e })),
        }
    }
    let exists = sqlx::query("SELECT id FROM history WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

    if exists.is_some() {
        return Json(serde_json::json!({ "status": "Finished" }));
    }
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

async fn get_topgear_result_page(Path(id): Path<String>) -> Html<String> {
    match fs::read_to_string("frontend/topgear_result.html") {
        Ok(template) => {
            let html = template.replace("{{ID}}", &id);
            Html(html)
        }
        Err(_) => Html("<h1>Error: Template not found</h1>".to_string()),
    }
}

async fn post_topgear(Form(input): Form<SimulationInput>) -> Html<String> {
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

    let template = fs::read_to_string("frontend/topgear.html")
        .unwrap_or_else(|_| "<h1>Error: Template missing</h1>".to_string());
    let html_with_grid = template.replace("{{ITEMS_GRID}}", &grid_html);
    let final_html = html_with_grid.replace(
        "// {{BASE_SIMC_INJECT}}", 
        &format!("const BASE_SIMC = `{}`;", input.input_content)
    );
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
    Json(payload): Json<TopGearRunRequest>,
) -> Redirect {
    let id = Uuid::new_v4().to_string();
    let _ = fs::create_dir_all("files");
    
    state.job_statuses.insert(id.clone(), SimStatus::Queued);

    let job = SimulationJob {
        id: id.clone(),
        job_type: JobType::TopGear { 
            base_simc: payload.base_simc,
            selected_items: payload.selected_items 
        },
    };

    if let Err(e) = state.queue_sender.send(job).await {
        eprintln!("Queue error: {}", e);
    }

    Redirect::to(&format!("/topgear/{}", id))
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