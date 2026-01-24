//! Session manager for the daemon.

use crate::config::SessionConfig;
use crate::error::{DaemonError, Result};
use nevoflux_storage::{
    CreateMessageParams, CreateSessionParams, ListMessagesParams, ListSessionsParams, Message,
    MessageRole, Session, SessionMode, Storage, UpdateSessionParams,
};
use std::sync::Arc;

/// Manager for session lifecycle and operations.
pub struct SessionManager {
    /// Underlying storage.
    storage: Arc<Storage>,
    /// Session configuration.
    config: SessionConfig,
}

impl SessionManager {
    /// Create a session manager with the given storage path.
    pub fn new(db_path: &str) -> Result<Self> {
        let storage = Storage::open(db_path)?;
        Ok(Self {
            storage: Arc::new(storage),
            config: SessionConfig::default(),
        })
    }

    /// Create an in-memory session manager (for testing).
    pub fn in_memory() -> Result<Self> {
        let storage = Storage::open_in_memory()?;
        Ok(Self {
            storage: Arc::new(storage),
            config: SessionConfig::default(),
        })
    }

    /// Create a session manager with existing storage.
    pub fn with_storage(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            config: SessionConfig::default(),
        }
    }

    /// Set the configuration.
    pub fn with_config(mut self, config: SessionConfig) -> Self {
        self.config = config;
        self
    }

    /// Get the underlying storage.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Create a new session.
    ///
    /// If `session_id` is None, a new ID will be generated.
    /// If `title` is None, no title will be set initially.
    pub async fn create_session(
        &self,
        session_id: Option<String>,
        title: Option<String>,
    ) -> Result<Session> {
        let mut params = CreateSessionParams::new();

        if let Some(id) = session_id {
            params = params.with_id(id);
        }

        if let Some(t) = title {
            params = params.with_title(t);
        }

        let session = self.storage.sessions().create(params)?;
        Ok(session)
    }

    /// Create an agent session.
    pub async fn create_agent_session(
        &self,
        session_id: Option<String>,
        title: Option<String>,
    ) -> Result<Session> {
        let mut params = CreateSessionParams::new().with_mode(SessionMode::Agent);

        if let Some(id) = session_id {
            params = params.with_id(id);
        }

        if let Some(t) = title {
            params = params.with_title(t);
        }

        let session = self.storage.sessions().create(params)?;
        Ok(session)
    }

    /// Get a session by ID.
    pub async fn get_session(&self, session_id: &str) -> Result<Option<Session>> {
        Ok(self.storage.sessions().get(session_id)?)
    }

    /// Get a session, creating it if auto_create is enabled and it doesn't exist.
    pub async fn get_or_create_session(&self, session_id: &str) -> Result<Session> {
        if let Some(session) = self.get_session(session_id).await? {
            return Ok(session);
        }

        if self.config.auto_create {
            self.create_session(Some(session_id.to_string()), None)
                .await
        } else {
            Err(DaemonError::SessionNotFound(session_id.to_string()))
        }
    }

    /// List sessions.
    pub async fn list_sessions(&self, params: ListSessionsParams) -> Result<Vec<Session>> {
        Ok(self.storage.sessions().list(params)?)
    }

    /// Update a session.
    pub async fn update_session(
        &self,
        session_id: &str,
        params: UpdateSessionParams,
    ) -> Result<Session> {
        let session = self.storage.sessions().update(session_id, params)?;
        Ok(session)
    }

    /// Delete a session.
    pub async fn delete_session(&self, session_id: &str) -> Result<bool> {
        // Delete messages first
        self.storage.messages().delete_by_session(session_id)?;
        // Then delete the session
        Ok(self.storage.sessions().delete(session_id)?)
    }

    /// Touch a session (update its updated_at timestamp).
    pub async fn touch_session(&self, session_id: &str) -> Result<()> {
        self.storage.sessions().touch(session_id)?;
        Ok(())
    }

    /// Add a message to a session.
    pub async fn add_message(
        &self,
        session_id: &str,
        role: MessageRole,
        content: &str,
    ) -> Result<Message> {
        let params = CreateMessageParams::new(session_id, role, content);
        let message = self.storage.messages().create(params)?;

        // Touch the session
        self.touch_session(session_id).await.ok();

        Ok(message)
    }

    /// Get messages for a session.
    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let params = ListMessagesParams::new(session_id);
        Ok(self.storage.messages().list(params)?)
    }

    /// Get recent messages for a session (with limit).
    pub async fn get_recent_messages(&self, session_id: &str, limit: u32) -> Result<Vec<Message>> {
        let params = ListMessagesParams::new(session_id).with_limit(limit);
        Ok(self.storage.messages().list(params)?)
    }

    /// Get message count for a session.
    pub async fn get_message_count(&self, session_id: &str) -> Result<u32> {
        Ok(self.storage.messages().count(session_id)?)
    }

    /// Pin a session.
    pub async fn pin_session(&self, session_id: &str) -> Result<Session> {
        self.update_session(session_id, UpdateSessionParams::new().with_pinned(true))
            .await
    }

    /// Unpin a session.
    pub async fn unpin_session(&self, session_id: &str) -> Result<Session> {
        self.update_session(session_id, UpdateSessionParams::new().with_pinned(false))
            .await
    }

    /// Archive a session.
    pub async fn archive_session(&self, session_id: &str) -> Result<Session> {
        self.update_session(session_id, UpdateSessionParams::new().with_archived(true))
            .await
    }

    /// Unarchive a session.
    pub async fn unarchive_session(&self, session_id: &str) -> Result<Session> {
        self.update_session(session_id, UpdateSessionParams::new().with_archived(false))
            .await
    }

    /// Set session title.
    pub async fn set_title(&self, session_id: &str, title: &str) -> Result<Session> {
        self.update_session(session_id, UpdateSessionParams::new().with_title(title))
            .await
    }

    /// Generate a title from the first message.
    pub async fn generate_title(&self, session_id: &str) -> Result<Option<String>> {
        let messages = self.get_recent_messages(session_id, 1).await?;

        if let Some(first_message) = messages.first() {
            // Take first 50 chars as title
            let title: String = first_message.content.chars().take(50).collect();
            let title = title.trim().to_string();

            if !title.is_empty() {
                self.set_title(session_id, &title).await?;
                return Ok(Some(title));
            }
        }

        Ok(None)
    }

    /// Get session count.
    pub async fn get_session_count(&self, include_archived: bool) -> Result<u32> {
        Ok(self.storage.sessions().count(include_archived)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_manager_create_session() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        assert!(!session.id.is_empty());
        assert!(session.title.is_none());
    }

    #[tokio::test]
    async fn test_session_manager_create_session_with_id() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager
            .create_session(
                Some("custom-id".to_string()),
                Some("Test Title".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(session.id, "custom-id");
        assert_eq!(session.title, Some("Test Title".to_string()));
    }

    #[tokio::test]
    async fn test_session_manager_create_agent_session() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_agent_session(None, None).await.unwrap();
        assert_eq!(session.mode, SessionMode::Agent);
    }

    #[tokio::test]
    async fn test_session_manager_get_session() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        let retrieved = manager.get_session(&session.id).await.unwrap();

        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id, session.id);
    }

    #[tokio::test]
    async fn test_session_manager_get_or_create() {
        let manager = SessionManager::in_memory().unwrap();

        // Should create new session
        let session1 = manager.get_or_create_session("new-session").await.unwrap();
        assert_eq!(session1.id, "new-session");

        // Should return existing session
        let session2 = manager.get_or_create_session("new-session").await.unwrap();
        assert_eq!(session2.id, session1.id);
    }

    #[tokio::test]
    async fn test_session_manager_add_message() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        let message = manager
            .add_message(&session.id, MessageRole::User, "Hello!")
            .await
            .unwrap();

        assert_eq!(message.session_id, session.id);
        assert_eq!(message.role, MessageRole::User);
        assert_eq!(message.content, "Hello!");
    }

    #[tokio::test]
    async fn test_session_manager_get_messages() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        manager
            .add_message(&session.id, MessageRole::User, "Hello!")
            .await
            .unwrap();
        manager
            .add_message(&session.id, MessageRole::Assistant, "Hi!")
            .await
            .unwrap();

        let messages = manager.get_messages(&session.id).await.unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn test_session_manager_pin_unpin() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        assert!(!session.pinned);

        let pinned = manager.pin_session(&session.id).await.unwrap();
        assert!(pinned.pinned);

        let unpinned = manager.unpin_session(&session.id).await.unwrap();
        assert!(!unpinned.pinned);
    }

    #[tokio::test]
    async fn test_session_manager_archive() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        assert!(!session.archived);

        let archived = manager.archive_session(&session.id).await.unwrap();
        assert!(archived.archived);
    }

    #[tokio::test]
    async fn test_session_manager_set_title() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        let updated = manager.set_title(&session.id, "New Title").await.unwrap();

        assert_eq!(updated.title, Some("New Title".to_string()));
    }

    #[tokio::test]
    async fn test_session_manager_delete_session() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();
        manager
            .add_message(&session.id, MessageRole::User, "Hello")
            .await
            .unwrap();

        let deleted = manager.delete_session(&session.id).await.unwrap();
        assert!(deleted);

        let retrieved = manager.get_session(&session.id).await.unwrap();
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_session_manager_message_count() {
        let manager = SessionManager::in_memory().unwrap();

        let session = manager.create_session(None, None).await.unwrap();

        assert_eq!(manager.get_message_count(&session.id).await.unwrap(), 0);

        manager
            .add_message(&session.id, MessageRole::User, "1")
            .await
            .unwrap();
        manager
            .add_message(&session.id, MessageRole::User, "2")
            .await
            .unwrap();

        assert_eq!(manager.get_message_count(&session.id).await.unwrap(), 2);
    }
}
