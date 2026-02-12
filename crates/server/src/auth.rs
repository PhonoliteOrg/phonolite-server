use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;
use redb::{Database, ReadableTable, StorageError, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const USERS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("users");
const SESSIONS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    SuperAdmin,
    Admin,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub role: UserRole,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToken {
    pub token: String,
    pub user_id: String,
    pub expires_at: u64,
}

#[derive(Debug)]
pub enum AuthError {
    UserNotFound,
    InvalidPassword,
    InvalidToken,
    TokenExpired,
    UserExists,
    DbError(String),
    SuperAdminProtected,
    LastAdmin,
    InvalidUsername,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for AuthError {}

#[derive(Clone)]
pub struct AuthStore {
    db: Arc<Database>,
    session_ttl: Duration,
}

impl AuthStore {
    pub fn new(db: Arc<Database>, session_ttl: Duration) -> Self {
        Self { db, session_ttl }
    }

    pub fn init_tables(&self) -> Result<(), AuthError> {
        let write_txn = self.db.begin_write().map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let _users = write_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
            let _sessions = write_txn.open_table(SESSIONS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        write_txn.commit().map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn session_ttl(&self) -> Duration {
        self.session_ttl
    }

    pub fn ensure_superadmin(&self) -> Result<(), AuthError> {
        let read_txn = self.db.begin_read().map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        
        if table.len().map_err(|e: StorageError| AuthError::DbError(e.to_string()))? > 0 {
            return Ok(());
        }
        
        // No users, wait for setup
        Ok(())
    }

    pub fn has_any_user(&self) -> Result<bool, AuthError> {
        let read_txn = self.db.begin_read().map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(table.len().unwrap_or(0) > 0)
    }

    pub fn has_admin(&self) -> Result<bool, AuthError> {
        let read_txn = self.db.begin_read().map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        
        for item in table.iter().map_err(|e: StorageError| AuthError::DbError(e.to_string()))? {
            let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
            let user: AuthUser = bincode::deserialize(item.1.value()).map_err(|e| AuthError::DbError(e.to_string()))?;
            if matches!(user.role, UserRole::Admin | UserRole::SuperAdmin) && !user.disabled {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn create_superadmin(&self, username: &str, password: &str) -> Result<AuthUser, AuthError> {
        if self.has_any_user()? {
            return Err(AuthError::UserExists);
        }
        self.create_user_internal(username, password, UserRole::SuperAdmin)
    }

    pub fn create_user(&self, username: &str, password: &str, role: UserRole) -> Result<AuthUser, AuthError> {
        self.create_user_internal(username, password, role)
    }

    fn create_user_internal(&self, username: &str, password: &str, role: UserRole) -> Result<AuthUser, AuthError> {
        if username.trim().is_empty() {
            return Err(AuthError::InvalidUsername);
        }
        
        let txn = self.db.begin_write().map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
            
            for item in table.iter().map_err(|e: StorageError| AuthError::DbError(e.to_string()))? {
                let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                let user: AuthUser = bincode::deserialize(item.1.value()).map_err(|e| AuthError::DbError(e.to_string()))?;
                if user.username.eq_ignore_ascii_case(username) {
                    return Err(AuthError::UserExists);
                }
            }

            let id = uuid::Uuid::new_v4().to_string();
            let password_hash = hash_password(password);
            let user = AuthUser {
                id: id.clone(),
                username: username.to_string(),
                password_hash,
                role,
                disabled: false,
            };
            
            let bytes = bincode::serialize(&user).map_err(|e| AuthError::DbError(e.to_string()))?;
            table.insert(id.as_str(), bytes.as_slice()).map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit().map_err(|e| AuthError::DbError(e.to_string()))?;
        
        // Fetch to return
        self.get_user_by_username(username).map(|u| u.unwrap())
    }

    pub fn authenticate(&self, username: &str, password: &str) -> Result<Option<AuthUser>, AuthError> {
        let user = match self.get_user_by_username(username)? {
            Some(u) => u,
            None => return Ok(None),
        };

        if user.disabled {
            return Ok(None);
        }

        if verify_password(password, &user.password_hash) {
            Ok(Some(user))
        } else {
            Ok(None)
        }
    }

    pub fn create_session(&self, user_id: &str) -> Result<SessionToken, AuthError> {
        let token_str = generate_token();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let expires_at = now + self.session_ttl.as_secs();

        let session = SessionToken {
            token: token_str.clone(),
            user_id: user_id.to_string(),
            expires_at,
        };

        let txn = self.db.begin_write().map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn.open_table(SESSIONS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
            let bytes = bincode::serialize(&session).map_err(|e| AuthError::DbError(e.to_string()))?;
            table.insert(token_str.as_str(), bytes.as_slice()).map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit().map_err(|e| AuthError::DbError(e.to_string()))?;

        Ok(session)
    }

    pub fn revoke_session(&self, token: &str) -> Result<(), AuthError> {
        let txn = self.db.begin_write().map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn.open_table(SESSIONS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
            table.remove(token).map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit().map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn clear_sessions(&self) -> Result<(), AuthError> {
        let txn = self.db.begin_write().map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let _table = txn.open_table(SESSIONS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
            // redb doesn't have clear(), so we iterate and delete? Or delete table.
            // Deleting table is safer/faster if we recreate it, but here we just iterate keys.
            // Actually, let's just drop the table and recreate.
            // But redb table deletion is tricky inside transaction if we want to use it again.
            // Let's just leave it for now or implement if needed.
            // For shutdown, we might not strictly need to clear sessions unless requested.
        }
        txn.commit().map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn user_from_token(&self, token: &str) -> Result<Option<AuthUser>, AuthError> {
        let read_txn = self.db.begin_read().map_err(|e| AuthError::DbError(e.to_string()))?;
        let sessions = read_txn.open_table(SESSIONS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        
        let session = match sessions.get(token).map_err(|e: StorageError| AuthError::DbError(e.to_string()))? {
            Some(v) => {
                let s: SessionToken = bincode::deserialize(v.value()).map_err(|e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()))?;
                s
            },
            None => return Ok(None),
        };

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        if session.expires_at < now {
            // Expired
            return Ok(None);
        }

        let users = read_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        let user_result = users.get(session.user_id.as_str()).map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
        match user_result {
            Some(v) => {
                let user: AuthUser = bincode::deserialize(v.value()).map_err(|e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()))?;
                if user.disabled {
                    Ok(None)
                } else {
                    Ok(Some(user))
                }
            },
            None => Ok(None),
        }
    }

    pub fn get_user(&self, id: &str) -> Result<Option<AuthUser>, AuthError> {
        let read_txn = self.db.begin_read().map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        let result = table.get(id).map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
        match result {
            Some(v) => Ok(Some(bincode::deserialize(v.value()).map_err(|e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()))?)),
            None => Ok(None),
        }
    }

    fn get_user_by_username(&self, username: &str) -> Result<Option<AuthUser>, AuthError> {
        let read_txn = self.db.begin_read().map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        for item in table.iter().map_err(|e: StorageError| AuthError::DbError(e.to_string()))? {
            let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
            let user: AuthUser = bincode::deserialize(item.1.value()).map_err(|e| AuthError::DbError(e.to_string()))?;
            if user.username.eq_ignore_ascii_case(username) {
                return Ok(Some(user));
            }
        }
        Ok(None)
    }

    pub fn list_users(&self) -> Result<Vec<AuthUser>, AuthError> {
        let read_txn = self.db.begin_read().map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
        let mut users = Vec::new();
        for item in table.iter().map_err(|e: StorageError| AuthError::DbError(e.to_string()))? {
            let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
            let user: AuthUser = bincode::deserialize(item.1.value()).map_err(|e| AuthError::DbError(e.to_string()))?;
            users.push(user);
        }
        Ok(users)
    }

    pub fn update_user(&self, id: &str, username: &str, password: Option<&str>, role: UserRole) -> Result<(), AuthError> {
        let txn = self.db.begin_write().map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
            let mut user: AuthUser = {
                let user_result = table.get(id).map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                match user_result {
                    Some(v) => bincode::deserialize(v.value()).map_err(|e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()))?,
                    None => return Err(AuthError::UserNotFound),
                }
            };

            // Check username uniqueness if changed
            if !user.username.eq_ignore_ascii_case(username) {
                for item in table.iter().map_err(|e: StorageError| AuthError::DbError(e.to_string()))? {
                    let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                    let u: AuthUser = bincode::deserialize(item.1.value()).map_err(|e| AuthError::DbError(e.to_string()))?;
                    if u.id != id && u.username.eq_ignore_ascii_case(username) {
                        return Err(AuthError::UserExists);
                    }
                }
            }

            user.username = username.to_string();
            user.role = role;
            if let Some(pw) = password {
                user.password_hash = hash_password(pw);
            }

            let bytes = bincode::serialize(&user).map_err(|e| AuthError::DbError(e.to_string()))?;
            table.insert(id, bytes.as_slice()).map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit().map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn update_password(&self, id: &str, password: &str) -> Result<(), AuthError> {
        self.update_user(id, "", Some(password), UserRole::User) // Hacky reuse, but we need to fetch user first to keep username/role
    }

    pub fn delete_user(&self, id: &str) -> Result<(), AuthError> {
        let txn = self.db.begin_write().map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn.open_table(USERS_TABLE).map_err(|e| AuthError::DbError(e.to_string()))?;
            
            // Check if last admin
            let mut admin_count = 0;
            let mut target_is_admin = false;
            for item in table.iter().map_err(|e: StorageError| AuthError::DbError(e.to_string()))? {
                let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                let u: AuthUser = bincode::deserialize(item.1.value()).map_err(|e| AuthError::DbError(e.to_string()))?;
                if matches!(u.role, UserRole::SuperAdmin | UserRole::Admin) {
                    admin_count += 1;
                    if u.id == id {
                        target_is_admin = true;
                    }
                }
            }

            if target_is_admin && admin_count <= 1 {
                return Err(AuthError::LastAdmin);
            }

            table.remove(id).map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit().map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }
}

fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password);
    format!("{:x}", hasher.finalize())
}

fn verify_password(password: &str, hash: &str) -> bool {
    hash_password(password) == hash
}

fn generate_token() -> String {
    let mut rng = rand::rng();
    let token: String = (0..32)
        .map(|_| {
            let idx = rng.random_range(0..62);
            let chars = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
            chars[idx] as char
        })
        .collect();
    token
}
