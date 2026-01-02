use axum::{
    Router,
    extract::{Form, Path, Query, State},
    response::{Html, Json, Redirect},
    routing::{get, post},
};
use dashmap::DashMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    collections::HashMap,
    env, fs,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tower_http::services::ServeDir;
use uuid::Uuid;
use futures::future::join_all;

use crate::simulate;

#[derive(Clone, Debug, Serialize, PartialEq)]
enum SimStatus {
    Queued,
    Processing,
    Finished,
    Failed(String),
}

#[derive(Clone)]
struct AppState {
    queue_sender: mpsc::Sender<SimulationJob>,
    job_statuses: Arc<DashMap<String, SimStatus>>,
    http_client: reqwest::Client,
    blizzard_auth: Arc<Mutex<BlizzardAuth>>,
}

struct SimulationJob {
    id: String,
    simc_file_path: String,
    output_file_path: String,
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

#[derive(Serialize)]
struct ItemDetailsResponse {
    icon_url: String,
    stats_html: String,
    weapon_type: String,
    gems: Vec<GemDetail>,
}

const MAX_RUNNING_SIMS: usize = 2;

pub async fn run_server() -> Result<(), String> {
    dotenvy::dotenv().ok();
    let job_statuses = Arc::new(DashMap::new());
    let (tx, mut rx) = mpsc::channel::<SimulationJob>(100);

    let client_id = env::var("BLIZZARD_CLIENT_ID").expect("Error BLIZZARD_CLIENT_ID");

    let client_secret = env::var("BLIZZARD_CLIENT_SECRET").expect("Error BLIZZARD_CLIENT_SECRET");

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
    };

    tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(MAX_RUNNING_SIMS));

        while let Some(job) = rx.recv().await {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let statuses = job_statuses.clone();

            tokio::spawn(async move {
                statuses.insert(job.id.clone(), SimStatus::Processing);
                println!("Starting simulation: {}", job.id);

                let simc_path = job.simc_file_path.clone();
                let output_path = job.output_file_path.clone();

                let result = tokio::task::spawn_blocking(move || {
                    simulate::run_simc(&simc_path, &output_path)
                })
                .await;

                match result {
                    Ok(Ok(_)) => {
                        statuses.insert(job.id.clone(), SimStatus::Finished);
                        println!("Finished simulation: {}", job.id);
                    }
                    Ok(Err(e)) => {
                        let err_msg = format!("Simulation Error: {}", e);
                        eprintln!("{}", err_msg);
                        statuses.insert(job.id.clone(), SimStatus::Failed(err_msg));
                    }
                    Err(e) => {
                        let err_msg = format!("Task Join Error: {}", e);
                        eprintln!("{}", err_msg);
                        statuses.insert(job.id.clone(), SimStatus::Failed(err_msg));
                    }
                }
                let _ = fs::remove_file(&job.simc_file_path);
                drop(permit);
            });
        }
    });

    let app = Router::new()
        .route("/run_simulation", post(post_simulation))
        .route("/quicksim/{id}", get(get_quicksim))
        .route("/quicksim/{id}/status", get(get_quicksim_status))
        .route("/quicksim/{id}/result", get(get_quicksim_result))
        .route("/topgear", post(post_topgear))
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

    let simc_file = format!("files/{}.simc", id);
    let output_file = format!("files/{}.json", id);

    if let Err(e) = fs::write(
        &simc_file,
        format!("{}\nmax_time=300\niterations=1000\n", input.input_content),
    ) {
        eprintln!("Error while writing: {}", e);
        return Redirect::to("/error");
    }

    state.job_statuses.insert(id.clone(), SimStatus::Queued);

    let job = SimulationJob {
        id: id.clone(),
        simc_file_path: simc_file,
        output_file_path: output_file,
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
    match fs::read_to_string("frontend/result.html") {
        Ok(template) => {
            let html = template.replace("{{ID}}", &id);
            Html(html)
        }
        Err(_) => Html("<h1>Error: Template not found</h1>".to_string()),
    }
}

async fn get_quicksim_status(State(state): State<AppState>, Path(id): Path<String>) -> Json<Value> {
    if let Some(status) = state.job_statuses.get(&id) {
        match status.value() {
            SimStatus::Queued => Json(serde_json::json!({ "status": "Queued" })),
            SimStatus::Processing => Json(serde_json::json!({ "status": "Processing" })),
            SimStatus::Finished => Json(serde_json::json!({ "status": "Finished" })),
            SimStatus::Failed(e) => Json(serde_json::json!({ "status": "Failed", "error": e })),
        }
    } else {
        Json(serde_json::json!({ "status": "Unknown" }))
    }
}

async fn get_quicksim_result(Path(id): Path<String>) -> Json<Value> {
    let file = format!("files/{}.json", id);
    if let Ok(data) = fs::read_to_string(&file) {
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            if let Some(dps) = v["sim"]["players"]
                .as_array()
                .and_then(|players| players.get(0))
                .and_then(|player| player["collected_data"]["dps"]["mean"].as_f64())
            {
                return Json(serde_json::json!({ "dps": dps }));
            }
        }
    }
    Json(serde_json::json!({ "error": "Data not found or invalid format" }))
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

async fn get_item_details(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ItemDetailsQuery>,
) -> Json<ItemDetailsResponse> {
    
    let token = match get_blizzard_token(&state).await {
        Ok(t) => t,
        Err(e) => return Json(ItemDetailsResponse {
            icon_url: "".into(),
            stats_html: format!("Error auth: {}", e),
            weapon_type: "".into(),
            gems: vec![],
        }),
    };

    let client = &state.http_client;

    let media_url = format!("https://eu.api.blizzard.com/data/wow/media/item/{}?namespace=static-eu&locale=en_US", id);
    let mut item_url = format!("https://eu.api.blizzard.com/data/wow/item/{}?namespace=static-eu&locale=en_US", id);

    if let Some(bonus_str) = &params.bonus {
        if !bonus_str.is_empty() {
            let api_bonus = bonus_str.replace("/", ",");
            item_url.push_str(&format!("&bonusList={}", api_bonus));
        }
    }

    let media_future = client.get(&media_url).bearer_auth(&token).send();
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

    let (media_res, item_res, gems_res) = tokio::join!(
        media_future,
        item_future,
        gems_future
    );
    
    let mut icon_url = "https://render.worldofwarcraft.com/eu/icons/56/inv_misc_questionmark.jpg".to_string();
    if let Ok(res) = media_res {
        if let Ok(media) = res.json::<BlizzardMediaResponse>().await {
            if let Some(assets) = media.assets {
                if let Some(icon) = assets.iter().find(|a| a.key == "icon") {
                    icon_url = icon.value.clone();
                }
            }
        }
    }

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

    Json(ItemDetailsResponse {
        icon_url,
        stats_html,
        weapon_type,
        gems,
    })
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
                
                let gems_attr = item.gem_ids.join(",");

                grid_html.push_str(&format!(
                    r#"
                    <div class="item-card {}" 
                         data-id="{}" 
                         data-slot="{}" 
                         data-bonus="{}" 
                         data-gems="{}" 
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
    
    Html(template.replace("{{ITEMS_GRID}}", &grid_html))
}
