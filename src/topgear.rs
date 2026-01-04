use crate::{
    quicksim::SimulationInput, simulate, users::{LIMIT_COMBINATIONS_FREE, LIMIT_COMBINATIONS_PREMIUM, UserRole}, utils::{AppState, JobType, QueueItem, SimStatus, SimulationJob, generate_header, parse_simc_input}
};
use axum::{
    extract::{Form, Path, State},
    response::{Html, Json, Redirect},
};
use dashmap::DashMap;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap},
    fs,
    sync::Arc,
    time::{SystemTime},
};
use tower_sessions::{Session};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct TopGearRunRequest {
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

pub async fn post_topgear(
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

pub async fn post_run_topgear_batch(
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
        tracing::error!("Queue error: {}", e);
        return Err("Queue full".into());
    }

    Ok(Redirect::to(&format!("/topgear/{}", id)))
}

pub async fn get_topgear_result_page(
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
        Err(e) => {
            tracing::error!("Template for topgear not found: {}", e);
            Html("<h1>Error: Template not found</h1>".to_string())
        }
    }
}

pub fn generate_combinations(items_map: HashMap<String, Vec<String>>) -> Vec<Vec<String>> {
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

pub fn process_topgear_batch(
    id: &str,
    base_simc: &str,
    items: HashMap<String, Vec<String>>,
    output_path: &str,
    job_statuses: Arc<DashMap<String, SimStatus>>,
) -> std::io::Result<()> {
    let combinations = generate_combinations(items);
    let total = combinations.len();
    tracing::info!("TopGear {}: Generated {} combinations", id, total);

    job_statuses.insert(
        id.to_string(),
        SimStatus::TopGearProcessing { current: 0, total },
    );

    let mut results = Vec::new();

    for (i, combination) in combinations.iter().enumerate() {
        job_statuses.insert(
            id.to_string(),
            SimStatus::TopGearProcessing {
                current: i + 1,
                total,
            },
        );

        let mut content = base_simc.to_string();
        content.push_str("\n\n# --- Top Gear Combination ---\n");
        
        let mut finger_count = 0;
        let mut trinket_count = 0;
        for line in combination {
            let mut processed_line = line.clone();
            if line.starts_with("finger") {
                finger_count += 1;
                let parts: Vec<&str> = line.splitn(2, '=').collect();
                if parts.len() == 2 {
                    processed_line = format!("finger{}={}", finger_count, parts[1]);
                }
            } 
            else if line.starts_with("trinket") {
                trinket_count += 1;
                let parts: Vec<&str> = line.splitn(2, '=').collect();
                if parts.len() == 2 {
                    processed_line = format!("trinket{}={}", trinket_count, parts[1]);
                }
            }

            content.push_str(&processed_line);
            content.push('\n');
        }

        content.push_str("max_time=300\niterations=1000\n");

        let temp_simc_path = format!("files/{}_{}.simc", id, i);
        let temp_json_path = format!("files/{}_{}.json", id, i);

        if let Err(e) = fs::write(&temp_simc_path, &content) {
            tracing::error!("Write err: {}", e);
            continue;
        }

        if let Ok(_) = simulate::run_simc(&temp_simc_path, &temp_json_path) {
            if let Ok(json_str) = fs::read_to_string(&temp_json_path) {
                if let Ok(v) = serde_json::from_str::<Value>(&json_str) {
                    if let Some(dps) =
                        v["sim"]["players"][0]["collected_data"]["dps"]["mean"].as_f64()
                    {
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

/* TESTS */
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_combinations_empty() {
        // Should calculate 1 combination (equipped gear)
        // But should not add new lines
        let items = HashMap::new();
        let combos = generate_combinations(items);
        assert_eq!(combos.len(), 1);
        assert!(combos[0].is_empty());
    }

    #[test]
    fn test_combinations_simple() {
        // 2 heads, 1 chest = 2 combinations
        let mut items = HashMap::new();
        items.insert("Head".to_string(), vec!["h1".to_string(), "h2".to_string()]);
        items.insert("Chest".to_string(), vec!["c1".to_string()]);

        let combos = generate_combinations(items);
        assert_eq!(combos.len(), 2);
    }

    #[test]
    fn test_combinations_rings() {
        // 3 rings into 2 slots = 
        // = 3 combinations (1-2, 1-3, 2-3)
        // + 1 head
        // 3 * 1 = 3 combinations 
        let mut items = HashMap::new();
        items.insert("Finger".to_string(), vec!["r1".to_string(), "r2".to_string(), "r3".to_string()]);
        items.insert("Head".to_string(), vec!["h1".to_string()]);

        let combos = generate_combinations(items);
        assert_eq!(combos.len(), 3);
    }
}