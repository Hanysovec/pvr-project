use crate::{
    users::UserRole,
    utils::{AppState, JobType, QueueItem, SimStatus, SimulationJob, generate_header},
};
use axum::{
    extract::{Form, Path, State},
    response::{Html, Redirect},
};
use serde::Deserialize;
use std::{fs, time::SystemTime};
use tower_sessions::Session;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct SimulationInput {
    pub input_content: String,
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
