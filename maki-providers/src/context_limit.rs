use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use async_lock::{Semaphore, SemaphoreGuardArc};
use flume::Sender;

use crate::AgentError;
use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::types::{Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};
use maki_config::providers::ProvidersConfig;
use maki_storage::id::SessionRef;
use serde_json::Value;

/// Holds a context-slot permit while a provider is actively generating.
/// Cloning shares the same underlying guard; the slot is released when the
/// last clone drops.
#[derive(Clone, Default)]
pub struct ContextPermit(Option<Arc<SemaphoreGuardArc>>);

impl fmt::Debug for ContextPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextPermit")
            .field("held", &self.0.is_some())
            .finish()
    }
}

/// Provider-level limiter for active model contexts/turns.
///
/// Local inference servers often keep exactly one model context loaded. This
/// limiter serializes the actual provider *requests* (not whole Agent runs),
/// so a parent run waiting for tool results does not block a subagent.
pub struct ContextLimiter {
    sems: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl ContextLimiter {
    fn new() -> Self {
        Self {
            sems: Mutex::new(HashMap::new()),
        }
    }

    pub fn global() -> &'static Self {
        static LIMITER: OnceLock<ContextLimiter> = OnceLock::new();
        LIMITER.get_or_init(Self::new)
    }

    /// Acquire one context slot for `provider`.
    ///
    /// Returns immediately if the provider has no `max_contexts` configured.
    /// Otherwise parks until a slot is free.
    pub async fn acquire(&self, provider: &str) -> Result<ContextPermit, AgentError> {
        let max = ProvidersConfig::load_or_default()
            .providers
            .get(provider)
            .and_then(|p| p.max_contexts)
            .filter(|&n| n > 0);
        let Some(n) = max else {
            return Ok(ContextPermit::default());
        };

        let sem = {
            let mut map = self.sems.lock().unwrap();
            map.entry(provider.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(n)))
                .clone()
        };

        let guard = sem.acquire_arc().await;
        Ok(ContextPermit(Some(Arc::new(guard))))
    }
}

/// Wraps a provider so every `stream_message` call acquires a context slot.
pub struct LimitedProvider {
    slug: String,
    inner: Arc<dyn Provider>,
}

impl LimitedProvider {
    pub fn wrap(provider: Box<dyn Provider>, slug: &str) -> Box<dyn Provider> {
        Box::new(Self {
            slug: slug.to_owned(),
            inner: Arc::from(provider),
        })
    }
}

impl Provider for LimitedProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&'a SessionRef>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let inner = Arc::clone(&self.inner);
        let slug = self.slug.clone();
        Box::pin(async move {
            let _permit = ContextLimiter::global().acquire(&slug).await?;
            inner
                .stream_message(model, messages, system, tools, event_tx, opts, session_id)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        self.inner.list_models()
    }

    fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
        self.inner.fetch_usage()
    }

    fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.inner.refresh_auth()
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        self.inner.reload_auth()
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        self.inner.rotate_key()
    }

    fn adjust_model(&self, model: &mut Model) {
        self.inner.adjust_model(model);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_permit_is_independent() {
        let p = ContextPermit::default();
        let _clone = p.clone();
    }

    #[test]
    fn limited_provider_blocks_at_capacity() {
        smol::block_on(async {
            let limiter = ContextLimiter::new();
            let p1 = limiter.acquire_with("test", Some(1)).await.unwrap();

            let mut second =
                smol::spawn(async move { limiter.acquire_with("test", Some(1)).await.unwrap() });
            assert!(
                futures_lite::future::poll_once(&mut second).await.is_none(),
                "second acquire should be parked"
            );

            drop(p1);
            let p2 = second.await;
            assert!(p2.0.is_some());
        });
    }

    #[test]
    fn different_providers_do_not_share_slots() {
        smol::block_on(async {
            let limiter = ContextLimiter::new();
            let a = limiter.acquire_with("a", Some(1)).await.unwrap();
            let b = limiter.acquire_with("b", Some(1)).await.unwrap();
            assert!(a.0.is_some());
            assert!(b.0.is_some());
        });
    }

    impl ContextLimiter {
        async fn acquire_with(
            &self,
            provider: &str,
            max: Option<usize>,
        ) -> Result<ContextPermit, AgentError> {
            let Some(n) = max.filter(|&n| n > 0) else {
                return Ok(ContextPermit::default());
            };
            let sem = {
                let mut map = self.sems.lock().unwrap();
                map.entry(provider.to_owned())
                    .or_insert_with(|| Arc::new(Semaphore::new(n)))
                    .clone()
            };
            let guard = sem.acquire_arc().await;
            Ok(ContextPermit(Some(Arc::new(guard))))
        }
    }
}
