use crate::realtime_conversation::handle_audio as handle_realtime_conversation_audio;
use crate::realtime_conversation::handle_close as handle_realtime_conversation_close;
use crate::realtime_conversation::handle_start as handle_realtime_conversation_start;
use crate::realtime_conversation::handle_text as handle_realtime_conversation_text;
use async_channel::Receiver;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::protocol::Submission;
use tracing::Instrument;
use tracing::debug_span;
use tracing::info_span;

use crate::session::SteerInputError;
use crate::session::session::Session;
use crate::session::session::SessionSettingsUpdate;
use crate::state::ActiveTurn;

use crate::config::Config;
use crate::config::ConfigOverrides;
use crate::realtime_context::REALTIME_TURN_TOKEN_BUDGET;
use crate::realtime_context::truncate_realtime_text_to_token_budget;
use crate::realtime_conversation::REALTIME_USER_TEXT_PREFIX;
use crate::realtime_conversation::prefix_realtime_v2_text;
use crate::review_prompts::resolve_review_request;
use crate::session::spawn_review_thread;
use crate::tasks::CompactTask;
use crate::tasks::UserShellCommandMode;
use crate::tasks::UserShellCommandTask;
use crate::tasks::execute_user_shell_command;
use codex_app_server_protocol::PermissionProfileModificationParams;
use codex_app_server_protocol::PermissionProfileSelectionParams;
use codex_app_server_protocol::TurnEnvironmentParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::McpServerRefreshConfig;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeConversationListVoicesResponseEvent;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_user_input::RequestUserInputResponse;

use crate::context_manager::is_user_turn_boundary;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::items::UserMessageItem;
use codex_protocol::mcp::RequestId as ProtocolRequestId;
use codex_protocol::user_input::UserInput;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use serde_json::Value;
use std::sync::Arc;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub async fn interrupt(sess: &Arc<Session>) {
    sess.interrupt_task().await;
}

pub async fn clean_background_terminals(sess: &Arc<Session>) {
    sess.close_unified_exec_processes().await;
}

pub async fn realtime_conversation_list_voices(sess: &Session, sub_id: String) {
    sess.send_event_raw(Event {
        id: sub_id,
        msg: EventMsg::RealtimeConversationListVoicesResponse(
            RealtimeConversationListVoicesResponseEvent {
                voices: RealtimeVoicesList::builtin(),
            },
        ),
    })
    .await;
}

pub async fn override_turn_context(sess: &Session, sub_id: String, updates: SessionSettingsUpdate) {
    if let Err(err) = sess.update_settings(updates).await {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
    }
}

pub async fn user_input_or_turn(sess: &Arc<Session>, sub_id: String, op: Op) {
    user_input_or_turn_inner(
        sess,
        sub_id,
        op,
        /*mirror_user_text_to_realtime*/ Some(()),
    )
    .await;
}

pub(super) async fn user_input_or_turn_inner(
    sess: &Arc<Session>,
    sub_id: String,
    op: Op,
    mirror_user_text_to_realtime: Option<()>,
) {
    let (items, updates, responsesapi_client_metadata) = prepare_user_input_or_turn(sess, op).await;

    let Ok(current_context) = sess.new_turn_with_sub_id(sub_id.clone(), updates).await else {
        // new_turn_with_sub_id already emits the error event.
        return;
    };
    sess.maybe_emit_unknown_model_warning_for_turn(current_context.as_ref())
        .await;
    let accepted_items = match sess
        .steer_input(
            items.clone(),
            /*expected_turn_id*/ None,
            responsesapi_client_metadata.clone(),
        )
        .await
    {
        Ok(_) => {
            current_context.session_telemetry.user_prompt(&items);
            Some(items)
        }
        Err(SteerInputError::NoActiveTurn(items)) => {
            start_regular_turn(
                sess,
                Arc::clone(&current_context),
                items,
                responsesapi_client_metadata,
            )
            .await
        }
        Err(err) => {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(err.to_error_event()),
            })
            .await;
            None
        }
    };
    if let (Some(items), Some(())) = (accepted_items, mirror_user_text_to_realtime) {
        self::mirror_user_text_to_realtime(sess, &items).await;
    }
}

pub(crate) async fn maybe_start_queued_turn(sess: &Arc<Session>, sub_id: String, op: Op) -> bool {
    let (items, updates, responsesapi_client_metadata) = prepare_user_input_or_turn(sess, op).await;
    let reserved_turn_state = {
        let mut active_turn = sess.active_turn.lock().await;
        if active_turn.is_some() {
            return false;
        }
        let active_turn = active_turn.get_or_insert_with(ActiveTurn::default);
        Arc::clone(&active_turn.turn_state)
    };
    let Ok(current_context) = sess.new_turn_with_sub_id(sub_id, updates).await else {
        clear_reserved_queued_turn(sess, &reserved_turn_state).await;
        return false;
    };
    sess.maybe_emit_unknown_model_warning_for_turn(current_context.as_ref())
        .await;
    let still_reserved = {
        let active_turn = sess.active_turn.lock().await;
        active_turn.as_ref().is_some_and(|active_turn| {
            active_turn.tasks.is_empty()
                && Arc::ptr_eq(&active_turn.turn_state, &reserved_turn_state)
        })
    };
    if !still_reserved {
        clear_reserved_queued_turn(sess, &reserved_turn_state).await;
        return false;
    }
    start_reserved_regular_turn(sess, current_context, items, responsesapi_client_metadata)
        .await
        .is_some()
}

pub(crate) async fn prepare_turn_start_op(
    sess: &Arc<Session>,
    params: TurnStartParams,
) -> codex_protocol::error::Result<(Op, bool)> {
    if params.thread_id != sess.conversation_id.to_string() {
        return Err(CodexErr::InvalidRequest(
            "turnStartParams.threadId must match the active thread".to_string(),
        ));
    }

    let collaboration_mode = params
        .collaboration_mode
        .map(normalize_turn_start_collaboration_mode);
    let environment_selections = parse_turn_environment_selections(sess, params.environments)?;
    let mapped_items = params
        .input
        .into_iter()
        .map(V2UserInput::into_core)
        .collect::<Vec<_>>();
    let turn_has_input = !mapped_items.is_empty();

    let has_any_overrides = params.cwd.is_some()
        || params.approval_policy.is_some()
        || params.approvals_reviewer.is_some()
        || params.sandbox_policy.is_some()
        || params.permissions.is_some()
        || params.model.is_some()
        || params.service_tier.is_some()
        || params.effort.is_some()
        || params.summary.is_some()
        || collaboration_mode.is_some()
        || params.personality.is_some();

    if params.sandbox_policy.is_some() && params.permissions.is_some() {
        return Err(CodexErr::InvalidRequest(
            "`permissions` cannot be combined with `sandboxPolicy`".to_string(),
        ));
    }

    let cwd = params.cwd;
    let approval_policy = params.approval_policy.map(|policy| policy.to_core());
    let approvals_reviewer = params.approvals_reviewer.map(|reviewer| reviewer.to_core());
    let sandbox_policy = params.sandbox_policy.map(|policy| policy.to_core());
    let (permission_profile, active_permission_profile) =
        resolve_turn_permission_selection(sess, cwd.clone(), params.permissions).await?;
    let model = params.model;
    let effort = params.effort.map(Some);
    let summary = params.summary;
    let service_tier = params.service_tier;
    let personality = params.personality;

    if has_any_overrides {
        validate_turn_start_settings(
            sess,
            cwd.clone(),
            approval_policy,
            approvals_reviewer,
            sandbox_policy.clone(),
            permission_profile.clone(),
            active_permission_profile.clone(),
            model.clone(),
            effort,
            summary,
            service_tier.clone(),
            collaboration_mode.clone(),
            personality,
        )
        .await?;
    }

    let turn_op = if has_any_overrides {
        Op::UserInputWithTurnContext {
            items: mapped_items,
            environments: environment_selections,
            final_output_json_schema: params.output_schema,
            responsesapi_client_metadata: params.responsesapi_client_metadata,
            cwd,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permission_profile,
            active_permission_profile,
            windows_sandbox_level: None,
            model,
            effort,
            summary,
            service_tier,
            collaboration_mode,
            personality,
        }
    } else {
        Op::UserInput {
            items: mapped_items,
            environments: environment_selections,
            final_output_json_schema: params.output_schema,
            responsesapi_client_metadata: params.responsesapi_client_metadata,
        }
    };

    Ok((turn_op, turn_has_input))
}

fn normalize_turn_start_collaboration_mode(
    mut collaboration_mode: CollaborationMode,
) -> CollaborationMode {
    if collaboration_mode.settings.developer_instructions.is_none()
        && let Some(instructions) = builtin_collaboration_mode_presets()
            .into_iter()
            .find(|preset| preset.mode == Some(collaboration_mode.mode))
            .and_then(|preset| preset.developer_instructions.flatten())
            .filter(|instructions| !instructions.is_empty())
    {
        collaboration_mode.settings.developer_instructions = Some(instructions);
    }

    collaboration_mode
}

fn parse_turn_environment_selections(
    sess: &Arc<Session>,
    environments: Option<Vec<TurnEnvironmentParams>>,
) -> codex_protocol::error::Result<Option<Vec<TurnEnvironmentSelection>>> {
    let environment_selections = environments.map(|environments| {
        environments
            .into_iter()
            .map(|environment| TurnEnvironmentSelection {
                environment_id: environment.environment_id,
                cwd: environment.cwd,
            })
            .collect::<Vec<_>>()
    });

    if let Some(environment_selections) = environment_selections.as_ref() {
        crate::environment_selection::resolve_environment_selections(
            sess.services.environment_manager.as_ref(),
            environment_selections,
        )?;
    }

    Ok(environment_selections)
}

async fn resolve_turn_permission_selection(
    sess: &Arc<Session>,
    cwd: Option<std::path::PathBuf>,
    permissions: Option<PermissionProfileSelectionParams>,
) -> codex_protocol::error::Result<(
    Option<codex_protocol::models::PermissionProfile>,
    Option<codex_protocol::models::ActivePermissionProfile>,
)> {
    let Some(permissions) = permissions else {
        return Ok((None, None));
    };

    let snapshot = sess.thread_config_snapshot().await;
    let config = sess.get_config().await;
    let mut overrides = ConfigOverrides {
        cwd: cwd.or_else(|| Some(snapshot.cwd.to_path_buf())),
        ..Default::default()
    };
    apply_permission_profile_selection_to_config_overrides(&mut overrides, permissions);
    let config = config.rebuild_with_overrides(overrides).await?;
    if let Some(warning) = config
        .startup_warnings
        .iter()
        .find(|warning| warning.contains("Configured value for `permission_profile` is disallowed"))
    {
        return Err(CodexErr::InvalidRequest(format!(
            "invalid turn context override: {warning}"
        )));
    }

    Ok((
        Some(config.permissions.permission_profile()),
        config.permissions.active_permission_profile(),
    ))
}

fn apply_permission_profile_selection_to_config_overrides(
    overrides: &mut ConfigOverrides,
    permissions: PermissionProfileSelectionParams,
) {
    let PermissionProfileSelectionParams::Profile { id, modifications } = permissions;
    overrides.default_permissions = Some(id);
    overrides
        .additional_writable_roots
        .extend(modifications.unwrap_or_default().into_iter().map(
            |modification| match modification {
                PermissionProfileModificationParams::AdditionalWritableRoot { path } => {
                    path.to_path_buf()
                }
            },
        ));
}

#[allow(clippy::too_many_arguments)]
async fn validate_turn_start_settings(
    sess: &Arc<Session>,
    cwd: Option<std::path::PathBuf>,
    approval_policy: Option<codex_protocol::protocol::AskForApproval>,
    approvals_reviewer: Option<codex_protocol::config_types::ApprovalsReviewer>,
    sandbox_policy: Option<codex_protocol::protocol::SandboxPolicy>,
    permission_profile: Option<codex_protocol::models::PermissionProfile>,
    active_permission_profile: Option<codex_protocol::models::ActivePermissionProfile>,
    model: Option<String>,
    effort: Option<Option<codex_protocol::openai_models::ReasoningEffort>>,
    summary: Option<codex_protocol::config_types::ReasoningSummary>,
    service_tier: Option<Option<String>>,
    collaboration_mode: Option<CollaborationMode>,
    personality: Option<codex_protocol::config_types::Personality>,
) -> codex_protocol::error::Result<()> {
    let collaboration_mode = if let Some(collaboration_mode) = collaboration_mode {
        collaboration_mode
    } else {
        sess.collaboration_mode()
            .await
            .with_updates(model, effort, /*developer_instructions*/ None)
    };

    sess.validate_settings(&SessionSettingsUpdate {
        cwd,
        approval_policy,
        approvals_reviewer,
        sandbox_policy,
        permission_profile,
        active_permission_profile,
        windows_sandbox_level: None,
        collaboration_mode: Some(collaboration_mode),
        reasoning_summary: summary,
        service_tier,
        personality,
        ..Default::default()
    })
    .await
    .map_err(|err| CodexErr::InvalidRequest(format!("invalid turn context override: {err}")))
}

async fn prepare_user_input_or_turn(
    sess: &Arc<Session>,
    op: Op,
) -> (
    Vec<UserInput>,
    SessionSettingsUpdate,
    Option<std::collections::HashMap<String, String>>,
) {
    match op {
        Op::UserTurn {
            cwd,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permission_profile,
            model,
            effort,
            summary,
            service_tier,
            final_output_json_schema,
            items,
            collaboration_mode,
            personality,
            environments,
        } => {
            let collaboration_mode = collaboration_mode.or_else(|| {
                Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: model.clone(),
                        reasoning_effort: effort,
                        developer_instructions: None,
                    },
                })
            });
            (
                items,
                SessionSettingsUpdate {
                    cwd: Some(cwd),
                    approval_policy: Some(approval_policy),
                    approvals_reviewer,
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    active_permission_profile: None,
                    windows_sandbox_level: None,
                    collaboration_mode,
                    reasoning_summary: summary,
                    service_tier,
                    final_output_json_schema: Some(final_output_json_schema),
                    environments,
                    personality,
                    app_server_client_name: None,
                    app_server_client_version: None,
                },
                None,
            )
        }
        Op::UserInputWithTurnContext {
            cwd,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permission_profile,
            active_permission_profile,
            windows_sandbox_level,
            model,
            effort,
            summary,
            service_tier,
            final_output_json_schema,
            items,
            responsesapi_client_metadata,
            collaboration_mode,
            personality,
            environments,
        } => {
            let collaboration_mode = if let Some(collab_mode) = collaboration_mode {
                Some(collab_mode)
            } else {
                let state = sess.state.lock().await;
                Some(
                    state
                        .session_configuration
                        .collaboration_mode
                        .with_updates(model, effort, /*developer_instructions*/ None),
                )
            };
            (
                items,
                SessionSettingsUpdate {
                    cwd,
                    approval_policy,
                    approvals_reviewer,
                    sandbox_policy,
                    permission_profile,
                    active_permission_profile,
                    windows_sandbox_level,
                    collaboration_mode,
                    reasoning_summary: summary,
                    service_tier,
                    final_output_json_schema: Some(final_output_json_schema),
                    environments,
                    personality,
                    app_server_client_name: None,
                    app_server_client_version: None,
                },
                responsesapi_client_metadata,
            )
        }
        Op::UserInput {
            items,
            environments,
            final_output_json_schema,
            responsesapi_client_metadata,
        } => (
            items,
            SessionSettingsUpdate {
                final_output_json_schema: Some(final_output_json_schema),
                environments,
                ..Default::default()
            },
            responsesapi_client_metadata,
        ),
        _ => unreachable!(),
    }
}

async fn start_regular_turn(
    sess: &Arc<Session>,
    current_context: Arc<crate::session::turn_context::TurnContext>,
    items: Vec<UserInput>,
    responsesapi_client_metadata: Option<std::collections::HashMap<String, String>>,
) -> Option<Vec<UserInput>> {
    if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
        current_context
            .turn_metadata_state
            .set_responsesapi_client_metadata(responsesapi_client_metadata);
    }
    current_context.session_telemetry.user_prompt(&items);
    sess.refresh_mcp_servers_if_requested(&current_context, Some(sess.mcp_elicitation_reviewer()))
        .await;
    let accepted_items = items.clone();
    sess.spawn_task(current_context, items, crate::tasks::RegularTask::new())
        .await;
    Some(accepted_items)
}

async fn start_reserved_regular_turn(
    sess: &Arc<Session>,
    current_context: Arc<crate::session::turn_context::TurnContext>,
    items: Vec<UserInput>,
    responsesapi_client_metadata: Option<std::collections::HashMap<String, String>>,
) -> Option<Vec<UserInput>> {
    if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
        current_context
            .turn_metadata_state
            .set_responsesapi_client_metadata(responsesapi_client_metadata);
    }
    current_context.session_telemetry.user_prompt(&items);
    sess.refresh_mcp_servers_if_requested(&current_context, Some(sess.mcp_elicitation_reviewer()))
        .await;
    let accepted_items = items.clone();
    sess.start_task(current_context, items, crate::tasks::RegularTask::new())
        .await;
    Some(accepted_items)
}

async fn clear_reserved_queued_turn(
    sess: &Arc<Session>,
    reserved_turn_state: &Arc<tokio::sync::Mutex<crate::state::TurnState>>,
) {
    let mut active_turn = sess.active_turn.lock().await;
    if active_turn.as_ref().is_some_and(|active_turn| {
        active_turn.tasks.is_empty() && Arc::ptr_eq(&active_turn.turn_state, reserved_turn_state)
    }) {
        *active_turn = None;
    }
}

async fn mirror_user_text_to_realtime(sess: &Arc<Session>, items: &[UserInput]) {
    let text = UserMessageItem::new(items).message();
    if text.is_empty() {
        return;
    }
    let text = if sess.conversation.is_running_v2().await {
        prefix_realtime_v2_text(text, REALTIME_USER_TEXT_PREFIX)
    } else {
        text
    };
    let text = truncate_realtime_text_to_token_budget(&text, REALTIME_TURN_TOKEN_BUDGET);
    if text.is_empty() {
        return;
    }
    if sess.conversation.running_state().await.is_none() {
        return;
    }
    if let Err(err) = sess.conversation.text_in(text).await {
        debug!("failed to mirror user text to realtime conversation: {err}");
    }
}

/// Records an inter-agent assistant envelope, then lets the shared pending-work scheduler
/// decide whether an idle session should start a regular turn.
pub async fn inter_agent_communication(
    sess: &Arc<Session>,
    sub_id: String,
    communication: InterAgentCommunication,
) {
    let trigger_turn = communication.trigger_turn;
    sess.enqueue_mailbox_communication(communication);
    if trigger_turn {
        sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)
            .await;
    }
}

pub async fn run_user_shell_command(sess: &Arc<Session>, sub_id: String, command: String) {
    if let Some((turn_context, cancellation_token)) =
        sess.active_turn_context_and_cancellation_token().await
    {
        let session = Arc::clone(sess);
        tokio::spawn(async move {
            execute_user_shell_command(
                session,
                turn_context,
                command,
                cancellation_token,
                UserShellCommandMode::ActiveTurnAuxiliary,
            )
            .await;
        });
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    sess.spawn_task(
        Arc::clone(&turn_context),
        Vec::new(),
        UserShellCommandTask::new(command),
    )
    .await;
}

pub async fn resolve_elicitation(
    sess: &Arc<Session>,
    server_name: String,
    request_id: ProtocolRequestId,
    decision: codex_protocol::approvals::ElicitationAction,
    content: Option<Value>,
    meta: Option<Value>,
) {
    let action = match decision {
        codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
        codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
        codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
    };
    let content = match action {
        // Preserve the legacy fallback for clients that only send an action.
        ElicitationAction::Accept => Some(content.unwrap_or_else(|| serde_json::json!({}))),
        ElicitationAction::Decline | ElicitationAction::Cancel => None,
    };
    let response = ElicitationResponse {
        action,
        content,
        meta,
    };
    let request_id = match request_id {
        ProtocolRequestId::String(value) => {
            rmcp::model::NumberOrString::String(std::sync::Arc::from(value))
        }
        ProtocolRequestId::Integer(value) => rmcp::model::NumberOrString::Number(value),
    };
    if let Err(err) = sess
        .resolve_elicitation(server_name, request_id, response)
        .await
    {
        warn!(
            error = %err,
            "failed to resolve elicitation request in session"
        );
    }
}

/// Propagate a user's exec approval decision to the session.
/// Also optionally applies an execpolicy amendment.
pub async fn exec_approval(
    sess: &Arc<Session>,
    approval_id: String,
    turn_id: Option<String>,
    decision: ReviewDecision,
) {
    let event_turn_id = turn_id.unwrap_or_else(|| approval_id.clone());
    if let ReviewDecision::ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment,
    } = &decision
    {
        match sess
            .persist_execpolicy_amendment(proposed_execpolicy_amendment)
            .await
        {
            Ok(()) => {
                sess.record_execpolicy_amendment_message(
                    &event_turn_id,
                    proposed_execpolicy_amendment,
                )
                .await;
            }
            Err(err) => {
                let message = format!("Failed to apply execpolicy amendment: {err}");
                tracing::warn!("{message}");
                let warning = EventMsg::Warning(WarningEvent { message });
                sess.send_event_raw(Event {
                    id: event_turn_id.clone(),
                    msg: warning,
                })
                .await;
            }
        }
    }
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&approval_id, other).await,
    }
}

pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
    match decision {
        ReviewDecision::Abort => {
            sess.interrupt_task().await;
        }
        other => sess.notify_approval(&id, other).await,
    }
}

pub async fn request_user_input_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestUserInputResponse,
) {
    sess.notify_user_input_response(&id, response).await;
}

pub async fn request_permissions_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestPermissionsResponse,
) {
    sess.notify_request_permissions_response(&id, response)
        .await;
}

pub async fn dynamic_tool_response(sess: &Arc<Session>, id: String, response: DynamicToolResponse) {
    sess.notify_dynamic_tool_response(&id, response).await;
}

pub async fn refresh_mcp_servers(sess: &Arc<Session>, refresh_config: McpServerRefreshConfig) {
    let mut guard = sess.pending_mcp_server_refresh_config.lock().await;
    *guard = Some(refresh_config);
}

pub async fn reload_user_config(sess: &Arc<Session>) {
    sess.reload_user_config_layer().await;
}

pub async fn compact(sess: &Arc<Session>, sub_id: String) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

    sess.spawn_task(
        Arc::clone(&turn_context),
        vec![UserInput::Text {
            text: turn_context.compact_prompt().to_string(),
            // Compaction prompt is synthesized; no UI element ranges to preserve.
            text_elements: Vec::new(),
        }],
        CompactTask,
    )
    .await;
}

pub async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) {
    if num_turns == 0 {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "num_turns must be >= 1".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let has_active_turn = { sess.active_turn.lock().await.is_some() };
    if has_active_turn {
        sess.send_event_raw(Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: "Cannot rollback while a turn is in progress.".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
    let live_thread = match sess.live_thread_for_persistence("rollback thread") {
        Ok(live_thread) => live_thread,
        Err(_) => {
            sess.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "thread rollback requires persisted thread history".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }
    };
    if let Err(err) = live_thread.flush().await {
        sess.send_event_raw(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: format!("failed to flush thread persistence for rollback replay: {err}"),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
            }),
        })
        .await;
        return;
    }

    let stored_history = match live_thread.load_history(/*include_archived*/ false).await {
        Ok(history) => history,
        Err(err) => {
            sess.send_event_raw(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: format!("failed to load thread history for rollback replay: {err}"),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }
    };

    let rollback_event = ThreadRolledBackEvent { num_turns };
    let rollback_msg = EventMsg::ThreadRolledBack(rollback_event.clone());
    let replay_items = stored_history
        .items
        .into_iter()
        .chain(std::iter::once(RolloutItem::EventMsg(rollback_msg.clone())))
        .collect::<Vec<_>>();
    sess.apply_rollout_reconstruction(turn_context.as_ref(), replay_items.as_slice())
        .await;
    sess.recompute_token_usage(turn_context.as_ref()).await;

    sess.persist_rollout_items(&[RolloutItem::EventMsg(rollback_msg.clone())])
        .await;
    if let Err(err) = sess.flush_rollout().await {
        sess.send_event(
            turn_context.as_ref(),
            EventMsg::Warning(WarningEvent {
                message: format!(
                    "Rolled the thread back, but failed to save the rollback marker. Codex will continue retrying. Error: {err}"
                ),
            }),
        )
        .await;
    }

    sess.deliver_event_raw(Event {
        id: turn_context.sub_id.clone(),
        msg: rollback_msg,
    })
    .await;
}

pub(super) async fn persist_thread_memory_mode_update(
    sess: &Arc<Session>,
    mode: ThreadMemoryMode,
) -> anyhow::Result<()> {
    let live_thread = sess.live_thread_for_persistence("update thread memory mode")?;
    live_thread.persist().await?;
    live_thread.flush().await?;
    live_thread
        .update_memory_mode(mode, /*include_archived*/ false)
        .await?;
    live_thread.flush().await?;
    Ok(())
}

/// Persists thread-level memory mode metadata for the active session.
///
/// This does not involve the model and only affects whether the thread is
/// eligible for future memory generation.
pub async fn set_thread_memory_mode(sess: &Arc<Session>, sub_id: String, mode: ThreadMemoryMode) {
    if let Err(err) = persist_thread_memory_mode_update(sess, mode).await {
        warn!("Failed to persist thread memory mode update to rollout: {err}");
        let event = Event {
            id: sub_id,
            msg: EventMsg::Error(ErrorEvent {
                message: err.to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }
}

pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    let _ = sess.conversation.shutdown().await;
    sess.services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
    let mcp_shutdown = {
        let mut manager = sess.services.mcp_connection_manager.write().await;
        manager.begin_shutdown()
    };
    mcp_shutdown.await;
    sess.guardian_review_session.shutdown().await;
    info!("Shutting down Codex instance");
    let history = sess.clone_history().await;
    let turn_count = history
        .raw_items()
        .iter()
        .filter(|item| is_user_turn_boundary(item))
        .count();
    sess.services.session_telemetry.counter(
        "codex.conversation.turn.count",
        i64::try_from(turn_count).unwrap_or(0),
        &[],
    );

    // Gracefully flush and shutdown thread persistence on session end so tests
    // that inspect durable state do not race with the background writer.
    if let Some(live_thread) = sess.live_thread()
        && let Err(e) = live_thread.shutdown().await
    {
        warn!("failed to shutdown thread persistence: {e}");
        let event = Event {
            id: sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: "Failed to shutdown thread persistence".to_string(),
                codex_error_info: Some(CodexErrorInfo::Other),
            }),
        };
        sess.send_event_raw(event).await;
    }

    let event = Event {
        id: sub_id,
        msg: EventMsg::ShutdownComplete,
    };
    sess.services
        .rollout_thread_trace
        .record_protocol_event(&event.msg);
    sess.deliver_event_raw(event).await;
    sess.services
        .rollout_thread_trace
        .record_ended(codex_rollout_trace::RolloutStatus::Completed);
    true
}

pub async fn review(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    review_request: ReviewRequest,
) {
    let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
    sess.maybe_emit_unknown_model_warning_for_turn(turn_context.as_ref())
        .await;
    sess.refresh_mcp_servers_if_requested(&turn_context, Some(sess.mcp_elicitation_reviewer()))
        .await;
    match resolve_review_request(review_request, &turn_context.cwd) {
        Ok(resolved) => {
            spawn_review_thread(
                Arc::clone(sess),
                Arc::clone(config),
                turn_context.clone(),
                sub_id,
                resolved,
            )
            .await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event(&turn_context, event.msg).await;
        }
    }
}

pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<Submission>,
) {
    // To break out of this loop, send Op::Shutdown.
    while let Ok(sub) = rx_sub.recv().await {
        debug!(?sub, "Submission");
        let dispatch_span = submission_dispatch_span(&sub);
        let should_exit = async {
            match sub.op.clone() {
                Op::Interrupt => {
                    interrupt(&sess).await;
                    false
                }
                Op::CleanBackgroundTerminals => {
                    clean_background_terminals(&sess).await;
                    false
                }
                Op::RealtimeConversationStart(params) => {
                    if let Err(err) =
                        handle_realtime_conversation_start(&sess, sub.id.clone(), params).await
                    {
                        sess.send_event_raw(Event {
                            id: sub.id.clone(),
                            msg: EventMsg::Error(ErrorEvent {
                                message: err.to_string(),
                                codex_error_info: Some(CodexErrorInfo::Other),
                            }),
                        })
                        .await;
                    }
                    false
                }
                Op::RealtimeConversationAudio(params) => {
                    handle_realtime_conversation_audio(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationText(params) => {
                    handle_realtime_conversation_text(&sess, sub.id.clone(), params).await;
                    false
                }
                Op::RealtimeConversationClose => {
                    handle_realtime_conversation_close(&sess, sub.id.clone()).await;
                    false
                }
                Op::RealtimeConversationListVoices => {
                    realtime_conversation_list_voices(&sess, sub.id.clone()).await;
                    false
                }
                Op::OverrideTurnContext {
                    cwd,
                    approval_policy,
                    approvals_reviewer,
                    sandbox_policy,
                    permission_profile,
                    windows_sandbox_level,
                    model,
                    effort,
                    summary,
                    service_tier,
                    collaboration_mode,
                    personality,
                } => {
                    let collaboration_mode = if let Some(collab_mode) = collaboration_mode {
                        collab_mode
                    } else {
                        let state = sess.state.lock().await;
                        state.session_configuration.collaboration_mode.with_updates(
                            model.clone(),
                            effort,
                            /*developer_instructions*/ None,
                        )
                    };
                    override_turn_context(
                        &sess,
                        sub.id.clone(),
                        SessionSettingsUpdate {
                            cwd,
                            approval_policy,
                            approvals_reviewer,
                            sandbox_policy,
                            permission_profile,
                            windows_sandbox_level,
                            collaboration_mode: Some(collaboration_mode),
                            reasoning_summary: summary,
                            service_tier,
                            personality,
                            ..Default::default()
                        },
                    )
                    .await;
                    false
                }
                Op::UserInput { .. }
                | Op::UserInputWithTurnContext { .. }
                | Op::UserTurn { .. } => {
                    user_input_or_turn(&sess, sub.id.clone(), sub.op).await;
                    false
                }
                Op::InterAgentCommunication { communication } => {
                    inter_agent_communication(&sess, sub.id.clone(), communication).await;
                    false
                }
                Op::ExecApproval {
                    id: approval_id,
                    turn_id,
                    decision,
                } => {
                    exec_approval(&sess, approval_id, turn_id, decision).await;
                    false
                }
                Op::PatchApproval { id, decision } => {
                    patch_approval(&sess, id, decision).await;
                    false
                }
                Op::UserInputAnswer { id, response } => {
                    request_user_input_response(&sess, id, response).await;
                    false
                }
                Op::RequestPermissionsResponse { id, response } => {
                    request_permissions_response(&sess, id, response).await;
                    false
                }
                Op::DynamicToolResponse { id, response } => {
                    dynamic_tool_response(&sess, id, response).await;
                    false
                }
                Op::RefreshMcpServers { config } => {
                    refresh_mcp_servers(&sess, config).await;
                    false
                }
                Op::ReloadUserConfig => {
                    reload_user_config(&sess).await;
                    false
                }
                Op::Compact => {
                    compact(&sess, sub.id.clone()).await;
                    false
                }
                Op::ThreadRollback { num_turns } => {
                    thread_rollback(&sess, sub.id.clone(), num_turns).await;
                    false
                }
                Op::SetThreadMemoryMode { mode } => {
                    set_thread_memory_mode(&sess, sub.id.clone(), mode).await;
                    false
                }
                Op::RunUserShellCommand { command } => {
                    run_user_shell_command(&sess, sub.id.clone(), command).await;
                    false
                }
                Op::ResolveElicitation {
                    server_name,
                    request_id,
                    decision,
                    content,
                    meta,
                } => {
                    resolve_elicitation(&sess, server_name, request_id, decision, content, meta)
                        .await;
                    false
                }
                Op::Shutdown => shutdown(&sess, sub.id.clone()).await,
                Op::Review { review_request } => {
                    review(&sess, &config, sub.id.clone(), review_request).await;
                    false
                }
                Op::ApproveGuardianDeniedAction { event } => {
                    approve_guardian_denied_action(&sess, event).await;
                    false
                }
                _ => false, // Ignore unknown ops; enum is non_exhaustive to allow extensions.
            }
        }
        .instrument(dispatch_span)
        .await;
        if should_exit {
            break;
        }
    }
    // If the submission loop exits because the channel closed without an
    // explicit shutdown op, still run process teardown for child processes
    // owned by this session.
    sess.services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
    let mcp_shutdown = {
        let mut manager = sess.services.mcp_connection_manager.write().await;
        manager.begin_shutdown()
    };
    mcp_shutdown.await;
    // Also drain cached guardian state on this implicit shutdown path.
    sess.guardian_review_session.shutdown().await;
    debug!("Agent loop exited");
}

async fn approve_guardian_denied_action(sess: &Arc<Session>, event: GuardianAssessmentEvent) {
    if event.status != GuardianAssessmentStatus::Denied {
        warn!(
            review_id = event.id.as_str(),
            "ignoring approval for non-denied Guardian assessment"
        );
        return;
    }

    let approved_action = serde_json::json!({
        "action": &event.action,
        "outcome": "allowed",
    });
    let approved_action_json = match serde_json::to_string_pretty(&approved_action) {
        Ok(approved_action_json) => approved_action_json,
        Err(error) => {
            warn!(%error, review_id = event.id.as_str(), "failed to serialize approved Guardian action");
            return;
        }
    };
    let approval_prefix = crate::guardian::AUTO_REVIEW_DENIED_ACTION_APPROVAL_DEVELOPER_PREFIX;
    let text = format!(
        r#"{approval_prefix}

Treat this as approval to perform that exact action in the same context in which it was originally requested.
Do not assume this also authorizes similar operations with different payloads.

Approved action:
{approved_action_json}"#,
    );
    let items = vec![ResponseInputItem::Message {
        role: "developer".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
    }];

    if let Err(items) = sess.inject_response_items(items).await {
        sess.queue_response_items_for_next_turn(items).await;
    }
}

pub(super) fn submission_dispatch_span(sub: &Submission) -> tracing::Span {
    let op_name = sub.op.kind();
    let span_name = format!("op.dispatch.{op_name}");
    let dispatch_span = match &sub.op {
        Op::RealtimeConversationAudio(_) => {
            debug_span!(
                "submission_dispatch",
                otel.name = span_name.as_str(),
                submission.id = sub.id.as_str(),
                codex.op = op_name
            )
        }
        _ => info_span!(
            "submission_dispatch",
            otel.name = span_name.as_str(),
            submission.id = sub.id.as_str(),
            codex.op = op_name
        ),
    };
    if let Some(trace) = sub.trace.as_ref()
        && !set_parent_from_w3c_trace_context(&dispatch_span, trace)
    {
        warn!(
            submission.id = sub.id.as_str(),
            "ignoring invalid submission trace carrier"
        );
    }
    dispatch_span
}
