use crate::server::AppState;
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use axum::{
    extract::{Form, Path, State},
    response::{Html, Json, Redirect},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Type;
use std::fs;
use tower_sessions::Session;

#[derive(Deserialize)]
pub struct AuthPayload {
    username: String,
    password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq, PartialOrd, Ord)]
#[sqlx(type_name = "TEXT")]
pub enum UserRole {
    User,
    Premium,
    Admin,
}
impl From<String> for UserRole {
    fn from(s: String) -> Self {
        match s.as_str() {
            "Admin" => UserRole::Admin,
            "Premium" => UserRole::Premium,
            "User" => UserRole::User,
            _ => UserRole::User,
        }
    }
}

#[derive(sqlx::FromRow, Serialize, Debug)]
pub struct _User {
    pub id: i64,
    pub username: String,
    #[serde(skip)]
    pub password_hash: String,
    pub created_at: String,
    pub role: UserRole,
}

pub const LIMIT_COMBINATIONS_FREE: usize = 50;
pub const LIMIT_COMBINATIONS_PREMIUM: usize = 200;

pub async fn register_page() -> Html<String> {
    match fs::read_to_string("frontend/register.html") {
        Ok(content) => Html(content),
        Err(_) => Html("<h1>Error: Login page not found</h1>".to_string()),
    }
}

pub async fn register(
    State(state): State<AppState>,
    Form(payload): Form<AuthPayload>,
) -> Result<Redirect, String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let password_hash = argon2
        .hash_password(payload.password.as_bytes(), &salt)
        .map_err(|e| e.to_string())?
        .to_string();

    let result = sqlx::query("INSERT INTO users (username, password_hash, role) VALUES (?, ?, ?)")
        .bind(&payload.username)
        .bind(&password_hash)
        .bind("User")
        .execute(&state.db)
        .await;

    match result {
        Ok(_) => Ok(Redirect::to("/")),
        Err(_) => Err("Username already taken".to_string()),
    }
}

pub async fn login(
    State(state): State<AppState>,
    session: Session,
    Form(payload): Form<AuthPayload>,
) -> Result<Redirect, String> {
    let row: Option<(i64, String, String)> =
        sqlx::query_as("SELECT id, password_hash, role FROM users WHERE username = ?")
            .bind(&payload.username)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| e.to_string())?;

    if let Some((id, hash, role_str)) = row {
        let parsed_hash = PasswordHash::new(&hash).map_err(|e| e.to_string())?;
        if Argon2::default()
            .verify_password(payload.password.as_bytes(), &parsed_hash)
            .is_ok()
        {
            session.insert("user_id", id).await.ok();
            session.insert("username", payload.username).await.ok();
            session.insert("role", role_str).await.ok();

            return Ok(Redirect::to("/"));
        }
    }

    Err("Invalid username or password".to_string())
}

pub async fn logout(session: Session) -> Redirect {
    session.flush().await.ok();
    Redirect::to("/")
}

pub async fn get_user_profile(
    State(state): State<AppState>,
    Path(username): Path<String>,
    _session: Session,
) -> Html<String> {
    let user_info: Option<(String,)> = sqlx::query_as("SELECT role FROM users WHERE username = ?")
        .bind(&username)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);

    let role_display = user_info.map(|(r,)| r).unwrap_or("Unknown".to_string());

    let rows = sqlx::query_as::<_, (String, String, Option<f64>, String)>(
        r#"
        SELECT h.id, h.sim_type, h.dps, h.created_at 
        FROM history h 
        JOIN users u ON h.user_id = u.id 
        WHERE u.username = ? 
        ORDER BY h.created_at DESC LIMIT 20
        "#,
    )
    .bind(&username)
    .fetch_all(&state.db)
    .await
    .unwrap_or(vec![]);

    let mut history_html = String::new();
    for (id, sim_type, dps, date) in rows {
        let dps_display = dps.map(|d| format!("{:.0}", d)).unwrap_or("-".to_string());

        let link = if sim_type == "TopGear" {
            "topgear"
        } else {
            "quicksim"
        };

        history_html.push_str(&format!(
            "<tr>
                <td>{}</td>
                <td>{}</td>
                <td class='dps-val'>{}</td>
                <td style='font-family: monospace; font-size: 0.9em; color: #888;'>{}</td>
                <td><a href='/{}/{}' style='color:#ffcc00; text-decoration: none;'>View Result</a></td>
            </tr>",
            sim_type, date, dps_display, id, link, id
        ));
    }

    let template = fs::read_to_string("frontend/account.html")
        .unwrap_or("<h1>Error: account.html not found</h1>".to_string());

    let html = template
        .replace("{{USERNAME}}", &username)
        .replace("{{ROLE}}", &role_display)
        .replace("{{HISTORY_ROWS}}", &history_html);

    Html(html)
}

pub async fn get_user_limits(State(state): State<AppState>, session: Session) -> Json<Value> {
    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    let role_str = if let Some(id) = user_id {
        sqlx::query_scalar::<_, String>("SELECT role FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None)
            .unwrap_or("User".to_string())
    } else {
        "User".to_string()
    };

    let role = UserRole::from(role_str);
    let is_premium = role >= UserRole::Premium;

    let limit = if is_premium {
        LIMIT_COMBINATIONS_PREMIUM
    } else {
        LIMIT_COMBINATIONS_FREE
    };

    Json(json!({
        "limit": limit,
        "is_premium": is_premium,
        "role": role
    }))
}
