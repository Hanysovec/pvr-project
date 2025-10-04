use axum::{
    Router,
    extract::{Form, Path},
    response::{Html, Json, Redirect},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::Value;
use std::{fs, net::SocketAddr};
use tower_http::services::ServeDir;
use uuid::Uuid;

use crate::simulate;

#[derive(Deserialize)]
struct SimulationInput {
    input_content: String,
}

pub async fn run_server() -> Result<(), String> {
    async fn post_simulation(Form(input): Form<SimulationInput>) -> Redirect {
        let id = Uuid::new_v4().to_string();
        let simc_file = format!("files/{}.simc", id);
        let output_file = format!("files/{}.json", id);

        if let Err(e) = fs::write(
            &simc_file,
            format!("{}\nmax_time=300\niterations=10000\n", input.input_content),
        ) {
            eprintln!("Error while writing: {}", e);
        }

        let simc_file_clone = simc_file.clone();
        let output_file_clone = output_file.clone();
        tokio::spawn(async move {
            if let Err(e) = simulate::run_simc(&simc_file_clone, &output_file_clone) {
                eprintln!("Error simulating: {}", e);
            }
            if let Err(e) = std::fs::remove_file(&simc_file_clone) {
                eprintln!("Error while removing file: {}", e);
            }
        });

        Redirect::to(&format!("/quicksim/{}", id))
    }

    async fn get_quicksim(Path(id): Path<String>) -> Html<String> {
        Html(format!(
            r#"
            <h1>QuickSim Result</h1>
            <div id="result">Simulation running</div>
            <script>
                async function check() {{
                    let res = await fetch("/quicksim/{id}/result");
                    if (res.ok) {{
                        let json = await res.json();
                        if (json.dps) {{
                            document.getElementById("result").textContent = "DPS: " + Math.round(json.dps);
                            return;
                        }}
                        if (json.error && json.error !== "Simulation running") {{
                            document.getElementById("result").textContent = "Error: " + json.error;
                            return;
                        }}
                    }}
                    setTimeout(check, 2000);
                }}
                check();
            </script>
        "#
        ))
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
        Json(serde_json::json!({ "error": "Simulation running" }))
    }

    let app = Router::new()
        .route("/run_simulation", post(post_simulation))
        .route("/quicksim/{id}", get(get_quicksim))
        .route("/quicksim/{id}/result", get(get_quicksim_result))
        .fallback_service(ServeDir::new("frontend"));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Error binding: {}", e))?;

    axum::serve(listener, app.into_make_service())
        .await
        .map_err(|e| format!("Error server: {}", e))
}
