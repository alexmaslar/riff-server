use sqlx::SqlitePool;

pub async fn log(
    pool: &SqlitePool,
    user_id: &str,
    username: &str,
    action: &str,
    details: Option<&str>,
    ip: Option<&str>,
) {
    let result = sqlx::query(
        "INSERT INTO audit_log (user_id, username, action, details, ip_address) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(username)
    .bind(action)
    .bind(details)
    .bind(ip)
    .execute(pool)
    .await;

    if let Err(e) = result {
        tracing::warn!("failed to write audit log: {e}");
    }
}
