use axum::{
    Json, Router,
    extract::{Form, Path},
    response::Html,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, io::Write, net::SocketAddr};
use tempfile::NamedTempFile;
use tower_http::services::ServeDir;
use uuid::Uuid;

use crate::simulate;

#[derive(Serialize)]
struct SimulationResponse {
    id: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct SimulationInput {
    input_content: String,
}

pub async fn run_server() -> Result<(), String> {
    async fn post_simulation(Form(input): Form<SimulationInput>) -> Json<SimulationResponse> {
        let id = Uuid::new_v4().to_string();
        let output_file = format!("files/{}.json", id);

        let mut temp_file = match NamedTempFile::new() {
            Ok(f) => f,
            Err(e) => {
                return Json(SimulationResponse {
                    id,
                    error: Some(format!("Failed to create temp file: {}", e)),
                });
            }
        };

        if let Err(e) = write!(
            temp_file,
            "{}\nmax_time=300\niterations=10000\n",
            input.input_content
        ) {
            return Json(SimulationResponse {
                id,
                error: Some(format!("Failed to write temp file: {}", e)),
            });
        }

        let temp_path = temp_file.path().to_str().unwrap();

        let result = match simulate::run_simc(temp_path, &output_file) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Simulation failed: {}", e)),
        };

        Json(SimulationResponse {
            id,
            error: result.err(),
        })
    }

    async fn get_quicksim(Path(id): Path<String>) -> Html<String> {
        let file = format!("files/{}.json", id);

        match load_result(&file) {
            Ok(dps) => Html(format!(
                "<h1>QuickSim Result</h1><p>DPS: {}</p>",
                dps.round()
            )),
            Err(e) => Html(format!("<h1>Error</h1><p>{}</p>", e)),
        }
    }

    let app = Router::new()
        .route("/run_simulation", post(post_simulation))
        .route("/quicksim/{id}", get(get_quicksim))
        .fallback_service(ServeDir::new("frontend"));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Failed to bind TCP listener: {}", e))?;

    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| format!("Server error: {}", e))
}

fn load_result(file: &str) -> Result<f64, String> {
    let data =
        fs::read_to_string(file).map_err(|e| format!("Failed to read file {}: {}", file, e))?;

    let v: Value =
        serde_json::from_str(&data).map_err(|e| format!("Failed to parse JSON: {}", e))?;

    v["sim"]["players"]
        .as_array()
        .and_then(|players| players.get(0))
        .and_then(|player| player["collected_data"]["dps"]["mean"].as_f64())
        .ok_or("Could not find DPS".to_string())
}
