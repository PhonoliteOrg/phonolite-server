use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable, StorageError, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const USERS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("auth_users_v1");
const SESSIONS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("auth_sessions_v1");
const SECURITY_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("auth_security_v1");
const OPAQUE_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("auth_opaque_credentials_v1");

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
    pub role: UserRole,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToken {
    pub token: String,
    pub user_id: String,
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    user_id: String,
    expires_at: u64,
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
    IncompatibleSchema,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompatibleSchema => write!(f, "unsupported authentication database; use a fresh index database and create new accounts (migration is not supported)"),
            _ => write!(f, "{:?}", self),
        }
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
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        // This is a new authentication schema. Never deserialize or convert
        // accounts, credentials, or sessions from a previous implementation.
        if write_txn.list_tables().map_err(db_error)?.any(|table| {
            matches!(
                table.name(),
                "users" | "sessions" | "security_v1" | "opaque_credentials_v1"
            )
        }) {
            return Err(AuthError::IncompatibleSchema);
        }
        {
            let users = write_txn
                .open_table(USERS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            let sessions = write_txn
                .open_table(SESSIONS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            let records = write_txn.open_table(OPAQUE_TABLE).map_err(db_error)?;
            let mut security = write_txn.open_table(SECURITY_TABLE).map_err(db_error)?;
            let profile = security
                .get("profile")
                .map_err(db_error)?
                .map(|v| v.value().to_vec());
            match profile.as_deref() {
                Some(value) if value == crate::security_protocol::PROFILE.as_bytes() => {
                    let setup =
                        security
                            .get("opaque_setup")
                            .map_err(db_error)?
                            .ok_or_else(|| {
                                db_error("OPAQUE setup is missing; restore a security backup")
                            })?;
                    opaque_ke::ServerSetup::<crate::security_protocol::Suite>::deserialize(
                        setup.value(),
                    )
                    .map_err(|_| db_error("corrupt OPAQUE setup; restore a security backup"))?;
                }
                Some(_) => return Err(AuthError::IncompatibleSchema),
                None => {
                    if security.len().map_err(db_error)? != 0
                        || users.len().map_err(db_error)? != 0
                        || records.len().map_err(db_error)? != 0
                        || sessions.len().map_err(db_error)? != 0
                    {
                        return Err(AuthError::IncompatibleSchema);
                    }
                    let setup = opaque_ke::ServerSetup::<crate::security_protocol::Suite>::new(
                        &mut rand08::rngs::OsRng,
                    );
                    security
                        .insert("opaque_setup", setup.serialize().as_slice())
                        .map_err(db_error)?;
                    security
                        .insert("profile", crate::security_protocol::PROFILE.as_bytes())
                        .map_err(db_error)?;
                }
            }
            if users.len().map_err(db_error)? != records.len().map_err(db_error)? {
                return Err(db_error(
                    "OPAQUE credential records are incomplete; restore a security backup",
                ));
            }
        }
        write_txn
            .commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn session_ttl(&self) -> Duration {
        self.session_ttl
    }

    pub fn ensure_superadmin(&self) -> Result<(), AuthError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn
            .open_table(USERS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;

        if table
            .len()
            .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
            > 0
        {
            return Ok(());
        }

        // No users, wait for setup
        Ok(())
    }

    pub fn has_any_user(&self) -> Result<bool, AuthError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn
            .open_table(USERS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(table.len().unwrap_or(0) > 0)
    }

    pub fn has_admin(&self) -> Result<bool, AuthError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn
            .open_table(USERS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;

        for item in table
            .iter()
            .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
        {
            let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
            let user: AuthUser = bincode::deserialize(item.1.value())
                .map_err(|e| AuthError::DbError(e.to_string()))?;
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

    pub fn create_user(
        &self,
        username: &str,
        password: &str,
        role: UserRole,
    ) -> Result<AuthUser, AuthError> {
        self.create_user_internal(username, password, role)
    }

    fn create_user_internal(
        &self,
        username: &str,
        password: &str,
        role: UserRole,
    ) -> Result<AuthUser, AuthError> {
        let username = username.trim();
        if username.is_empty() || username.len() > 128 {
            return Err(AuthError::InvalidUsername);
        }

        let txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn
                .open_table(USERS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;

            for item in table
                .iter()
                .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
            {
                let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                let user: AuthUser = bincode::deserialize(item.1.value())
                    .map_err(|e| AuthError::DbError(e.to_string()))?;
                if user.username.eq_ignore_ascii_case(username) {
                    return Err(AuthError::UserExists);
                }
            }

            let id = uuid::Uuid::new_v4().to_string();
            if password.is_empty() || password.len() > 1024 {
                return Err(AuthError::InvalidPassword);
            }
            let record = crate::security_protocol::enroll(
                &self.opaque_setup()?,
                id.as_bytes(),
                password.as_bytes(),
            )
            .map_err(db_error)?;
            txn.open_table(OPAQUE_TABLE)
                .map_err(db_error)?
                .insert(id.as_str(), record.as_slice())
                .map_err(db_error)?;
            let user = AuthUser {
                id: id.clone(),
                username: username.to_string(),
                role,
                disabled: false,
            };

            let bytes = bincode::serialize(&user).map_err(|e| AuthError::DbError(e.to_string()))?;
            table
                .insert(id.as_str(), bytes.as_slice())
                .map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;

        // Fetch to return
        self.get_user_by_username(username).map(|u| u.unwrap())
    }

    pub fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<AuthUser>, AuthError> {
        let user = match self.get_user_by_username(username)? {
            Some(u) => u,
            None => return Ok(None),
        };

        if user.disabled {
            return Ok(None);
        }

        Ok(if self.verify_opaque_password(&user.id, password)? {
            Some(user)
        } else {
            None
        })
    }

    pub fn create_session(&self, user_id: &str) -> Result<SessionToken, AuthError> {
        self.create_session_inner(user_id, None)
    }

    pub fn create_bound_session(
        &self,
        user_id: &str,
        record_hash: &[u8],
    ) -> Result<SessionToken, AuthError> {
        self.create_session_inner(user_id, Some(record_hash))
    }

    fn create_session_inner(
        &self,
        user_id: &str,
        record_hash: Option<&[u8]>,
    ) -> Result<SessionToken, AuthError> {
        let token_str = generate_token();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expires_at = now + self.session_ttl.as_secs();

        let session = SessionToken {
            token: token_str.clone(),
            user_id: user_id.to_string(),
            expires_at,
        };

        let txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            // Validate under the same write transaction that creates the session.
            // A concurrent password reset cannot mint a session from an old proof.
            let users = txn.open_table(USERS_TABLE).map_err(db_error)?;
            let value = users
                .get(user_id)
                .map_err(db_error)?
                .ok_or(AuthError::UserNotFound)?;
            let user: AuthUser = bincode::deserialize(value.value()).map_err(db_error)?;
            if user.disabled {
                return Err(AuthError::InvalidPassword);
            }
            if let Some(expected) = record_hash {
                let records = txn.open_table(OPAQUE_TABLE).map_err(db_error)?;
                let current = records
                    .get(user_id)
                    .map_err(db_error)?
                    .ok_or(AuthError::InvalidPassword)?;
                if Sha256::digest(current.value()).as_slice() != expected {
                    return Err(AuthError::InvalidPassword);
                }
            }
            let mut table = txn
                .open_table(SESSIONS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            let bytes = bincode::serialize(&StoredSession {
                user_id: session.user_id.clone(),
                expires_at,
            })
            .map_err(|e| AuthError::DbError(e.to_string()))?;
            table
                .insert(token_digest(&token_str).as_str(), bytes.as_slice())
                .map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;

        Ok(session)
    }

    pub fn revoke_session(&self, token: &str) -> Result<(), AuthError> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn
                .open_table(SESSIONS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            table
                .remove(token_digest(token).as_str())
                .map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn clear_sessions(&self) -> Result<(), AuthError> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn
                .open_table(SESSIONS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            remove_sessions(&mut table, None)?;
        }
        txn.commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn user_from_token(&self, token: &str) -> Result<Option<AuthUser>, AuthError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let sessions = read_txn
            .open_table(SESSIONS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;

        let session = match sessions
            .get(token_digest(token).as_str())
            .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
        {
            Some(v) => {
                let s: StoredSession = bincode::deserialize(v.value())
                    .map_err(|e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()))?;
                s
            }
            None => return Ok(None),
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if session.expires_at <= now {
            // Expired
            return Ok(None);
        }

        let users = read_txn
            .open_table(USERS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let user_result = users
            .get(session.user_id.as_str())
            .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
        match user_result {
            Some(v) => {
                let user: AuthUser = bincode::deserialize(v.value())
                    .map_err(|e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()))?;
                if user.disabled {
                    Ok(None)
                } else {
                    Ok(Some(user))
                }
            }
            None => Ok(None),
        }
    }

    pub fn get_user(&self, id: &str) -> Result<Option<AuthUser>, AuthError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn
            .open_table(USERS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let result = table
            .get(id)
            .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
        match result {
            Some(v) => Ok(Some(bincode::deserialize(v.value()).map_err(
                |e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()),
            )?)),
            None => Ok(None),
        }
    }

    pub fn get_user_by_username(&self, username: &str) -> Result<Option<AuthUser>, AuthError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn
            .open_table(USERS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        for item in table
            .iter()
            .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
        {
            let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
            let user: AuthUser = bincode::deserialize(item.1.value())
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            if user.username.eq_ignore_ascii_case(username) {
                return Ok(Some(user));
            }
        }
        Ok(None)
    }

    pub fn list_users(&self) -> Result<Vec<AuthUser>, AuthError> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let table = read_txn
            .open_table(USERS_TABLE)
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        let mut users = Vec::new();
        for item in table
            .iter()
            .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
        {
            let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
            let user: AuthUser = bincode::deserialize(item.1.value())
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            users.push(user);
        }
        Ok(users)
    }

    pub fn update_user(
        &self,
        id: &str,
        username: &str,
        password: Option<&str>,
        role: UserRole,
    ) -> Result<(), AuthError> {
        let username = username.trim();
        if username.is_empty() || username.len() > 128 {
            return Err(AuthError::InvalidUsername);
        }
        let txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn
                .open_table(USERS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            let mut user: AuthUser = {
                let user_result = table
                    .get(id)
                    .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                match user_result {
                    Some(v) => bincode::deserialize(v.value())
                        .map_err(|e: Box<bincode::ErrorKind>| AuthError::DbError(e.to_string()))?,
                    None => return Err(AuthError::UserNotFound),
                }
            };

            // Check username uniqueness if changed
            if !user.username.eq_ignore_ascii_case(username) {
                for item in table
                    .iter()
                    .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
                {
                    let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                    let u: AuthUser = bincode::deserialize(item.1.value())
                        .map_err(|e| AuthError::DbError(e.to_string()))?;
                    if u.id != id && u.username.eq_ignore_ascii_case(username) {
                        return Err(AuthError::UserExists);
                    }
                }
            }

            user.username = username.to_string();
            user.role = role;
            if let Some(pw) = password {
                if pw.is_empty() || pw.len() > 1024 {
                    return Err(AuthError::InvalidPassword);
                }
                let record = crate::security_protocol::enroll(
                    &self.opaque_setup()?,
                    id.as_bytes(),
                    pw.as_bytes(),
                )
                .map_err(db_error)?;
                txn.open_table(OPAQUE_TABLE)
                    .map_err(db_error)?
                    .insert(id, record.as_slice())
                    .map_err(db_error)?;
            }

            let bytes = bincode::serialize(&user).map_err(|e| AuthError::DbError(e.to_string()))?;
            table
                .insert(id, bytes.as_slice())
                .map_err(|e| AuthError::DbError(e.to_string()))?;
        }
        self.revoke_user_sessions_in(&txn, id)?;
        txn.commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn update_password(&self, id: &str, password: &str) -> Result<(), AuthError> {
        if password.is_empty() || password.len() > 1024 {
            return Err(AuthError::InvalidPassword);
        }
        let record = crate::security_protocol::enroll(
            &self.opaque_setup()?,
            id.as_bytes(),
            password.as_bytes(),
        )
        .map_err(db_error)?;
        let txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let table = txn
                .open_table(USERS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            if table.get(id).map_err(db_error)?.is_none() {
                return Err(AuthError::UserNotFound);
            }
            txn.open_table(OPAQUE_TABLE)
                .map_err(db_error)?
                .insert(id, record.as_slice())
                .map_err(db_error)?;
        }
        self.revoke_user_sessions_in(&txn, id)?;
        txn.commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn delete_user(&self, id: &str) -> Result<(), AuthError> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        {
            let mut table = txn
                .open_table(USERS_TABLE)
                .map_err(|e| AuthError::DbError(e.to_string()))?;

            // Check if last admin
            let mut admin_count = 0;
            let mut target_is_admin = false;
            for item in table
                .iter()
                .map_err(|e: StorageError| AuthError::DbError(e.to_string()))?
            {
                let item = item.map_err(|e: StorageError| AuthError::DbError(e.to_string()))?;
                let u: AuthUser = bincode::deserialize(item.1.value())
                    .map_err(|e| AuthError::DbError(e.to_string()))?;
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

            table
                .remove(id)
                .map_err(|e| AuthError::DbError(e.to_string()))?;
            txn.open_table(OPAQUE_TABLE)
                .map_err(db_error)?
                .remove(id)
                .map_err(db_error)?;
        }
        self.revoke_user_sessions_in(&txn, id)?;
        txn.commit()
            .map_err(|e| AuthError::DbError(e.to_string()))?;
        Ok(())
    }

    pub fn opaque_setup(
        &self,
    ) -> Result<opaque_ke::ServerSetup<crate::security_protocol::Suite>, AuthError> {
        let txn = self.db.begin_read().map_err(db_error)?;
        let table = txn.open_table(SECURITY_TABLE).map_err(db_error)?;
        let value = table
            .get("opaque_setup")
            .map_err(db_error)?
            .ok_or_else(|| db_error("OPAQUE setup missing"))?;
        opaque_ke::ServerSetup::deserialize(value.value())
            .map_err(|_| db_error("corrupt OPAQUE setup; restore backup"))
    }

    pub fn opaque_record(&self, id: &str) -> Result<Option<Vec<u8>>, AuthError> {
        let txn = self.db.begin_read().map_err(db_error)?;
        let table = txn.open_table(OPAQUE_TABLE).map_err(db_error)?;
        let value = table.get(id).map_err(db_error)?.map(|v| v.value().to_vec());
        Ok(value)
    }

    fn verify_opaque_password(&self, id: &str, password: &str) -> Result<bool, AuthError> {
        use crate::security_protocol::{ksf, Suite, SERVER_IDENTITY};
        use opaque_ke::{
            ClientLogin, ClientLoginFinishParameters, Identifiers, ServerLogin,
            ServerLoginParameters, ServerRegistration,
        };
        let Some(record) = self.opaque_record(id)? else {
            return Ok(false);
        };
        let setup = self.opaque_setup()?;
        let mut rng = rand08::rngs::OsRng;
        let first = ClientLogin::<Suite>::start(&mut rng, password.as_bytes()).map_err(db_error)?;
        let response = ServerLogin::start(
            &mut rng,
            &setup,
            Some(ServerRegistration::deserialize(&record).map_err(db_error)?),
            first.message,
            id.as_bytes(),
            ServerLoginParameters {
                context: Some(b"phonolite-local-admin-v1"),
                identifiers: Identifiers {
                    client: None,
                    server: Some(SERVER_IDENTITY),
                },
            },
        )
        .map_err(db_error)?;
        let hash = ksf();
        let proof = first.state.finish(
            &mut rng,
            password.as_bytes(),
            response.message,
            ClientLoginFinishParameters::new(
                Some(b"phonolite-local-admin-v1"),
                Identifiers {
                    client: None,
                    server: Some(SERVER_IDENTITY),
                },
                Some(&hash),
            ),
        );
        Ok(proof.is_ok_and(|p| {
            response
                .state
                .finish(
                    p.message,
                    ServerLoginParameters {
                        context: Some(b"phonolite-local-admin-v1"),
                        identifiers: Identifiers {
                            client: None,
                            server: Some(SERVER_IDENTITY),
                        },
                    },
                )
                .is_ok()
        }))
    }

    fn revoke_user_sessions_in(
        &self,
        txn: &redb::WriteTransaction,
        id: &str,
    ) -> Result<(), AuthError> {
        let mut sessions = txn.open_table(SESSIONS_TABLE).map_err(db_error)?;
        remove_sessions(&mut sessions, Some(id))
    }
}

fn db_error(error: impl std::fmt::Display) -> AuthError {
    AuthError::DbError(error.to_string())
}
fn remove_sessions(
    table: &mut redb::Table<&str, &[u8]>,
    user_id: Option<&str>,
) -> Result<(), AuthError> {
    let mut keys = Vec::new();
    for entry in table.iter().map_err(db_error)? {
        let (key, value) = entry.map_err(db_error)?;
        if user_id.is_none()
            || bincode::deserialize::<StoredSession>(value.value())
                .map_or(true, |s| Some(s.user_id.as_str()) == user_id)
        {
            keys.push(key.value().to_string());
        }
    }
    for key in keys {
        table.remove(key.as_str()).map_err(db_error)?;
    }
    Ok(())
}
fn token_digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn generate_token() -> String {
    use rand08::RngCore;
    let mut bytes = [0u8; 32];
    rand08::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use redb::{Database, ReadableTable};

    use super::{AuthStore, UserRole};

    struct TestDb {
        path: PathBuf,
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn test_store() -> (AuthStore, TestDb) {
        let path =
            std::env::temp_dir().join(format!("phonolite-auth-test-{}.redb", uuid::Uuid::new_v4()));
        let db = Arc::new(Database::create(&path).unwrap());
        let store = AuthStore::new(db, Duration::from_secs(60));
        store.init_tables().unwrap();
        (store, TestDb { path })
    }

    fn set_disabled(store: &AuthStore, user_id: &str, disabled: bool) {
        let txn = store.db.begin_write().unwrap();
        {
            let mut table = txn.open_table(super::USERS_TABLE).unwrap();
            let bytes = table.get(user_id).unwrap().unwrap().value().to_vec();
            let mut user: super::AuthUser = bincode::deserialize(&bytes).unwrap();
            user.disabled = disabled;
            let bytes = bincode::serialize(&user).unwrap();
            table.insert(user_id, bytes.as_slice()).unwrap();
        }
        txn.commit().unwrap();
    }

    #[test]
    fn authenticate_and_session_round_trip() {
        let (store, _db) = test_store();
        let user = store.create_superadmin("admin", "hunter2").unwrap();

        let authenticated = store.authenticate("admin", "hunter2").unwrap().unwrap();
        let session = store.create_session(&user.id).unwrap();
        let from_token = store.user_from_token(&session.token).unwrap().unwrap();

        assert_eq!(authenticated.id, user.id);
        assert_eq!(from_token.username, "admin");
        assert_eq!(from_token.role, UserRole::SuperAdmin);
    }

    #[test]
    fn usernames_are_unique_case_insensitively() {
        let (store, _db) = test_store();
        store.create_superadmin("Admin", "hunter2").unwrap();

        let err = store
            .create_user("admin", "secret", UserRole::User)
            .unwrap_err();

        assert!(matches!(err, super::AuthError::UserExists));
    }

    #[test]
    fn update_password_preserves_username_and_role() {
        let (store, _db) = test_store();
        let user = store.create_superadmin("root", "old-password").unwrap();

        store.update_password(&user.id, "new-password").unwrap();

        let updated = store.get_user(&user.id).unwrap().unwrap();
        let authenticated = store.authenticate("root", "new-password").unwrap().unwrap();

        assert_eq!(updated.username, "root");
        assert_eq!(updated.role, UserRole::SuperAdmin);
        assert_eq!(authenticated.id, user.id);
    }

    #[test]
    fn disabled_users_are_rejected_by_authentication_and_session_lookup() {
        let (store, _db) = test_store();
        let user = store
            .create_user("listener", "secret", UserRole::User)
            .unwrap();
        let session = store.create_session(&user.id).unwrap();

        set_disabled(&store, &user.id, true);

        assert!(store.authenticate("listener", "secret").unwrap().is_none());
        assert!(store.user_from_token(&session.token).unwrap().is_none());
        assert!(!store.has_admin().unwrap());
    }

    #[test]
    fn delete_user_rejects_last_admin_but_allows_deletion_after_replacement() {
        let (store, _db) = test_store();
        let superadmin = store.create_superadmin("root", "secret").unwrap();

        let err = store.delete_user(&superadmin.id).unwrap_err();
        assert!(matches!(err, super::AuthError::LastAdmin));

        let backup = store
            .create_user("backup", "secret", UserRole::Admin)
            .unwrap();
        store.delete_user(&superadmin.id).unwrap();

        assert!(store.get_user(&superadmin.id).unwrap().is_none());
        assert_eq!(
            store.get_user(&backup.id).unwrap().unwrap().role,
            UserRole::Admin
        );
    }

    #[test]
    fn revoked_sessions_can_no_longer_be_used() {
        let (store, _db) = test_store();
        let user = store.create_superadmin("root", "secret").unwrap();
        let session = store.create_session(&user.id).unwrap();

        assert!(store.user_from_token(&session.token).unwrap().is_some());

        store.revoke_session(&session.token).unwrap();

        assert!(store.user_from_token(&session.token).unwrap().is_none());
    }

    #[test]
    fn session_storage_contains_only_a_token_digest_and_bulk_logout_works() {
        let (store, _db) = test_store();
        let user = store.create_superadmin("root", "secret").unwrap();
        let session = store.create_session(&user.id).unwrap();
        let read = store.db.begin_read().unwrap();
        let table = read.open_table(super::SESSIONS_TABLE).unwrap();
        assert!(table.get(session.token.as_str()).unwrap().is_none());
        let stored = table
            .get(super::token_digest(&session.token).as_str())
            .unwrap()
            .unwrap();
        let decoded: super::StoredSession = bincode::deserialize(stored.value()).unwrap();
        assert_eq!(decoded.user_id, user.id);
        assert!(!stored
            .value()
            .windows(session.token.len())
            .any(|bytes| bytes == session.token.as_bytes()));
        drop(stored);
        drop(table);
        drop(read);
        store.clear_sessions().unwrap();
        assert!(store.user_from_token(&session.token).unwrap().is_none());
    }

    #[test]
    fn previous_authentication_tables_are_rejected_without_modification() {
        for name in ["users", "sessions", "security_v1", "opaque_credentials_v1"] {
            let path = std::env::temp_dir().join(format!(
                "phonolite-incompatible-auth-{}.redb",
                uuid::Uuid::new_v4()
            ));
            let _cleanup = TestDb { path: path.clone() };
            let db = Arc::new(Database::create(&path).unwrap());
            let old: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new(name);
            let write = db.begin_write().unwrap();
            {
                write
                    .open_table(old)
                    .unwrap()
                    .insert("account", &b"unsupported data"[..])
                    .unwrap();
            }
            write.commit().unwrap();
            let store = AuthStore::new(db, Duration::from_secs(60));
            let error = store.init_tables().unwrap_err();
            assert!(matches!(error, super::AuthError::IncompatibleSchema));
            assert!(error.to_string().contains("fresh index database"));
            let read = store.db.begin_read().unwrap();
            assert_eq!(
                read.open_table(old)
                    .unwrap()
                    .get("account")
                    .unwrap()
                    .unwrap()
                    .value(),
                b"unsupported data"
            );
            assert!(read.open_table(super::USERS_TABLE).is_err());
            assert!(read.open_table(super::SECURITY_TABLE).is_err());
        }
    }

    #[test]
    fn new_schema_reopens_without_converting_or_clearing_sessions() {
        let (store, _db) = test_store();
        let user = store.create_superadmin("root", "secret").unwrap();
        let session = store.create_session(&user.id).unwrap();
        let setup = store.opaque_setup().unwrap().serialize();
        store.init_tables().unwrap();
        assert_eq!(store.opaque_setup().unwrap().serialize(), setup);
        assert_eq!(
            store.user_from_token(&session.token).unwrap().unwrap().id,
            user.id
        );
    }

    #[test]
    fn missing_or_unknown_profile_is_rejected_without_regeneration() {
        let (store, _db) = test_store();
        let write = store.db.begin_write().unwrap();
        {
            write
                .open_table(super::SECURITY_TABLE)
                .unwrap()
                .remove("profile")
                .unwrap();
        }
        write.commit().unwrap();
        assert!(matches!(
            store.init_tables(),
            Err(super::AuthError::IncompatibleSchema)
        ));
        let write = store.db.begin_write().unwrap();
        {
            write
                .open_table(super::SECURITY_TABLE)
                .unwrap()
                .insert("profile", &b"unsupported"[..])
                .unwrap();
        }
        write.commit().unwrap();
        assert!(matches!(
            store.init_tables(),
            Err(super::AuthError::IncompatibleSchema)
        ));
    }

    #[test]
    fn missing_setup_is_never_silently_regenerated_for_enrolled_users() {
        let (store, _db) = test_store();
        store.create_superadmin("root", "secret").unwrap();
        let write = store.db.begin_write().unwrap();
        {
            write
                .open_table(super::SECURITY_TABLE)
                .unwrap()
                .remove("opaque_setup")
                .unwrap();
        }
        write.commit().unwrap();
        assert!(store.init_tables().is_err());
    }
}
