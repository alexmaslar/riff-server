use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user id
    pub username: String,
    pub role: String,
    pub exp: usize,
}

pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.len() < 8 {
        return Err("password must be at least 8 characters");
    }
    Ok(())
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("password hash failed: {}", e))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> anyhow::Result<bool> {
    let parsed = PasswordHash::new(hash).map_err(|e| anyhow::anyhow!("invalid hash: {}", e))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

pub fn create_token(
    user_id: &Uuid,
    username: &str,
    role: &str,
    secret: &str,
) -> anyhow::Result<String> {
    let expiration = Utc::now()
        .checked_add_signed(chrono::Duration::days(7))
        .expect("valid timestamp")
        .timestamp() as usize;

    let claims = Claims {
        sub: user_id.to_string(),
        username: username.to_string(),
        role: role.to_string(),
        exp: expiration,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?;

    Ok(token)
}

pub fn validate_token(token: &str, secret: &str) -> anyhow::Result<Claims> {
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )?;
    Ok(token_data.claims)
}

/// On first startup, create admin user if no users exist and admin_password is set in config.
pub async fn bootstrap_admin(pool: &SqlitePool, config: &Config) -> anyhow::Result<()> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;

    if count.0 > 0 {
        return Ok(());
    }

    let password = match &config.auth.admin_password {
        Some(p) if !p.is_empty() => p.clone(),
        _ => {
            tracing::warn!("no users exist and auth.admin_password is not set in config.yaml — set it to create an admin account");
            return Ok(());
        }
    };

    if password.len() < 8 {
        tracing::warn!(
            "admin_password is only {} characters — passwords should be at least 8 characters for security",
            password.len()
        );
    }

    let username = &config.auth.admin_username;
    let id = Uuid::new_v4();
    let hash = hash_password(&password)?;

    sqlx::query("INSERT INTO users (id, username, password_hash, role) VALUES (?, ?, ?, ?)")
        .bind(id.to_string())
        .bind(username)
        .bind(&hash)
        .bind("admin")
        .execute(pool)
        .await?;

    tracing::info!("created admin user '{}'", username);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_and_verify_password() {
        let password = "test_password_123";
        let hash = hash_password(password).unwrap();

        assert!(hash.starts_with("$argon2"));
        assert!(verify_password(password, &hash).unwrap());
    }

    #[test]
    fn test_wrong_password_fails_verification() {
        let hash = hash_password("correct_password").unwrap();
        assert!(!verify_password("wrong_password", &hash).unwrap());
    }

    #[test]
    fn test_different_hashes_for_same_password() {
        let password = "same_password";
        let hash1 = hash_password(password).unwrap();
        let hash2 = hash_password(password).unwrap();

        // Different salts should produce different hashes
        assert_ne!(hash1, hash2);
        // But both should verify
        assert!(verify_password(password, &hash1).unwrap());
        assert!(verify_password(password, &hash2).unwrap());
    }

    #[test]
    fn test_create_and_validate_token() {
        let user_id = Uuid::new_v4();
        let secret = "test_jwt_secret_key";

        let token = create_token(&user_id, "testuser", "admin", secret).unwrap();
        assert!(!token.is_empty());

        let claims = validate_token(&token, secret).unwrap();
        assert_eq!(claims.sub, user_id.to_string());
        assert_eq!(claims.username, "testuser");
        assert_eq!(claims.role, "admin");
    }

    #[test]
    fn test_token_with_wrong_secret_fails() {
        let user_id = Uuid::new_v4();
        let token = create_token(&user_id, "user", "user", "secret1").unwrap();

        let result = validate_token(&token, "secret2");
        assert!(result.is_err());
    }

    #[test]
    fn test_token_expiration_is_future() {
        let user_id = Uuid::new_v4();
        let secret = "test_secret";
        let token = create_token(&user_id, "user", "user", secret).unwrap();

        let claims = validate_token(&token, secret).unwrap();
        let now = Utc::now().timestamp() as usize;
        assert!(claims.exp > now);
    }

    #[test]
    fn test_token_contains_correct_role() {
        let user_id = Uuid::new_v4();
        let secret = "test_secret";

        let admin_token = create_token(&user_id, "admin", "admin", secret).unwrap();
        let user_token = create_token(&user_id, "user", "user", secret).unwrap();

        let admin_claims = validate_token(&admin_token, secret).unwrap();
        let user_claims = validate_token(&user_token, secret).unwrap();

        assert_eq!(admin_claims.role, "admin");
        assert_eq!(user_claims.role, "user");
    }

    #[test]
    fn test_invalid_token_string_fails() {
        let result = validate_token("not.a.valid.token", "secret");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_password_hashes() {
        let hash = hash_password("").unwrap();
        assert!(verify_password("", &hash).unwrap());
        assert!(!verify_password("notempty", &hash).unwrap());
    }

    #[test]
    fn test_invalid_hash_returns_error() {
        let result = verify_password("password", "not_a_valid_hash");
        assert!(result.is_err());
    }
}
