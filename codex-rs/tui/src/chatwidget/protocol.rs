use super::*;

impl ChatWidget {
    pub(crate) fn handle_server_notification(
        &mut self,
        notification: ServerNotification,
        replay_kind: Option<ReplayKind>,
    ) {
        // Reject misrouted child updates before shared notification handling mutates parent state.
        if let ServerNotification::McpServerStatusUpdated(notification) = &notification
            && let (Some(notification_thread_id), Some(thread_id)) =
                (notification.thread_id.as_deref(), self.thread_id())
            && notification_thread_id != thread_id.to_string()
        {
            return;
        }

        let from_replay = replay_kind.is_some();
        let is_resume_initial_replay =
            matches!(replay_kind, Some(ReplayKind::ResumeInitialMessages));
        let is_retry_error = matches!(
            &notification,
            ServerNotification::Error(ErrorNotification {
                will_retry: true,
                ..
            })
        );
        if !is_resume_initial_replay && !is_retry_error {
            self.restore_retry_status_header_if_present();
        }
        match notification {
            ServerNotification::ThreadTokenUsageUpdated(notification) => {
                self.set_token_info(Some(token_usage_info_from_app_server(
                    notification.token_usage,
                )));
            }
            ServerNotification::ThreadNameUpdated(notification) => {
                match ThreadId::from_string(&notification.thread_id) {
                    Ok(thread_id) => {
                        self.on_thread_name_updated(thread_id, notification.thread_name)
                    }
                    Err(err) => {
                        tracing::warn!(
                            thread_id = notification.thread_id,
                            error = %err,
                            "ignoring app-server ThreadNameUpdated with invalid thread_id"
                        );
                    }
                }
            }
            ServerNotification::ThreadGoalUpdated(notification) => {
                self.on_thread_goal_updated(notification.goal, notification.turn_id);
            }
            ServerNotification::ThreadGoalCleared(notification) => {
                self.on_thread_goal_cleared(notification.thread_id.as_str());
            }
            ServerNotification::ThreadSettingsUpdated(notification) => {
                self.on_thread_settings_updated(notification);
            }
            ServerNotification::TurnStarted(notification) => {
                let turn_id = notification.turn.id;
                self.turn_lifecycle.last_turn_id = Some(turn_id.clone());
                if !matches!(replay_kind, Some(ReplayKind::ResumeInitialMessages)) {
                    self.bind_pending_remote_im_route_to_turn(&turn_id);
                }
                self.last_non_retry_error = None;
                if !matches!(replay_kind, Some(ReplayKind::ResumeInitialMessages)) {
                    let turn_route = self.remote_im_turn_routes.get(&turn_id);
                    if turn_route.is_some_and(|route| route.source_routed) {
                        crate::multi_ai_code_im_bridge::send_source_task_activity(
                            turn_route.map(|route| route.reply_id.as_str()),
                            turn_route.and_then(|route| route.task_id.as_deref()),
                        );
                    } else if let Some(task_id) =
                        turn_route.and_then(|route| route.task_id.as_deref())
                    {
                        crate::multi_ai_code_im_bridge::send_task_activity(task_id);
                    }
                    self.on_task_started();
                }
            }
            ServerNotification::TurnCompleted(notification) => {
                self.handle_turn_completed_notification(notification, replay_kind);
            }
            ServerNotification::ItemStarted(notification) => {
                self.handle_item_started_notification(notification, replay_kind.is_some());
            }
            ServerNotification::ItemCompleted(notification) => {
                self.handle_item_completed_notification(notification, replay_kind);
            }
            ServerNotification::AgentMessageDelta(notification) => {
                self.on_agent_message_delta(notification.delta);
            }
            ServerNotification::PlanDelta(notification) => self.on_plan_delta(notification.delta),
            ServerNotification::ReasoningSummaryTextDelta(notification) => {
                self.on_agent_reasoning_delta(notification.delta);
            }
            ServerNotification::ReasoningTextDelta(notification) => {
                if self.config.show_raw_agent_reasoning {
                    self.on_agent_reasoning_delta(notification.delta);
                }
            }
            ServerNotification::ReasoningSummaryPartAdded(_) => self.on_reasoning_section_break(),
            ServerNotification::TerminalInteraction(notification) => {
                self.on_terminal_interaction(notification.process_id, notification.stdin)
            }
            ServerNotification::CommandExecutionOutputDelta(notification) => {
                self.on_exec_command_output_delta(&notification.item_id, &notification.delta);
            }
            ServerNotification::FileChangeOutputDelta(notification) => {
                self.on_patch_apply_output_delta(notification.item_id, notification.delta);
            }
            ServerNotification::TurnDiffUpdated(notification) => {
                self.on_turn_diff(notification.diff)
            }
            ServerNotification::TurnPlanUpdated(notification) => {
                self.on_plan_update(UpdatePlanArgs {
                    explanation: notification.explanation,
                    plan: notification
                        .plan
                        .into_iter()
                        .map(|step| UpdatePlanItemArg {
                            step: step.step,
                            status: match step.status {
                                TurnPlanStepStatus::Pending => UpdatePlanItemStatus::Pending,
                                TurnPlanStepStatus::InProgress => UpdatePlanItemStatus::InProgress,
                                TurnPlanStepStatus::Completed => UpdatePlanItemStatus::Completed,
                            },
                        })
                        .collect(),
                })
            }
            ServerNotification::HookStarted(notification) => {
                self.on_hook_started(notification.run);
            }
            ServerNotification::HookCompleted(notification) => {
                self.on_hook_completed(notification.run);
            }
            ServerNotification::Error(notification) => {
                if notification.will_retry {
                    if !from_replay {
                        self.on_stream_error(
                            notification.error.message,
                            notification.error.additional_details,
                        );
                    }
                } else {
                    if !from_replay {
                        let turn_route =
                            self.remote_im_route_for_turn(notification.turn_id.as_str());
                        if turn_route.as_ref().is_some_and(|route| route.source_routed) {
                            crate::multi_ai_code_im_bridge::send_source_turn_error(
                                &notification.error.message,
                                Some(notification.turn_id.as_str()),
                                turn_route.as_ref().map(|route| route.reply_id.as_str()),
                                turn_route
                                    .as_ref()
                                    .and_then(|route| route.task_id.as_deref()),
                            );
                        } else if let Some(route) = turn_route {
                            crate::multi_ai_code_im_bridge::send_turn_error(
                                &notification.error.message,
                                Some(notification.turn_id.as_str()),
                                &route.reply_id,
                                route.task_id.as_deref(),
                            );
                        }
                    }
                    self.last_non_retry_error = Some((
                        notification.turn_id.clone(),
                        notification.error.message.clone(),
                    ));
                    self.handle_non_retry_error(
                        notification.error.message,
                        notification.error.codex_error_info,
                    );
                }
            }
            ServerNotification::SkillsChanged(_) => {
                self.refresh_skills_for_current_cwd(/*force_reload*/ true);
            }
            ServerNotification::ModelRerouted(_) => {}
            ServerNotification::ModelVerification(notification) => {
                self.on_app_server_model_verification(&notification.verifications)
            }
            ServerNotification::ModelSafetyBufferingUpdated(notification) => {
                self.on_model_safety_buffering_updated(notification, replay_kind)
            }
            ServerNotification::Warning(notification) => self.on_warning(notification.message),
            ServerNotification::GuardianWarning(notification) => {
                self.on_warning(notification.message)
            }
            ServerNotification::DeprecationNotice(notification) => {
                self.on_deprecation_notice(notification.summary, notification.details)
            }
            ServerNotification::ConfigWarning(notification) => self.on_warning(
                notification
                    .details
                    .map(|details| format!("{}: {details}", notification.summary))
                    .unwrap_or(notification.summary),
            ),
            ServerNotification::McpServerStatusUpdated(notification) => {
                self.on_mcp_server_status_updated(notification)
            }
            ServerNotification::ItemGuardianApprovalReviewStarted(notification) => {
                self.on_guardian_review_notification(
                    notification.review_id,
                    notification.turn_id,
                    notification.started_at_ms,
                    notification.review,
                    /*completion*/ None,
                    notification.action,
                );
            }
            ServerNotification::ItemGuardianApprovalReviewCompleted(notification) => {
                self.on_guardian_review_notification(
                    notification.review_id,
                    notification.turn_id,
                    notification.started_at_ms,
                    notification.review,
                    Some((notification.completed_at_ms, notification.decision_source)),
                    notification.action,
                );
            }
            ServerNotification::ThreadClosed(_) => {
                if !from_replay {
                    self.on_shutdown_complete();
                }
            }
            ServerNotification::ServerRequestResolved(_)
            | ServerNotification::AccountUpdated(_)
            | ServerNotification::AccountRateLimitsUpdated(_)
            | ServerNotification::ThreadStarted(_)
            | ServerNotification::ThreadStatusChanged(_)
            | ServerNotification::ThreadArchived(_)
            | ServerNotification::ThreadDeleted(_)
            | ServerNotification::ThreadUnarchived(_)
            | ServerNotification::RawResponseItemCompleted(_)
            | ServerNotification::RawResponseCompleted(_)
            | ServerNotification::CommandExecOutputDelta(_)
            | ServerNotification::ProcessOutputDelta(_)
            | ServerNotification::ProcessExited(_)
            | ServerNotification::FileChangePatchUpdated(_)
            | ServerNotification::McpToolCallProgress(_)
            | ServerNotification::McpServerOauthLoginCompleted(_)
            | ServerNotification::AppListUpdated(_)
            | ServerNotification::EnvironmentConnected(_)
            | ServerNotification::EnvironmentDisconnected(_)
            | ServerNotification::RemoteControlStatusChanged(_)
            | ServerNotification::ExternalAgentConfigImportProgress(_)
            | ServerNotification::ExternalAgentConfigImportCompleted(_)
            | ServerNotification::FsChanged(_)
            | ServerNotification::TurnModerationMetadata(_)
            | ServerNotification::FuzzyFileSearchSessionUpdated(_)
            | ServerNotification::FuzzyFileSearchSessionCompleted(_)
            | ServerNotification::ThreadRealtimeStarted(_)
            | ServerNotification::ThreadRealtimeItemAdded(_)
            | ServerNotification::ThreadRealtimeOutputAudioDelta(_)
            | ServerNotification::ThreadRealtimeError(_)
            | ServerNotification::ThreadRealtimeClosed(_)
            | ServerNotification::ThreadRealtimeSdp(_)
            | ServerNotification::ThreadRealtimeTranscriptDelta(_)
            | ServerNotification::ThreadRealtimeTranscriptDone(_)
            | ServerNotification::WindowsWorldWritableWarning(_)
            | ServerNotification::WindowsSandboxSetupCompleted(_)
            | ServerNotification::AccountLoginCompleted(_) => {}
            ServerNotification::ContextCompacted(_) => {}
        }
    }

    pub(super) fn handle_turn_completed_notification(
        &mut self,
        notification: TurnCompletedNotification,
        replay_kind: Option<ReplayKind>,
    ) {
        let completed_turn_id = notification.turn.id.clone();
        let terminal_status = !matches!(notification.turn.status, TurnStatus::InProgress);
        // User-message dedupe only suppresses the app-server echo of a prompt
        // this TUI already rendered locally. Once that turn ends, another
        // client can submit the same text and it still needs its own user cell.
        self.last_rendered_user_message_display = None;
        match notification.turn.status {
            TurnStatus::Completed => {
                let remote_im_route = self.remote_im_route_for_turn(notification.turn.id.as_str());
                let source_routed = remote_im_route
                    .as_ref()
                    .is_some_and(|route| route.source_routed);
                let active_goal_continuing = self
                    .current_goal_status
                    .as_ref()
                    .is_some_and(GoalStatusState::is_active);
                let remote_im_reply_id =
                    remote_im_route.as_ref().map(|route| route.reply_id.clone());
                let remote_im_task_id = remote_im_route
                    .as_ref()
                    .and_then(|route| route.task_id.clone());
                let last_agent_message =
                    notification
                        .turn
                        .items
                        .iter()
                        .rev()
                        .find_map(|item| match item {
                            ThreadItem::AgentMessage {
                                id,
                                text,
                                phase: Some(MessagePhase::FinalAnswer) | None,
                                ..
                            } => Some((item.clone(), id.clone(), text.clone())),
                            _ => None,
                        });
                if let Some((item, id, _)) = &last_agent_message
                    && self
                        .transcript
                        .last_completed_agent_message
                        .as_ref()
                        .is_none_or(|(turn_id, item_id)| {
                            turn_id != &notification.turn.id || item_id != id
                        })
                {
                    self.handle_thread_item(
                        item.clone(),
                        notification.turn.id.clone(),
                        replay_kind
                            .map_or(ThreadItemRenderSource::Live, ThreadItemRenderSource::Replay),
                    );
                }
                if replay_kind.is_none() {
                    if source_routed {
                        if let Some((_, id, text)) = &last_agent_message {
                            crate::multi_ai_code_im_bridge::send_source_assistant_final(
                                text,
                                Some(id.as_str()),
                                remote_im_reply_id.as_deref(),
                                remote_im_task_id.as_deref(),
                            );
                        } else {
                            crate::multi_ai_code_im_bridge::send_source_turn_error(
                                "Codex turn completed without a final assistant response.",
                                Some(notification.turn.id.as_str()),
                                remote_im_reply_id.as_deref(),
                                remote_im_task_id.as_deref(),
                            );
                        }
                    } else if active_goal_continuing && remote_im_reply_id.is_some() {
                        if let Some((_, id, text)) = &last_agent_message {
                            crate::multi_ai_code_im_bridge::send_assistant_text(
                                text,
                                Some(id.as_str()),
                                remote_im_task_id.as_deref(),
                            );
                        } else if let Some(task_id) = remote_im_task_id.as_deref() {
                            crate::multi_ai_code_im_bridge::send_task_activity(task_id);
                        }
                    } else if let Some(reply_id) = remote_im_reply_id {
                        if let Some((_, id, text)) = &last_agent_message {
                            crate::multi_ai_code_im_bridge::send_assistant_final(
                                text,
                                Some(id.as_str()),
                                &reply_id,
                                remote_im_task_id.as_deref(),
                            );
                        } else {
                            crate::multi_ai_code_im_bridge::send_turn_error(
                                "Codex turn completed without a final assistant response.",
                                Some(notification.turn.id.as_str()),
                                &reply_id,
                                remote_im_task_id.as_deref(),
                            );
                        }
                    }
                }
                self.last_non_retry_error = None;
                self.on_task_complete(
                    last_agent_message.map(|(_, _, text)| text),
                    notification.turn.duration_ms,
                    replay_kind.is_some(),
                );
                if !source_routed
                    && !active_goal_continuing
                    && let Some(route) = remote_im_route.as_ref()
                {
                    self.clear_active_remote_im_route_if_matches(route);
                }
            }
            TurnStatus::Interrupted => {
                let remote_im_route = self.remote_im_route_for_turn(notification.turn.id.as_str());
                if remote_im_route
                    .as_ref()
                    .is_some_and(|route| route.source_routed)
                {
                    if replay_kind.is_none() {
                        crate::multi_ai_code_im_bridge::send_source_turn_error(
                            "Codex turn was interrupted.",
                            Some(notification.turn.id.as_str()),
                            remote_im_route
                                .as_ref()
                                .map(|route| route.reply_id.as_str()),
                            remote_im_route
                                .as_ref()
                                .and_then(|route| route.task_id.as_deref()),
                        );
                    }
                } else if let Some(route) = remote_im_route.as_ref() {
                    if replay_kind.is_none() {
                        crate::multi_ai_code_im_bridge::send_turn_error(
                            "Codex turn was interrupted.",
                            Some(notification.turn.id.as_str()),
                            &route.reply_id,
                            route.task_id.as_deref(),
                        );
                    }
                    self.clear_active_remote_im_route_if_matches(route);
                }
                self.last_non_retry_error = None;
                let reason = if self
                    .turn_lifecycle
                    .take_budget_limited(notification.turn.id.as_str())
                {
                    TurnAbortReason::BudgetLimited
                } else {
                    TurnAbortReason::Interrupted
                };
                self.on_interrupted_turn(reason);
            }
            TurnStatus::Failed => {
                let turn_id = notification.turn.id.clone();
                let remote_im_route = self.remote_im_route_for_turn(turn_id.as_str());
                let source_routed = remote_im_route
                    .as_ref()
                    .is_some_and(|route| route.source_routed);
                let remote_im_reply_id = remote_im_route
                    .as_ref()
                    .map(|route| route.reply_id.as_str());
                let remote_im_task_id = remote_im_route
                    .as_ref()
                    .and_then(|route| route.task_id.as_deref());
                if let Some(error) = notification.turn.error {
                    if self.last_non_retry_error.as_ref()
                        == Some(&(notification.turn.id.clone(), error.message.clone()))
                    {
                        self.last_non_retry_error = None;
                    } else {
                        if replay_kind.is_none() {
                            if source_routed {
                                crate::multi_ai_code_im_bridge::send_source_turn_error(
                                    &error.message,
                                    Some(turn_id.as_str()),
                                    remote_im_reply_id,
                                    remote_im_task_id,
                                );
                            } else if let Some(reply_id) = remote_im_reply_id {
                                crate::multi_ai_code_im_bridge::send_turn_error(
                                    &error.message,
                                    Some(turn_id.as_str()),
                                    reply_id,
                                    remote_im_task_id,
                                );
                            }
                        }
                        self.handle_non_retry_error(error.message, error.codex_error_info);
                    }
                } else {
                    if replay_kind.is_none() {
                        if source_routed {
                            crate::multi_ai_code_im_bridge::send_source_turn_error(
                                "Codex turn failed without an error message.",
                                Some(turn_id.as_str()),
                                remote_im_reply_id,
                                remote_im_task_id,
                            );
                        } else if let Some(reply_id) = remote_im_reply_id {
                            crate::multi_ai_code_im_bridge::send_turn_error(
                                "Codex turn failed without an error message.",
                                Some(turn_id.as_str()),
                                reply_id,
                                remote_im_task_id,
                            );
                        }
                    }
                    self.last_non_retry_error = None;
                    self.finalize_turn();
                    self.request_redraw();
                    self.maybe_send_next_queued_input();
                }
                if !source_routed && let Some(route) = remote_im_route.as_ref() {
                    self.clear_active_remote_im_route_if_matches(route);
                }
            }
            TurnStatus::InProgress => {}
        }
        if terminal_status {
            self.finish_remote_im_turn_route(&completed_turn_id);
        }
    }

    fn handle_item_started_notification(
        &mut self,
        notification: ItemStartedNotification,
        from_replay: bool,
    ) {
        match notification.item {
            item @ ThreadItem::CommandExecution { .. } => self.on_command_execution_started(item),
            ThreadItem::FileChange { id: _, changes, .. } => {
                self.on_patch_apply_begin(file_update_changes_to_display(changes));
            }
            item @ ThreadItem::McpToolCall { .. } => self.on_mcp_tool_call_started(item),
            ThreadItem::WebSearch(item) => {
                self.on_web_search_begin(item.id);
            }
            ThreadItem::ImageGeneration(_) => {
                self.on_image_generation_begin();
            }
            ThreadItem::CollabAgentToolCall {
                id,
                tool,
                status,
                sender_thread_id,
                receiver_thread_ids,
                prompt,
                model,
                reasoning_effort,
                agents_states,
            } => self.on_collab_agent_tool_call(ThreadItem::CollabAgentToolCall {
                id,
                tool,
                status,
                sender_thread_id,
                receiver_thread_ids,
                prompt,
                model,
                reasoning_effort,
                agents_states,
            }),
            item @ ThreadItem::SubAgentActivity { .. } => self.on_sub_agent_activity(item),
            ThreadItem::EnteredReviewMode { review, .. } if !from_replay => {
                self.enter_review_mode_with_hint(review, /*from_replay*/ false);
            }
            _ => {}
        }
    }

    fn handle_item_completed_notification(
        &mut self,
        notification: ItemCompletedNotification,
        replay_kind: Option<ReplayKind>,
    ) {
        self.handle_thread_item(
            notification.item,
            notification.turn_id,
            replay_kind.map_or(ThreadItemRenderSource::Live, ThreadItemRenderSource::Replay),
        );
    }
}
