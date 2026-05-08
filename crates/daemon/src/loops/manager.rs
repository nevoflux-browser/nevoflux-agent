//! LoopManager — the daemon's facade for the /loop skill (spec §4 architecture).
//!
//! Phase 7 wires only the `time:<duration>` trigger; other trigger variants
//! land in later phases (event in 11, dynamic in 12, state in 19, AND/OR in 20).

use crate::event_bus::EventBus;
use crate::loops::events::LoopEvents;
use crate::loops::executor::IterationExecutor;
use crate::loops::expression::TriggerExpr;
use crate::loops::registry::LoopRegistry;
use crate::loops::scheduler::{LoopFireRequest, TriggerScheduler};
use crate::loops::tool_classes::{default_classes, parse_class_list, ToolClass};
use crate::loops::types::{LoopId, LoopRuntime};
use nevoflux_storage::connection::Database;
use nevoflux_storage::models::{current_timestamp, LoopRecord, LoopState};
use nevoflux_storage::repositories::LoopRepository;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct CreateLoopArgs {
    pub session_id: String,
    pub trigger_expr_text: String,
    pub prompt_text: Option<String>,
    pub wrapped_skill: Option<String>, // JSON {name, args}
    pub allowed_tool_classes: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct LoopManager {
    db: Database,
    registry: LoopRegistry,
    scheduler: TriggerScheduler,
    fire_tx: mpsc::Sender<LoopFireRequest>,
    events: Arc<LoopEvents>,
    event_bus: Option<Arc<EventBus>>,
}

impl LoopManager {
    /// Spawns the dispatcher task and returns the manager handle.
    /// The dispatcher consumes `LoopFireRequest`s emitted by triggers
    /// and routes them to `IterationExecutor::execute`, applying the
    /// drop-on-busy concurrency policy from spec §8.2.
    pub fn start(db: Database) -> Self {
        Self::start_with_bus(db, None)
    }

    /// Same as [`start`], but with an EventBus handle so the manager and its
    /// dispatcher emit `system:loop:*` events. If `bus` is `None`, all
    /// emissions are silent no-ops (used by unit tests that don't care).
    pub fn start_with_bus(db: Database, bus: Option<Arc<EventBus>>) -> Self {
        let (fire_tx, mut fire_rx) = mpsc::channel::<LoopFireRequest>(64);
        let registry = LoopRegistry::new();
        let scheduler = TriggerScheduler::new();
        let events = Arc::new(LoopEvents::new(bus.clone()));
        let executor = Arc::new(IterationExecutor::new_with_events(
            db.clone(),
            events.clone(),
        ));

        let registry_for_task = registry.clone();
        let executor_for_task = executor.clone();
        let events_for_task = events.clone();
        tokio::spawn(async move {
            while let Some(req) = fire_rx.recv().await {
                // §8.2: drop if currently running.
                let busy = registry_for_task
                    .with_mut(&req.loop_id, |rt| rt.current_iteration.is_some())
                    .unwrap_or(false);
                if busy {
                    let now = current_timestamp();
                    let _ = LoopRepository::new(&executor_for_task.database())
                        .increment_skipped(req.loop_id.as_ref(), now);
                    let session_id = registry_for_task
                        .with_mut(&req.loop_id, |rt| rt.session_id.clone())
                        .unwrap_or_default();
                    let count = LoopRepository::new(&executor_for_task.database())
                        .get(req.loop_id.as_ref())
                        .ok()
                        .flatten()
                        .map(|r| r.skipped_triggers)
                        .unwrap_or(0);
                    events_for_task
                        .trigger_dropped(&session_id, &req.loop_id, count)
                        .await;
                    continue;
                }

                let token = Arc::new(tokio_util::sync::CancellationToken::new());
                registry_for_task.with_mut(&req.loop_id, |rt| {
                    rt.current_iteration = Some(token.clone());
                });

                let _ = executor_for_task
                    .execute(req.loop_id.clone(), req.fire_reason)
                    .await;

                registry_for_task.with_mut(&req.loop_id, |rt| {
                    rt.current_iteration = None;
                });
            }
        });

        Self {
            db,
            registry,
            scheduler,
            fire_tx,
            events,
            event_bus: bus.clone(),
        }
    }

    pub fn registry(&self) -> &LoopRegistry {
        &self.registry
    }

    pub async fn create_loop(&self, args: CreateLoopArgs) -> Result<LoopId, String> {
        // XOR — also CHECK-enforced in sqlite, but check here for a clean error.
        if args.prompt_text.is_some() == args.wrapped_skill.is_some() {
            return Err("exactly one of prompt_text or wrapped_skill is required".into());
        }
        let expr = TriggerExpr::parse(&args.trigger_expr_text).map_err(|e| e.to_string())?;

        let classes: Vec<ToolClass> = match args.allowed_tool_classes.as_ref() {
            Some(list) => parse_class_list(list)?.into_iter().collect(),
            None => default_classes(),
        };
        let classes_str: Vec<String> = classes.iter().map(|c| c.as_str().to_string()).collect();

        let id = LoopId::generate();
        let now = current_timestamp();
        let rec = LoopRecord {
            id: id.0.clone(),
            session_id: args.session_id.clone(),
            trigger_expr: args.trigger_expr_text.clone(),
            prompt_text: args.prompt_text,
            wrapped_skill: args.wrapped_skill,
            allowed_tool_classes: classes_str,
            scratchpad: String::new(),
            state: LoopState::Pending,
            consecutive_failures: 0,
            skipped_triggers: 0,
            iteration_count: 0,
            created_at: now,
            updated_at: now,
        };
        LoopRepository::new(&self.db)
            .create(&rec)
            .map_err(|e| e.to_string())?;

        self.registry
            .insert(LoopRuntime::new(id.clone(), args.session_id.clone()));
        self.wire_trigger(&id, &expr);

        self.events
            .created(
                &args.session_id,
                &id,
                &rec.trigger_expr,
                rec.prompt_text.as_deref(),
                rec.wrapped_skill.as_deref(),
            )
            .await;

        Ok(id)
    }

    pub async fn cancel_loop(&self, id: &LoopId, force: bool) -> Result<(), String> {
        // Snapshot the session_id BEFORE we touch the registry — we still
        // need it for event emission after the runtime entry is removed.
        let session_id = self
            .registry
            .with_mut(id, |rt| rt.session_id.clone())
            .unwrap_or_default();

        let subs: Vec<String> = self
            .registry
            .with_mut(id, |rt| {
                let s = std::mem::take(&mut rt.subscription_ids);
                if force {
                    rt.cancel_token.cancel();
                }
                s
            })
            .unwrap_or_default();
        for sub in &subs {
            self.scheduler.unsubscribe(sub);
        }
        LoopRepository::new(&self.db)
            .update_state(id.as_ref(), LoopState::Cancelled, current_timestamp())
            .map_err(|e| e.to_string())?;

        self.events
            .state_changed(&session_id, id, "cancelled", "running", Some("user"))
            .await;
        self.events.cancelled(&session_id, id, "user", force).await;

        self.registry.remove(id);
        Ok(())
    }

    pub async fn list_by_session(&self, session_id: &str) -> Result<Vec<LoopRecord>, String> {
        LoopRepository::new(&self.db)
            .list_by_session(session_id)
            .map_err(|e| e.to_string())
    }

    /// Wire a trigger expression's subscriptions. Phase 7 handles only `time:<duration>`;
    /// other variants are left as warn-stubs for later phases.
    fn wire_trigger(&self, id: &LoopId, expr: &TriggerExpr) {
        match expr {
            TriggerExpr::Time(dur) => {
                let sub = self
                    .scheduler
                    .schedule_time(id.clone(), *dur, self.fire_tx.clone());
                self.registry
                    .with_mut(id, |rt| rt.subscription_ids.push(sub));
            }
            TriggerExpr::TimeDynamic => {
                tracing::warn!("time:dynamic not yet wired (Phase 12)");
            }
            TriggerExpr::Event(topic) => {
                let Some(bus) = self.event_bus.clone() else {
                    tracing::warn!("event:{} ignored — LoopManager has no EventBus handle", topic);
                    return;
                };
                match self.scheduler.schedule_event(id.clone(), topic.clone(), bus, self.fire_tx.clone()) {
                    Ok(sub) => {
                        self.registry.with_mut(id, |rt| rt.subscription_ids.push(sub));
                    }
                    Err(e) => {
                        tracing::warn!("event:{} subscription failed: {e}", topic);
                    }
                }
            }
            TriggerExpr::State { .. } => {
                tracing::warn!("state:* not yet wired (Phase 19)");
            }
            TriggerExpr::And(_) | TriggerExpr::Or(_) => {
                tracing::warn!("AND/OR combinators not yet wired (Phase 20)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nevoflux_storage::models::CreateSessionParams;
    use nevoflux_storage::Storage;
    use std::time::Duration;

    fn fresh() -> Storage {
        let s = Storage::open_in_memory().unwrap();
        s.sessions()
            .create(CreateSessionParams::new().with_id("s1").with_title("t"))
            .unwrap();
        s
    }

    #[tokio::test(start_paused = true)]
    async fn create_loop_persists_record() {
        let storage = fresh();
        let mgr = LoopManager::start(storage.database().clone());
        let id = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "time:5m".into(),
                prompt_text: Some("check".into()),
                wrapped_skill: None,
                allowed_tool_classes: None,
            })
            .await
            .unwrap();

        let rec = storage.loops().get(id.as_ref()).unwrap().unwrap();
        assert_eq!(rec.state, LoopState::Pending);
        assert_eq!(rec.trigger_expr, "time:5m");
        // default classes applied
        assert!(rec.allowed_tool_classes.contains(&"read".to_string()));
        assert!(rec
            .allowed_tool_classes
            .contains(&"scratchpad-write".to_string()));
        assert!(rec
            .allowed_tool_classes
            .contains(&"event-subscribe".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn create_loop_rejects_invalid_trigger() {
        let storage = fresh();
        let mgr = LoopManager::start(storage.database().clone());
        let err = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "garbage".into(),
                prompt_text: Some("p".into()),
                wrapped_skill: None,
                allowed_tool_classes: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("unknown atom") || err.to_lowercase().contains("garbage"));
    }

    #[tokio::test(start_paused = true)]
    async fn create_loop_rejects_xor_violation() {
        let storage = fresh();
        let mgr = LoopManager::start(storage.database().clone());

        let err = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "time:5m".into(),
                prompt_text: None,
                wrapped_skill: None,
                allowed_tool_classes: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("prompt_text or wrapped_skill"));

        let err = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "time:5m".into(),
                prompt_text: Some("p".into()),
                wrapped_skill: Some("{}".into()),
                allowed_tool_classes: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("prompt_text or wrapped_skill"));
    }

    #[tokio::test(start_paused = true)]
    async fn create_loop_then_one_iteration_fires() {
        let storage = fresh();
        let mgr = LoopManager::start(storage.database().clone());

        let id = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "time:1m".into(),
                prompt_text: Some("check".into()),
                wrapped_skill: None,
                allowed_tool_classes: None,
            })
            .await
            .unwrap();

        // Advance past the first 60s tick, then drive the runtime so the
        // scheduler task -> dispatcher channel -> executor chain drains.
        // With virtual time paused, we alternate `advance` + yield to
        // ensure every spawned task gets polled.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(61)).await;
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
        }

        let rec = storage.loops().get(id.as_ref()).unwrap().unwrap();
        assert!(
            rec.iteration_count >= 1,
            "iteration_count was {}",
            rec.iteration_count
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_loop_unsubscribes_triggers() {
        let storage = fresh();
        let mgr = LoopManager::start(storage.database().clone());

        let id = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "time:1m".into(),
                prompt_text: Some("p".into()),
                wrapped_skill: None,
                allowed_tool_classes: None,
            })
            .await
            .unwrap();

        mgr.cancel_loop(&id, false).await.unwrap();

        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(120)).await;
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
        }

        let rec = storage.loops().get(id.as_ref()).unwrap().unwrap();
        assert_eq!(rec.state, LoopState::Cancelled);
        assert_eq!(
            rec.iteration_count, 0,
            "no iterations should have fired after cancel"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn list_by_session_returns_session_loops() {
        let storage = fresh();
        let mgr = LoopManager::start(storage.database().clone());

        let _ = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "time:5m".into(),
                prompt_text: Some("p".into()),
                wrapped_skill: None,
                allowed_tool_classes: None,
            })
            .await
            .unwrap();
        let _ = mgr
            .create_loop(CreateLoopArgs {
                session_id: "s1".into(),
                trigger_expr_text: "time:10m".into(),
                prompt_text: Some("q".into()),
                wrapped_skill: None,
                allowed_tool_classes: None,
            })
            .await
            .unwrap();

        let list = mgr.list_by_session("s1").await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn event_trigger_fires_iteration_on_publish() {
        use crate::event_bus::types::{BusEvent, PublisherIdentity};
        use crate::event_bus::EventBus;
        use std::sync::Arc;

        let storage = fresh();
        let bus = Arc::new(EventBus::new());
        let mgr = LoopManager::start_with_bus(storage.database().clone(), Some(bus.clone()));

        let id = mgr.create_loop(CreateLoopArgs {
            session_id: "s1".into(),
            trigger_expr_text: "event:ui:test:click".into(),
            prompt_text: Some("p".into()),
            wrapped_skill: None,
            allowed_tool_classes: None,
        }).await.unwrap();

        bus.publish(BusEvent::ephemeral(
            "ui:test:click",
            serde_json::json!({}),
            PublisherIdentity::Internal,
        )).await.unwrap();

        // Real-time wait for the dispatcher + executor to run.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let rec = storage.loops().get(id.as_ref()).unwrap().unwrap();
        assert!(rec.iteration_count >= 1, "iteration_count was {}", rec.iteration_count);
    }
}
