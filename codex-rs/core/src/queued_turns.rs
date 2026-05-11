//! Core runtime support for durable queued turns.

use crate::StateDbHandle;
use crate::session::handlers;
use crate::session::session::Session;
use anyhow::Context;
use codex_app_server_protocol::TurnStartParams;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadQueuedTurnDispatchedEvent;
use codex_rollout::state_db::reconcile_rollout;
use codex_thread_store::LocalThreadStore;
use futures::future::BoxFuture;
use std::sync::Arc;
use uuid::Uuid;

impl Session {
    /// Starts the head queued turn when this session is still idle.
    pub(crate) fn maybe_start_thread_queued_turn_if_idle<'a>(
        self: &'a Arc<Self>,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let Some(state_db) = self.state_db_for_thread_queue().await? else {
                return Ok(());
            };
            let Some(queued_turn) = state_db
                .first_thread_queued_turn(self.conversation_id)
                .await?
            else {
                return Ok(());
            };
            let params: TurnStartParams =
                serde_json::from_str(queued_turn.turn_start_params_json.as_str())
                    .context("failed to decode queued turn params")?;
            let (turn_op, turn_has_input) = handlers::prepare_turn_start_op(self, params)
                .await
                .context("failed to prepare queued turn")?;
            let turn_id = Uuid::now_v7().to_string();
            if !handlers::maybe_start_queued_turn(self, turn_id.clone(), turn_op).await {
                return Ok(());
            }

            state_db
                .delete_thread_queued_turn(
                    self.conversation_id,
                    queued_turn.queued_turn_id.as_str(),
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to remove dispatched queued turn {}",
                        queued_turn.queued_turn_id
                    )
                })?;

            self.send_event_raw(Event {
                id: turn_id,
                msg: EventMsg::ThreadQueuedTurnDispatched(ThreadQueuedTurnDispatchedEvent {
                    thread_id: self.conversation_id,
                    turn_has_input,
                }),
            })
            .await;
            Ok(())
        })
    }

    async fn state_db_for_thread_queue(&self) -> anyhow::Result<Option<StateDbHandle>> {
        let config = self.get_config().await;
        if config.ephemeral {
            return Ok(None);
        }

        self.try_ensure_rollout_materialized()
            .await
            .context("failed to materialize rollout before opening state db for thread queue")?;

        let state_db = if let Some(state_db) = self.state_db() {
            state_db
        } else if let Some(local_store) = self
            .services
            .thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
        {
            local_store.state_db().await.ok_or_else(|| {
                anyhow::anyhow!(
                    "thread queue requires a local persisted thread with a state database"
                )
            })?
        } else {
            anyhow::bail!("thread queue requires a local persisted thread with a state database");
        };

        let thread_metadata_present = state_db
            .get_thread(self.conversation_id)
            .await
            .context("failed to read thread metadata before reconciling thread queue")?
            .is_some();
        if !thread_metadata_present {
            let rollout_path = self
                .current_rollout_path()
                .await
                .context("failed to locate rollout before reconciling thread queue")?
                .ok_or_else(|| {
                    anyhow::anyhow!("thread queue requires materialized thread metadata")
                })?;
            reconcile_rollout(
                Some(&state_db),
                rollout_path.as_path(),
                config.model_provider_id.as_str(),
                /*builder*/ None,
                &[],
                /*archived_only*/ None,
                /*new_thread_memory_mode*/ None,
            )
            .await;
        }

        Ok(Some(state_db))
    }
}
