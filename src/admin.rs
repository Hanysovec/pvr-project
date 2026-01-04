use crate::server::AppState;
use argon2::{
    Argon2, PasswordHasher,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    extract::{Form, State},
    response::{Html, Redirect},
};
use serde::Deserialize;
use std::fs;
use tower_sessions::Session;

#[derive(sqlx::FromRow)]
struct UserSummary {
    id: i64,
    username: String,
    role: String,
    created_at: String,
}

#[derive(Deserialize)]
pub struct UpdateRolePayload {
    user_id: i64,
    new_role: String,
}

#[derive(Deserialize)]
pub struct DeleteUserPayload {
    user_id: i64,
}

#[derive(Deserialize)]
pub struct ResetPasswordPayload {
    user_id: i64,
    new_password: String,
}

async fn is_admin(state: &AppState, session: &Session) -> bool {
    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    if let Some(id) = user_id {
        let row: Option<(String,)> = sqlx::query_as("SELECT role FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);

        if let Some((role,)) = row {
            return role == "Admin";
        }
    }
    false
}

pub async fn admin_dashboard(State(state): State<AppState>, session: Session) -> Html<String> {
    if !is_admin(&state, &session).await {
        return Html("<h1>Access Denied</h1>".to_string());
    }

    let users = sqlx::query_as::<_, UserSummary>(
        "SELECT id, username, role, created_at FROM users ORDER BY id ASC",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or(vec![]);

    let mut rows_html = String::new();
    for u in users {
        let role_options = vec!["User", "Premium", "Admin"];
        let mut options_html = String::new();
        for r in role_options {
            let selected = if u.role == r { "selected" } else { "" };
            options_html.push_str(&format!(
                "<option value='{}' {}>{}</option>",
                r, selected, r
            ));
        }

        rows_html.push_str(&format!(
            r#"
            <tr>
                <td>{id}</td>
                <td>{username}</td>
                <td>{created_at}</td>
                <td>
                    <form action="/admin/role" method="post" style="display:inline;">
                        <input type="hidden" name="user_id" value="{id}">
                        <select name="new_role" onchange="this.form.submit()">
                            {options}
                        </select>
                    </form>
                </td>
                <td>
                    <form action="/admin/password" method="post" style="display:flex; gap:5px;">
                        <input type="hidden" name="user_id" value="{id}">
                        <input type="text" name="new_password" placeholder="New Pass" style="width:80px; padding:2px;">
                        <button type="submit" class="btn-small">Set</button>
                    </form>
                </td>
                <td>
                    <form action="/admin/delete" method="post" onsubmit="return confirm('Are you sure?');">
                        <input type="hidden" name="user_id" value="{id}">
                        <button type="submit" class="btn-danger">Delete</button>
                    </form>
                </td>
            </tr>
            "#,
            id = u.id,
            username = u.username,
            created_at = u.created_at,
            options = options_html
        ));
    }

    let template = fs::read_to_string("frontend/admin.html")
        .unwrap_or("<h1>Error loading template</h1>".to_string());
    Html(template.replace("{{USER_ROWS}}", &rows_html))
}

pub async fn update_role(
    State(state): State<AppState>,
    session: Session,
    Form(payload): Form<UpdateRolePayload>,
) -> Redirect {
    if !is_admin(&state, &session).await {
        return Redirect::to("/");
    }

    let _ = sqlx::query("UPDATE users SET role = ? WHERE id = ?")
        .bind(&payload.new_role)
        .bind(payload.user_id)
        .execute(&state.db)
        .await;

    Redirect::to("/admin")
}

pub async fn delete_user(
    State(state): State<AppState>,
    session: Session,
    Form(payload): Form<DeleteUserPayload>,
) -> Redirect {
    if !is_admin(&state, &session).await {
        return Redirect::to("/");
    }

    let _ = sqlx::query("DELETE FROM history WHERE user_id = ?")
        .bind(payload.user_id)
        .execute(&state.db)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(payload.user_id)
        .execute(&state.db)
        .await;

    Redirect::to("/admin")
}

pub async fn reset_password(
    State(state): State<AppState>,
    session: Session,
    Form(payload): Form<ResetPasswordPayload>,
) -> Redirect {
    if !is_admin(&state, &session).await {
        return Redirect::to("/");
    }

    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    if let Ok(hash) = argon2.hash_password(payload.new_password.as_bytes(), &salt) {
        let _ = sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(hash.to_string())
            .bind(payload.user_id)
            .execute(&state.db)
            .await;
    }

    Redirect::to("/admin")
}
