use crate::utils::{AppState, SimStatus};
use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Serialize)]
pub struct ItemIconResponse {
    icon_url: String,
}

#[derive(Serialize)]
pub struct ItemTooltipResponse {
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
pub struct ItemDetailsQuery {
    bonus: Option<String>,
    gems: Option<String>,
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

#[derive(Deserialize)]
pub struct WowheadQuery {
    item_id: String,
    bonus: Option<String>,
    enchant: Option<String>,
    gems: Option<String>,
    ilvl: Option<String>,
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
    let item_url = format!(
        "https://eu.api.blizzard.com/data/wow/item/{}?namespace=static-eu&locale=en_US",
        gem_id
    );
    let media_url = format!(
        "https://eu.api.blizzard.com/data/wow/media/item/{}?namespace=static-eu&locale=en_US",
        gem_id
    );

    let (item_res, media_res) = tokio::join!(
        client.get(&item_url).bearer_auth(token).send(),
        client.get(&media_url).bearer_auth(token).send()
    );

    let mut name = "Unknown Gem".to_string();
    let mut icon_url = "".to_string();

    if let Ok(res) = item_res {
        #[derive(Deserialize)]
        struct SimpleItem {
            name: String,
        }
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

pub async fn get_status_check(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    if let Some(status) = state.job_statuses.get(&id) {
        match status.value() {
            SimStatus::Queued => {
                let tracker = state.queue_tracker.lock().await;
                if let Some(my_item) = tracker.iter().find(|x| x.id == id) {
                    let total_in_queue = tracker.len();

                    let position = tracker
                        .iter()
                        .filter(|other| {
                            if my_item.is_premium {
                                other.is_premium && other.created_at < my_item.created_at
                            } else {
                                if other.is_premium {
                                    true
                                } else {
                                    other.created_at < my_item.created_at
                                }
                            }
                        })
                        .count()
                        + 1;

                    return Json(serde_json::json!({
                        "status": "Queued",
                        "queue_position": position,
                        "queue_total": total_in_queue
                    }));
                } else {
                    return Json(
                        serde_json::json!({ "status": "Queued", "queue_position": 1, "queue_total": 1 }),
                    );
                }
            }
            SimStatus::Processing => return Json(serde_json::json!({ "status": "Processing" })),
            SimStatus::TopGearProcessing { current, total } => {
                return Json(serde_json::json!({
                    "status": "Processing", "progress_current": current, "progress_total": total
                }));
            }
            SimStatus::Finished => return Json(serde_json::json!({ "status": "Finished" })),
            SimStatus::Failed(e) => {
                return Json(serde_json::json!({ "status": "Failed", "error": e }));
            }
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

pub async fn get_result_data_from_db(
    State(state): State<AppState>,
    Path(id): Path<String>,
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

pub async fn get_item_icon_only(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<ItemIconResponse> {
    if let Some(cached_url) = state.item_icon_cache.get(&id) {
        return Json(ItemIconResponse {
            icon_url: cached_url.value().clone(),
        });
    }
    let token = match get_blizzard_token(&state).await {
        Ok(t) => t,
        Err(_) => {
            return Json(ItemIconResponse {
                icon_url: "".into(),
            });
        }
    };

    let media_url = format!(
        "https://eu.api.blizzard.com/data/wow/media/item/{}?namespace=static-eu&locale=en_US",
        id
    );
    let mut icon_url =
        "https://render.worldofwarcraft.com/eu/icons/56/inv_misc_questionmark.jpg".to_string();

    if let Ok(res) = state
        .http_client
        .get(&media_url)
        .bearer_auth(&token)
        .send()
        .await
    {
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

pub async fn get_item_tooltip(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ItemDetailsQuery>,
) -> Json<ItemTooltipResponse> {
    let token = match get_blizzard_token(&state).await {
        Ok(t) => t,
        Err(e) => {
            return Json(ItemTooltipResponse {
                stats_html: format!("Error auth: {}", e),
                weapon_type: "".into(),
                gems: vec![],
            });
        }
    };

    let client = &state.http_client;
    let mut item_url = format!(
        "https://eu.api.blizzard.com/data/wow/item/{}?namespace=static-eu&locale=en_US",
        id
    );

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
                let futures = gems_str
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|gem_id| get_gem_detail(client, &token, gem_id));
                return join_all(futures).await;
            }
        }
        vec![]
    };

    let (item_res, gems_res) = tokio::join!(item_future, gems_future);

    let mut stats_html = String::new();
    let mut weapon_type = String::new();

    if let Ok(res) = item_res {
        if let Ok(data) = res.json::<BlizzardItemDataResponse>().await {
            let type_name = data.inventory_type.name.as_str();
            weapon_type = match type_name {
                "Two-Hand" | "Ranged" | "Polearms" | "Staves" => "2H".to_string(),
                "One-Hand" | "Main Hand" | "Daggers" | "Maces" | "Axes" | "Swords"
                | "Warglaives" => "1H".to_string(),
                "Off Hand" | "Shields" | "Held In Off-hand" => "OH".to_string(),
                _ => "".to_string(),
            };

            if let Some(stats) = data.preview_item.stats {
                for stat in stats {
                    let text = if let Some(d) = stat.display {
                        d.display_string
                    } else {
                        format!("+{} {}", stat.value, stat.stat_type.name)
                    };
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

pub async fn get_wowhead_tooltip(
    State(state): State<AppState>,
    Query(params): Query<WowheadQuery>,
) -> Json<Value> {
    // https://nether.wowhead.com/tooltip/item/
    let mut url = format!(
        "https://nether.wowhead.com/tooltip/item/{}?dataEnv=1&locale=0",
        params.item_id
    );

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
        }
        Err(e) => eprintln!("Wowhead proxy error: {}", e),
    }

    Json(serde_json::json!({ "error": "Failed to fetch tooltip" }))
}
