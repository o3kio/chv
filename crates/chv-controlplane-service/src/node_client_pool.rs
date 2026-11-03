use crate::node_client::{CircuitBreaker, NodeClient};
use chv_errors::ChvError;
use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct NodeClientPool {
    clients: DashMap<String, (NodeClient, Instant)>,
    breakers: DashMap<String, Arc<CircuitBreaker>>,
    ttl: Duration,
    /// Per-daemon bearer token attached to every client this pool creates
    /// (#662). `None` keeps direct/test constructions token-less; the
    /// production bootstrap always installs the configured token.
    auth_token: Option<Arc<str>>,
}

impl Default for NodeClientPool {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeClientPool {
    pub fn new() -> Self {
        Self {
            clients: DashMap::new(),
            breakers: DashMap::new(),
            ttl: Duration::from_secs(300),
            auth_token: None,
        }
    }

    /// Attach the configured agent bearer token to every client created by
    /// this pool (#662).
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        let token = token.trim();
        self.auth_token = (!token.is_empty()).then(|| Arc::<str>::from(token));
        self
    }

    pub async fn get_or_connect(
        &self,
        node_id: &str,
        socket_path: &Path,
    ) -> Result<NodeClient, ChvError> {
        // Atomically remove expired entries using remove_if to avoid TOCTOU race
        self.clients.remove_if(node_id, |_, (_, created_at)| {
            created_at.elapsed() >= self.ttl
        });

        // If a valid (non-expired) entry still exists, return it
        if let Some(entry) = self.clients.get(node_id) {
            return Ok(entry.0.clone());
        }

        let breaker = self
            .breakers
            .entry(node_id.to_string())
            .or_insert_with(|| Arc::new(CircuitBreaker::new()))
            .clone();
        let client = NodeClient::connect_with_breaker_and_token(
            socket_path,
            breaker,
            self.auth_token.as_deref(),
        )
        .await?;
        let inserted = self
            .clients
            .entry(node_id.to_string())
            .or_insert_with(|| (client.clone(), Instant::now()));
        Ok(inserted.0.clone())
    }

    pub fn evict(&self, node_id: &str) {
        self.clients.remove(node_id);
    }
}
