use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub sub: Option<String>,
    pub role: String,
    #[serde(default)]
    pub claims: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AuthError {}

impl Actor {
    pub fn is_service_role(&self) -> bool {
        self.role == "service_role"
    }

    pub fn is_admin(&self) -> bool {
        self.role == "service_role" || self.role == "admin"
    }

    pub fn is_anon(&self) -> bool {
        self.role == "anon"
    }

    pub fn is_authenticated(&self) -> bool {
        self.role != "anon"
    }
}

pub fn mint_token(
    sub: &str,
    role: &str,
    claims: Map<String, Value>,
    expires_in: Option<i64>,
) -> Result<String, AuthError> {
    let now = now_secs();
    let mut payload = json!({
        "sub": sub,
        "role": role,
        "claims": claims,
        "iat": now
    });
    if let Some(expires_in) = expires_in {
        payload["exp"] = json!(now + expires_in);
    }
    let header = json!({ "alg": "HS256", "typ": "JWT" });
    let signing_input = format!("{}.{}", b64_json(&header)?, b64_json(&payload)?);
    let sig = sign(&signing_input)?;
    Ok(format!("{signing_input}.{sig}"))
}

pub fn actor_from_authorization(header: Option<&str>) -> Result<Actor, AuthError> {
    match header {
        None => Ok(Actor {
            sub: None,
            role: "anon".to_string(),
            claims: Map::new(),
        }),
        Some(value) if value.starts_with("Bearer ") => {
            verify_token(value.trim_start_matches("Bearer ").trim())
        }
        Some(_) => Err(AuthError(
            "authorization header must use Bearer token".to_string(),
        )),
    }
}

pub fn verify_token(token: &str) -> Result<Actor, AuthError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(AuthError("invalid token format".to_string()));
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected = sign(&signing_input)?;
    if expected.as_bytes() != parts[2].as_bytes() {
        return Err(AuthError("invalid token signature".to_string()));
    }

    let header: Value = decode_json(parts[0])?;
    if header.get("alg").and_then(Value::as_str) != Some("HS256") {
        return Err(AuthError("unsupported token algorithm".to_string()));
    }
    let payload: Value = decode_json(parts[1])?;
    if let Some(exp) = payload.get("exp").and_then(Value::as_i64) {
        if exp < now_secs() {
            return Err(AuthError("token expired".to_string()));
        }
    }
    let claims = payload
        .get("claims")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Ok(Actor {
        sub: payload
            .get("sub")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        role: payload
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("authenticated")
            .to_string(),
        claims,
    })
}

pub fn actor_claim(actor: &Actor, name: &str) -> Value {
    match name {
        "sub" => actor.sub.clone().map(Value::String).unwrap_or(Value::Null),
        "role" => Value::String(actor.role.clone()),
        other => actor.claims.get(other).cloned().unwrap_or(Value::Null),
    }
}

fn b64_json(value: &Value) -> Result<String, AuthError> {
    serde_json::to_vec(value)
        .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
        .map_err(|err| AuthError(format!("failed to encode token: {err}")))
}

fn decode_json(input: &str) -> Result<Value, AuthError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| AuthError("invalid token payload".to_string()))?;
    serde_json::from_slice(&bytes).map_err(|_| AuthError("invalid token payload".to_string()))
}

pub fn is_production() -> bool {
    std::env::var("SDB_ENV").as_deref() == Ok("production")
}

fn jwt_secret() -> Result<String, AuthError> {
    match std::env::var("SDB_JWT_SECRET") {
        Ok(secret) if !secret.is_empty() => Ok(secret),
        _ if is_production() => Err(AuthError(
            "SDB_JWT_SECRET is required in production mode (SDB_ENV=production)".to_string(),
        )),
        _ => Ok("dev-secret-change-me".to_string()),
    }
}

fn sign(input: &str) -> Result<String, AuthError> {
    let secret = jwt_secret()?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AuthError("invalid secret".to_string()))?;
    mac.update(input.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
