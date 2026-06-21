use rusqlite::{Connection, ErrorCode, OptionalExtension};
use serde_json::Value;

pub const AUTH_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLogoutScope {
    Global,
    Local,
    Others,
}

#[derive(Debug)]
pub enum AuthStoreError {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    UserAlreadyRegistered,
    UserNotFound,
    InvalidRefreshToken,
    SessionRequired,
}

impl std::fmt::Display for AuthStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Json(err) => write!(f, "json error: {err}"),
            Self::UserAlreadyRegistered => write!(f, "User already registered"),
            Self::UserNotFound => write!(f, "User not found"),
            Self::InvalidRefreshToken => write!(f, "Invalid refresh token"),
            Self::SessionRequired => write!(f, "Session id is required"),
        }
    }
}

impl std::error::Error for AuthStoreError {}

impl From<rusqlite::Error> for AuthStoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<serde_json::Error> for AuthStoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug, Clone, Default)]
pub struct AuthUserPatch {
    pub email: Option<String>,
    pub phone: Option<String>,
    pub password_hash: Option<String>,
    pub user_metadata: Option<Value>,
    pub updated_at: String,
}

/// Creates the minimal durable GoTrue-compatible schema for per-project auth.
///
/// Future provider identities, MFA factors, audit logs, and sessions should reference
/// `_sdb_auth_users.id` instead of storing principal state outside the project snapshot/WAL chain.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS _sdb_auth_users(
            id TEXT PRIMARY KEY,
            email TEXT UNIQUE,
            phone TEXT UNIQUE,
            password_hash TEXT NOT NULL,
            user_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS _sdb_auth_users_email_idx
            ON _sdb_auth_users(email);
        CREATE INDEX IF NOT EXISTS _sdb_auth_users_phone_idx
            ON _sdb_auth_users(phone);

        CREATE TABLE IF NOT EXISTS _sdb_auth_refresh_tokens(
            token TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            session_id TEXT,
            parent_token TEXT,
            created_at INTEGER NOT NULL,
            expires_at INTEGER,
            revoked_at INTEGER,
            FOREIGN KEY(user_id) REFERENCES _sdb_auth_users(id) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS _sdb_auth_sessions(
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            revoked_at INTEGER,
            FOREIGN KEY(user_id) REFERENCES _sdb_auth_users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS _sdb_auth_refresh_tokens_user_id_idx
            ON _sdb_auth_refresh_tokens(user_id);
        CREATE INDEX IF NOT EXISTS _sdb_auth_refresh_tokens_active_idx
            ON _sdb_auth_refresh_tokens(token, revoked_at);
        CREATE INDEX IF NOT EXISTS _sdb_auth_refresh_tokens_session_idx
            ON _sdb_auth_refresh_tokens(session_id);
        CREATE INDEX IF NOT EXISTS _sdb_auth_sessions_user_id_idx
            ON _sdb_auth_sessions(user_id);
        ",
    )?;
    add_column_if_missing(
        conn,
        "_sdb_auth_refresh_tokens",
        "session_id",
        "ALTER TABLE _sdb_auth_refresh_tokens ADD COLUMN session_id TEXT",
    )?;
    add_column_if_missing(
        conn,
        "_sdb_auth_refresh_tokens",
        "expires_at",
        "ALTER TABLE _sdb_auth_refresh_tokens ADD COLUMN expires_at INTEGER",
    )?;
    Ok(())
}

pub struct AuthStore<'a> {
    conn: &'a Connection,
}

impl<'a> AuthStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn get_user_by_email_or_phone(
        &self,
        identifier: &str,
    ) -> Result<Option<Value>, AuthStoreError> {
        self.conn
            .query_row(
                "
                SELECT user_json
                FROM _sdb_auth_users
                WHERE email=?1 OR phone=?1
                LIMIT 1
                ",
                [identifier],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|raw| serde_json::from_str(&raw).map_err(AuthStoreError::from))
            .transpose()
    }

    pub fn get_user_by_id(&self, user_id: &str) -> Result<Option<Value>, AuthStoreError> {
        self.conn
            .query_row(
                "SELECT user_json FROM _sdb_auth_users WHERE id=?",
                [user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|raw| serde_json::from_str(&raw).map_err(AuthStoreError::from))
            .transpose()
    }

    pub fn get_active_refresh_token(&self, token: &str, now: i64) -> Result<Value, AuthStoreError> {
        let existing = self
            .conn
            .query_row(
                "
                SELECT rt.session_id, u.user_json
                FROM _sdb_auth_refresh_tokens rt
                JOIN _sdb_auth_users u ON u.id = rt.user_id
                JOIN _sdb_auth_sessions s ON s.id = rt.session_id
                WHERE rt.token=?
                  AND rt.revoked_at IS NULL
                  AND rt.expires_at > ?
                  AND s.revoked_at IS NULL
                  AND s.expires_at > ?
                ",
                (token, now, now),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((session_id, user_json)) = existing else {
            return Err(AuthStoreError::InvalidRefreshToken);
        };
        Ok(serde_json::json!({
            "session_id": session_id,
            "user": serde_json::from_str::<Value>(&user_json)?
        }))
    }

    #[cfg(test)]
    pub fn active_refresh_token_count(
        &self,
        user_id: &str,
        now: i64,
    ) -> Result<i64, AuthStoreError> {
        Ok(self.conn.query_row(
            "
            SELECT COUNT(*)
            FROM _sdb_auth_refresh_tokens rt
            JOIN _sdb_auth_sessions s ON s.id = rt.session_id
            WHERE rt.user_id=?
              AND rt.revoked_at IS NULL
              AND rt.expires_at > ?
              AND s.revoked_at IS NULL
              AND s.expires_at > ?
            ",
            (user_id, now, now),
            |row| row.get(0),
        )?)
    }
}

pub struct AuthStoreWriter<'a> {
    conn: &'a mut Connection,
}

impl<'a> AuthStoreWriter<'a> {
    pub fn new(conn: &'a mut Connection) -> Self {
        Self { conn }
    }

    pub fn create_user(&mut self, user: Value) -> Result<Value, AuthStoreError> {
        let user_id = required_str(&user, "id")?;
        let password_hash = required_str(&user, "password_hash")?;
        let created_at = required_str(&user, "created_at")?;
        let updated_at = required_str(&user, "updated_at")?;
        let email = optional_str(&user, "email");
        let phone = optional_str(&user, "phone");
        let user_json = serde_json::to_string(&user)?;

        let result = self.conn.execute(
            "
            INSERT INTO _sdb_auth_users(
                id, email, phone, password_hash, user_json, created_at, updated_at
            )
            VALUES(?, ?, ?, ?, ?, ?, ?)
            ",
            (
                user_id,
                email.as_deref(),
                phone.as_deref(),
                password_hash,
                &user_json,
                created_at,
                updated_at,
            ),
        );
        match result {
            Ok(_) => Ok(user),
            Err(err) if is_constraint_violation(&err) => Err(AuthStoreError::UserAlreadyRegistered),
            Err(err) => Err(AuthStoreError::Sqlite(err)),
        }
    }

    pub fn issue_refresh_token(
        &mut self,
        token: &str,
        user_id: &str,
        session_id: &str,
        created_at: i64,
        expires_at: i64,
    ) -> Result<(), AuthStoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "
            INSERT INTO _sdb_auth_sessions(id, user_id, created_at, expires_at)
            VALUES(?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET expires_at=excluded.expires_at
            ",
            (session_id, user_id, created_at, expires_at),
        )?;
        tx.execute(
            "
            INSERT INTO _sdb_auth_refresh_tokens(token, user_id, session_id, created_at, expires_at)
            VALUES(?, ?, ?, ?, ?)
            ",
            (token, user_id, session_id, created_at, expires_at),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn rotate_refresh_token(
        &mut self,
        old_token: &str,
        new_token: &str,
        created_at: i64,
        expires_at: i64,
    ) -> Result<Value, AuthStoreError> {
        let tx = self.conn.transaction()?;
        let existing = tx
            .query_row(
                "
                SELECT rt.user_id, rt.session_id, u.user_json
                FROM _sdb_auth_refresh_tokens rt
                JOIN _sdb_auth_users u ON u.id = rt.user_id
                JOIN _sdb_auth_sessions s ON s.id = rt.session_id
                WHERE rt.token=?
                  AND rt.revoked_at IS NULL
                  AND rt.expires_at > ?
                  AND s.revoked_at IS NULL
                  AND s.expires_at > ?
                ",
                (old_token, created_at, created_at),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((user_id, session_id, user_json)) = existing else {
            return Err(AuthStoreError::InvalidRefreshToken);
        };
        tx.execute(
            "UPDATE _sdb_auth_refresh_tokens SET revoked_at=? WHERE token=?",
            (created_at, old_token),
        )?;
        tx.execute(
            "UPDATE _sdb_auth_sessions SET expires_at=? WHERE id=?",
            (expires_at, &session_id),
        )?;
        tx.execute(
            "
            INSERT INTO _sdb_auth_refresh_tokens(token, user_id, session_id, parent_token, created_at, expires_at)
            VALUES(?, ?, ?, ?, ?, ?)
            ",
            (new_token, &user_id, &session_id, old_token, created_at, expires_at),
        )?;
        tx.commit()?;
        Ok(serde_json::json!({
            "session_id": session_id,
            "user": serde_json::from_str::<Value>(&user_json)?
        }))
    }

    pub fn revoke_sessions(
        &mut self,
        user_id: &str,
        current_session_id: Option<&str>,
        scope: AuthLogoutScope,
        revoked_at: i64,
    ) -> Result<i64, AuthStoreError> {
        let tx = self.conn.transaction()?;
        let changed = match scope {
            AuthLogoutScope::Global => {
                let sessions = tx.execute(
                    "
                    UPDATE _sdb_auth_sessions
                    SET revoked_at=?
                    WHERE user_id=? AND revoked_at IS NULL
                    ",
                    (revoked_at, user_id),
                )?;
                tx.execute(
                    "
                    UPDATE _sdb_auth_refresh_tokens
                    SET revoked_at=?
                    WHERE user_id=? AND revoked_at IS NULL
                    ",
                    (revoked_at, user_id),
                )?;
                sessions as i64
            }
            AuthLogoutScope::Local => {
                let session_id = current_session_id.ok_or(AuthStoreError::SessionRequired)?;
                let sessions = tx.execute(
                    "
                    UPDATE _sdb_auth_sessions
                    SET revoked_at=?
                    WHERE user_id=? AND id=? AND revoked_at IS NULL
                    ",
                    (revoked_at, user_id, session_id),
                )?;
                tx.execute(
                    "
                    UPDATE _sdb_auth_refresh_tokens
                    SET revoked_at=?
                    WHERE user_id=? AND session_id=? AND revoked_at IS NULL
                    ",
                    (revoked_at, user_id, session_id),
                )?;
                sessions as i64
            }
            AuthLogoutScope::Others => {
                let session_id = current_session_id.ok_or(AuthStoreError::SessionRequired)?;
                let sessions = tx.execute(
                    "
                    UPDATE _sdb_auth_sessions
                    SET revoked_at=?
                    WHERE user_id=? AND id<>? AND revoked_at IS NULL
                    ",
                    (revoked_at, user_id, session_id),
                )?;
                tx.execute(
                    "
                    UPDATE _sdb_auth_refresh_tokens
                    SET revoked_at=?
                    WHERE user_id=? AND session_id<>? AND revoked_at IS NULL
                    ",
                    (revoked_at, user_id, session_id),
                )?;
                sessions as i64
            }
        };
        tx.commit()?;
        Ok(changed)
    }

    pub fn update_user(
        &mut self,
        user_id: &str,
        patch: AuthUserPatch,
    ) -> Result<Value, AuthStoreError> {
        let Some(mut user) = AuthStore::new(self.conn).get_user_by_id(user_id)? else {
            return Err(AuthStoreError::UserNotFound);
        };
        if let Some(email) = patch.email {
            user["email"] = Value::String(email);
        }
        if let Some(phone) = patch.phone {
            user["phone"] = Value::String(phone);
        }
        if let Some(password_hash) = patch.password_hash {
            user["password_hash"] = Value::String(password_hash);
        }
        if let Some(user_metadata) = patch.user_metadata {
            user["user_metadata"] = user_metadata;
        }
        user["updated_at"] = Value::String(patch.updated_at.clone());

        let email = optional_str(&user, "email");
        let phone = optional_str(&user, "phone");
        let password_hash = required_str(&user, "password_hash")?.to_string();
        let user_json = serde_json::to_string(&user)?;
        let result = self.conn.execute(
            "
            UPDATE _sdb_auth_users
            SET email=?, phone=?, password_hash=?, user_json=?, updated_at=?
            WHERE id=?
            ",
            (
                email.as_deref(),
                phone.as_deref(),
                &password_hash,
                &user_json,
                &patch.updated_at,
                user_id,
            ),
        );
        match result {
            Ok(0) => Err(AuthStoreError::UserNotFound),
            Ok(_) => Ok(user),
            Err(err) if is_constraint_violation(&err) => Err(AuthStoreError::UserAlreadyRegistered),
            Err(err) => Err(AuthStoreError::Sqlite(err)),
        }
    }
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, AuthStoreError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        AuthStoreError::Json(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("missing auth user field: {field}"),
        )))
    })
}

fn optional_str(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn is_constraint_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == ErrorCode::ConstraintViolation
    )
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    sql: &str,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .any(|name| name == column);
    if !exists {
        conn.execute_batch(sql)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(email: &str) -> Value {
        json!({
            "id": "user-1",
            "email": email,
            "phone": null,
            "password_hash": "pw",
            "user_metadata": {},
            "app_metadata": {},
            "aud": "authenticated",
            "role": "authenticated",
            "created_at": "2026-06-21T00:00:00Z",
            "email_confirmed_at": "2026-06-21T00:00:00Z",
            "last_sign_in_at": "2026-06-21T00:00:00Z",
            "updated_at": "2026-06-21T00:00:00Z"
        })
    }

    #[test]
    fn schema_stores_users_and_refresh_token_rotation() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();

        let created = AuthStoreWriter::new(&mut conn)
            .create_user(user("a@example.com"))
            .unwrap();
        assert_eq!(created["email"], "a@example.com");
        assert_eq!(
            AuthStore::new(&conn)
                .get_user_by_email_or_phone("a@example.com")
                .unwrap()
                .unwrap()["id"],
            "user-1"
        );

        AuthStoreWriter::new(&mut conn)
            .issue_refresh_token("rt-1", "user-1", "session-1", 1, 100)
            .unwrap();
        let active = AuthStore::new(&conn)
            .get_active_refresh_token("rt-1", 2)
            .unwrap();
        assert_eq!(active["session_id"], "session-1");
        let rotated = AuthStoreWriter::new(&mut conn)
            .rotate_refresh_token("rt-1", "rt-2", 2, 101)
            .unwrap();
        assert_eq!(rotated["user"]["id"], "user-1");
        assert_eq!(rotated["session_id"], "session-1");
        assert!(matches!(
            AuthStoreWriter::new(&mut conn).rotate_refresh_token("rt-1", "rt-3", 3, 102),
            Err(AuthStoreError::InvalidRefreshToken)
        ));
        assert!(matches!(
            AuthStore::new(&conn).get_active_refresh_token("rt-2", 102),
            Err(AuthStoreError::InvalidRefreshToken)
        ));
    }

    #[test]
    fn revokes_refresh_tokens_by_logout_scope() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        AuthStoreWriter::new(&mut conn)
            .create_user(user("a@example.com"))
            .unwrap();
        AuthStoreWriter::new(&mut conn)
            .issue_refresh_token("rt-1", "user-1", "session-1", 1, 100)
            .unwrap();
        AuthStoreWriter::new(&mut conn)
            .issue_refresh_token("rt-2", "user-1", "session-2", 1, 100)
            .unwrap();

        AuthStoreWriter::new(&mut conn)
            .revoke_sessions("user-1", Some("session-1"), AuthLogoutScope::Local, 2)
            .unwrap();
        assert!(matches!(
            AuthStore::new(&conn).get_active_refresh_token("rt-1", 3),
            Err(AuthStoreError::InvalidRefreshToken)
        ));
        assert!(
            AuthStore::new(&conn)
                .get_active_refresh_token("rt-2", 3)
                .is_ok()
        );

        AuthStoreWriter::new(&mut conn)
            .revoke_sessions("user-1", Some("session-1"), AuthLogoutScope::Others, 4)
            .unwrap();
        assert!(matches!(
            AuthStore::new(&conn).get_active_refresh_token("rt-2", 5),
            Err(AuthStoreError::InvalidRefreshToken)
        ));

        AuthStoreWriter::new(&mut conn)
            .issue_refresh_token("rt-3", "user-1", "session-3", 6, 100)
            .unwrap();
        AuthStoreWriter::new(&mut conn)
            .revoke_sessions("user-1", None, AuthLogoutScope::Global, 7)
            .unwrap();
        assert!(matches!(
            AuthStore::new(&conn).get_active_refresh_token("rt-3", 8),
            Err(AuthStoreError::InvalidRefreshToken)
        ));
    }

    #[test]
    fn update_user_updates_json_and_lookup_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        AuthStoreWriter::new(&mut conn)
            .create_user(user("old@example.com"))
            .unwrap();

        let updated = AuthStoreWriter::new(&mut conn)
            .update_user(
                "user-1",
                AuthUserPatch {
                    email: Some("new@example.com".to_string()),
                    user_metadata: Some(json!({ "updated": true })),
                    updated_at: "2026-06-21T00:00:01Z".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(updated["email"], "new@example.com");
        assert_eq!(updated["user_metadata"]["updated"], true);
        assert!(
            AuthStore::new(&conn)
                .get_user_by_email_or_phone("old@example.com")
                .unwrap()
                .is_none()
        );
        assert!(
            AuthStore::new(&conn)
                .get_user_by_email_or_phone("new@example.com")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn duplicate_email_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        AuthStoreWriter::new(&mut conn)
            .create_user(user("a@example.com"))
            .unwrap();
        let dup = AuthStoreWriter::new(&mut conn).create_user(user("a@example.com"));
        assert!(matches!(dup, Err(AuthStoreError::UserAlreadyRegistered)));
    }

    #[test]
    fn get_user_by_id_returns_user() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        AuthStoreWriter::new(&mut conn)
            .create_user(user("a@example.com"))
            .unwrap();
        let found = AuthStore::new(&conn).get_user_by_id("user-1").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap()["email"], "a@example.com");
    }

    #[test]
    fn get_user_by_id_returns_none_for_missing() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let found = AuthStore::new(&conn).get_user_by_id("nonexistent").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn update_user_returns_error_for_missing_user() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        let result = AuthStoreWriter::new(&mut conn).update_user(
            "nonexistent",
            AuthUserPatch {
                email: Some("new@example.com".to_string()),
                ..Default::default()
            },
        );
        assert!(matches!(result, Err(AuthStoreError::UserNotFound)));
    }

    #[test]
    fn update_user_password_hash() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        AuthStoreWriter::new(&mut conn)
            .create_user(user("a@example.com"))
            .unwrap();
        let updated = AuthStoreWriter::new(&mut conn)
            .update_user(
                "user-1",
                AuthUserPatch {
                    password_hash: Some("new_hash".to_string()),
                    updated_at: "2026-06-21T00:00:02Z".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated["password_hash"], "new_hash");
    }

    #[test]
    fn expired_refresh_token_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        AuthStoreWriter::new(&mut conn)
            .create_user(user("a@example.com"))
            .unwrap();
        AuthStoreWriter::new(&mut conn)
            .issue_refresh_token("rt-1", "user-1", "session-1", 1, 10)
            .unwrap();
        assert!(matches!(
            AuthStore::new(&conn).get_active_refresh_token("rt-1", 20),
            Err(AuthStoreError::InvalidRefreshToken)
        ));
    }
}
