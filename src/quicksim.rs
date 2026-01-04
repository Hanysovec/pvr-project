use crate::{
    simulate,
    users::UserRole,
    utils::{AppState, JobType, QueueItem, SimStatus, SimulationJob, generate_header},
};
use axum::{
    extract::{Form, Path, State},
    response::{Html, Redirect},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, time::SystemTime};
use tower_sessions::Session;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct SimulationInput {
    pub input_content: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct QuickSimResult {
    pub dps: f64,
    pub fight_length: f64,
    pub spells: Vec<SpellStats>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SpellStats {
    pub name: String,
    pub total_damage: f64,
    pub dps: f64,
    pub percent: f64,
    pub cast_count: f64,
}

fn parse_quicksim(raw_json_str: &str) -> Option<QuickSimResult> {
    let root: Value = match serde_json::from_str(raw_json_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("JSON parsing failed: {}", e);
            return None;
        }
    };

    let player = root.get("sim")?.get("players")?.get(0)?;
    let collected = player.get("collected_data")?;

    let dps = collected.get("dps")?.get("mean")?.as_f64().unwrap_or(0.0);
    let fight_len = collected
        .get("fight_length")?
        .get("mean")?
        .as_f64()
        .unwrap_or(0.0);

    let mut spells = Vec::new();
    let mut total_dmg_check = 0.0;

    if let Some(stats_array) = player.get("stats").and_then(|v| v.as_array()) {
        for spell_data in stats_array {
            let damage = spell_data
                .get("compound_amount")
                .and_then(|v| v.as_f64())
                .or_else(|| {
                    spell_data
                        .get("actual_amount")
                        .and_then(|a| a.get("mean"))
                        .and_then(|v| v.as_f64())
                })
                .unwrap_or(0.0);

            if damage <= 0.0 {
                continue;
            }

            let name = spell_data
                .get("spell_name")
                .and_then(|v| v.as_str())
                .or_else(|| spell_data.get("name").and_then(|v| v.as_str()))
                .unwrap_or("Unknown Spell")
                .to_string();

            let count = spell_data
                .get("num_executes")
                .and_then(|x| x.get("mean").or_else(|| x.get("count")))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);

            let spell_dps = if fight_len > 0.0 {
                damage / fight_len
            } else {
                0.0
            };

            total_dmg_check += damage;

            spells.push(SpellStats {
                name,
                total_damage: damage,
                dps: spell_dps,
                percent: 0.0,
                cast_count: count.round(),
            });
        }
    }

    spells.sort_by(|a, b| b.total_damage.partial_cmp(&a.total_damage).unwrap());
    if total_dmg_check > 0.0 {
        for s in &mut spells {
            s.percent = (s.total_damage / total_dmg_check) * 100.0;
        }
    }

    Some(QuickSimResult {
        dps,
        fight_length: fight_len,
        spells,
    })
}

pub fn process_quicksim_batch(id: &str, base_simc: &str, output_path: &str) -> std::io::Result<()> {
    let temp_simc = format!("files/{}.simc", id);
    let temp_raw_json = format!("files/{}_raw.json", id);

    fs::write(
        &temp_simc,
        format!("{}\nmax_time=300\niterations=1000\n", base_simc),
    )?;

    simulate::run_simc(&temp_simc, &temp_raw_json)?;

    let final_content = if let Ok(raw_content) = fs::read_to_string(&temp_raw_json) {
        if let Some(minified) = parse_quicksim(&raw_content) {
            serde_json::to_string(&minified)?
        } else {
            tracing::error!("Failed to parse QuickSim JSON for ID {}", id);
            String::from("{\"error\": \"Failed to parse SimC output\"}")
        }
    } else {
        String::from("{}")
    };

    fs::write(output_path, final_content)?;

    let _ = fs::remove_file(temp_simc);
    let _ = fs::remove_file(temp_raw_json);
    Ok(())
}

pub async fn post_simulation(
    State(state): State<AppState>,
    session: Session,
    Form(input): Form<SimulationInput>,
) -> Redirect {
    let id = Uuid::new_v4().to_string();
    let _ = fs::create_dir_all("files");

    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    let role_str: String = session
        .get("role")
        .await
        .unwrap_or(Some("User".to_string()))
        .unwrap_or("User".to_string());
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
        job_type: JobType::QuickSim {
            base_simc: input.input_content,
        },
        user_id,
    };

    let queue = if is_premium {
        &state.premium_queue
    } else {
        &state.standard_queue
    };
    if let Err(e) = queue.send(job).await {
        tracing::error!("Failed to queue job: {}", e);
        state
            .job_statuses
            .insert(id.clone(), SimStatus::Failed("Queue full".into()));
    }

    Redirect::to(&format!("/quicksim/{}", id))
}

pub async fn get_quicksim(session: Session, Path(id): Path<String>) -> Html<String> {
    match fs::read_to_string("frontend/quicksim_result.html") {
        Ok(template) => {
            let header_html = generate_header(&session).await;
            let html = template
                .replace("{{ID}}", &id)
                .replace("{{HEADER}}", &header_html);

            Html(html)
        }
        Err(e) => {
            tracing::error!("Template for quicksim not found: {}", e);
            Html("<h1>Error: Template not found</h1>".to_string())
        }
    }
}
