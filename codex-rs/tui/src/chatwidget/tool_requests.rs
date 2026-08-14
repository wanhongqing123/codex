//! Interactive tool request surfaces for `ChatWidget`.
//!
//! This module owns approval, permission, elicitation, and user-input prompts
//! that block on user decisions.

use super::*;

impl ChatWidget {
    pub(super) fn on_exec_approval_request(&mut self, _id: String, ev: ExecApprovalRequestEvent) {
        if self.forward_exec_approval_to_remote_im(&ev) {
            return;
        }
        self.defer_or_handle(
            ev,
            InterruptManager::push_exec_approval,
            Self::handle_exec_approval_now,
        );
    }

    /// Returns true when remote delivery failed and the request was canceled
    /// fail-closed, in which case no local overlay should be installed.
    fn forward_exec_approval_to_remote_im(&mut self, ev: &ExecApprovalRequestEvent) -> bool {
        // An immutable turn route remains available until terminal cleanup so
        // output cannot be reassigned to another peer. It must not, however,
        // preserve approval authority after local TUI input has taken over.
        if !self.remote_im_forwarding_active {
            return false;
        }
        // Never route a security decision through the mutable "active" task.
        // A later prompt can arrive while this turn is blocked on approval, so
        // the immutable turn route is the authority-bearing correlation.
        let Some(route) = self.remote_im_turn_routes.get(&ev.turn_id).cloned() else {
            return false;
        };
        if !route.source_routed {
            return false;
        }
        let Some(task_id) = route.task_id.clone() else {
            return false;
        };
        let Some(thread_id) = self.thread_id else {
            return false;
        };
        let approval_id = ev.effective_approval_id();
        let pending = RemoteImPendingExecApproval {
            thread_id,
            turn_id: ev.turn_id.clone(),
            available_decisions: ev.effective_available_decisions(),
            reply_id: Some(route.reply_id.clone()),
            task_id: task_id.clone(),
        };
        // Register before handing the event to the async bridge. A fast IM reply
        // must never race ahead of the allow-list used to validate its decision.
        self.remote_im_pending_exec_approvals
            .insert(approval_id.clone(), pending);
        match crate::multi_ai_code_im_bridge::send_approval_request(
            thread_id,
            ev,
            Some(route.reply_id.as_str()),
            Some(&task_id),
        ) {
            crate::multi_ai_code_im_bridge::ReliableSendOutcome::Queued => return false,
            crate::multi_ai_code_im_bridge::ReliableSendOutcome::Unavailable => {
                tracing::warn!(
                    %thread_id,
                    turn_id = %ev.turn_id,
                    %task_id,
                    %approval_id,
                    "remote IM approval bridge is unavailable; keeping approval local"
                );
                return false;
            }
            crate::multi_ai_code_im_bridge::ReliableSendOutcome::Saturated => {}
        }

        self.remote_im_pending_exec_approvals.remove(&approval_id);
        tracing::error!(
            %thread_id,
            turn_id = %ev.turn_id,
            %task_id,
            %approval_id,
            "remote IM approval queue is saturated; canceling command execution fail-closed"
        );
        self.add_error_message(
            "远程审批消息无法可靠入队；为安全起见已自动取消本次命令。".to_string(),
        );
        self.app_event_tx.send(AppEvent::SubmitThreadOp {
            thread_id,
            op: AppCommand::exec_approval(
                approval_id,
                Some(ev.turn_id.clone()),
                CommandExecutionApprovalDecision::Cancel,
            ),
        });
        true
    }

    pub(crate) fn validate_remote_im_exec_approval(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        task_id: &str,
        approval_id: &str,
        decision: &CommandExecutionApprovalDecision,
    ) -> Result<(), String> {
        let Some(pending) = self.remote_im_pending_exec_approvals.get(approval_id) else {
            return Err("approval is no longer pending".to_string());
        };
        if pending.thread_id != thread_id {
            return Err("approval belongs to a different Codex thread".to_string());
        }
        if pending.turn_id != turn_id {
            return Err("approval belongs to a different Codex turn".to_string());
        }
        if pending.task_id != task_id {
            return Err("approval belongs to a different remote IM task".to_string());
        }
        if !pending.available_decisions.contains(decision) {
            return Err("approval decision is not available for this request".to_string());
        }
        Ok(())
    }

    pub(crate) fn note_remote_im_exec_approval_resolved(&mut self, approval_id: &str) {
        let Some(pending) = self.remote_im_pending_exec_approvals.remove(approval_id) else {
            return;
        };
        crate::multi_ai_code_im_bridge::send_approval_resolved(
            pending.thread_id,
            &pending.turn_id,
            approval_id,
            pending.reply_id.as_deref(),
            Some(&pending.task_id),
        );
    }

    pub(crate) fn invalidate_remote_im_exec_approvals(&mut self) {
        // A thread switch destroys the local approval authority. Tell the host
        // immediately so its one-time IM tokens do not remain clickable until
        // their ten-minute timeout.
        let pending = std::mem::take(&mut self.remote_im_pending_exec_approvals);
        for (approval_id, approval) in pending {
            crate::multi_ai_code_im_bridge::send_approval_resolved(
                approval.thread_id,
                &approval.turn_id,
                &approval_id,
                approval.reply_id.as_deref(),
                Some(&approval.task_id),
            );
        }
    }

    pub(super) fn on_apply_patch_approval_request(
        &mut self,
        _id: String,
        ev: ApplyPatchApprovalRequestEvent,
    ) {
        self.defer_or_handle(
            ev,
            InterruptManager::push_apply_patch_approval,
            Self::handle_apply_patch_approval_now,
        );
    }

    /// Handle guardian review lifecycle events for the current thread.
    ///
    /// In-progress assessments temporarily own the live status footer so the
    /// user can see what is being reviewed, including parallel review
    /// aggregation. Terminal assessments clear or update that footer state and
    /// render the final approved/denied history cell when guardian returns a
    /// decision.
    pub(super) fn on_guardian_assessment(&mut self, ev: GuardianAssessmentEvent) {
        let permission_request_summary = |subject: &str, reason: &Option<String>| {
            reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!("{subject}: {reason}"))
                .unwrap_or_else(|| subject.to_string())
        };
        let guardian_action_summary = |action: &GuardianAssessmentAction| match action {
            GuardianAssessmentAction::Command { command, .. } => Some(command.clone()),
            GuardianAssessmentAction::Execve { program, argv, .. } => {
                let command = if argv.is_empty() {
                    vec![program.clone()]
                } else {
                    argv.clone()
                };
                shlex::try_join(command.iter().map(String::as_str))
                    .ok()
                    .or_else(|| Some(command.join(" ")))
            }
            GuardianAssessmentAction::ApplyPatch { files, .. } => Some(if files.len() == 1 {
                format!("apply_patch touching {}", files[0].display())
            } else {
                format!("apply_patch touching {} files", files.len())
            }),
            GuardianAssessmentAction::NetworkAccess { target, .. } => {
                Some(format!("network access to {target}"))
            }
            GuardianAssessmentAction::McpToolCall {
                server,
                tool_name,
                connector_name,
                ..
            } => {
                let label = connector_name.as_deref().unwrap_or(server.as_str());
                Some(format!("MCP {tool_name} on {label}"))
            }
            GuardianAssessmentAction::RequestPermissions { reason, .. } => {
                Some(permission_request_summary("permission request", reason))
            }
        };
        let guardian_command = |action: &GuardianAssessmentAction| match action {
            GuardianAssessmentAction::Command { command, .. } => shlex::split(command)
                .filter(|command| !command.is_empty())
                .or_else(|| Some(vec![command.clone()])),
            GuardianAssessmentAction::Execve { program, argv, .. } => Some(if argv.is_empty() {
                vec![program.clone()]
            } else {
                argv.clone()
            })
            .filter(|command| !command.is_empty()),
            GuardianAssessmentAction::ApplyPatch { .. }
            | GuardianAssessmentAction::NetworkAccess { .. }
            | GuardianAssessmentAction::McpToolCall { .. }
            | GuardianAssessmentAction::RequestPermissions { .. } => None,
        };

        if ev.status == GuardianAssessmentStatus::InProgress
            && let Some(detail) = guardian_action_summary(&ev.action)
        {
            // In-progress assessments own the live footer state while the
            // review is pending. Parallel reviews are aggregated into one
            // footer summary by `PendingGuardianReviewStatus`.
            self.bottom_pane.ensure_status_indicator();
            self.bottom_pane
                .set_interrupt_hint_visible(/*visible*/ true);
            self.status_state
                .pending_guardian_review_status
                .start_or_update(ev.id.clone(), detail);
            if let Some(status) = self
                .status_state
                .pending_guardian_review_status
                .status_indicator_state()
            {
                self.set_status(
                    status.header,
                    status.details,
                    StatusDetailsCapitalization::Preserve,
                    status.details_max_lines,
                );
            }
            self.request_redraw();
            return;
        }

        // Terminal assessments remove the matching pending footer entry first,
        // then render the final approved/denied history cell below.
        if self
            .status_state
            .pending_guardian_review_status
            .finish(&ev.id)
        {
            if let Some(status) = self
                .status_state
                .pending_guardian_review_status
                .status_indicator_state()
            {
                self.set_status(
                    status.header,
                    status.details,
                    StatusDetailsCapitalization::Preserve,
                    status.details_max_lines,
                );
            } else if self.status_state.current_status.is_guardian_review() {
                self.set_status_header(String::from("Working"));
            }
        } else if self.status_state.pending_guardian_review_status.is_empty()
            && self.status_state.current_status.is_guardian_review()
        {
            self.set_status_header(String::from("Working"));
        }

        if ev.status == GuardianAssessmentStatus::Approved {
            let cell = if let Some(command) = guardian_command(&ev.action) {
                history_cell::new_approval_decision_cell(
                    history_cell::ApprovalDecisionSubject::Command(command),
                    crate::history_cell::ReviewDecision::Approved,
                    history_cell::ApprovalDecisionActor::Guardian,
                )
            } else if let Some(summary) = guardian_action_summary(&ev.action) {
                history_cell::new_guardian_approved_action_request(summary)
            } else {
                let summary = serde_json::to_string(&ev.action)
                    .unwrap_or_else(|_| "<unrenderable guardian action>".to_string());
                history_cell::new_guardian_approved_action_request(summary)
            };

            self.add_boxed_history(cell);
            self.request_redraw();
            return;
        }

        if ev.status == GuardianAssessmentStatus::TimedOut {
            let cell = if let Some(command) = guardian_command(&ev.action) {
                history_cell::new_approval_decision_cell(
                    history_cell::ApprovalDecisionSubject::Command(command),
                    crate::history_cell::ReviewDecision::TimedOut,
                    history_cell::ApprovalDecisionActor::Guardian,
                )
            } else {
                match &ev.action {
                    GuardianAssessmentAction::ApplyPatch { files, .. } => {
                        let files = files
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>();
                        history_cell::new_guardian_timed_out_patch_request(files)
                    }
                    GuardianAssessmentAction::McpToolCall {
                        server, tool_name, ..
                    } => history_cell::new_guardian_timed_out_action_request(format!(
                        "codex could call MCP tool {server}.{tool_name}"
                    )),
                    GuardianAssessmentAction::NetworkAccess { target, .. } => {
                        history_cell::new_guardian_timed_out_action_request(format!(
                            "codex could access {target}"
                        ))
                    }
                    GuardianAssessmentAction::RequestPermissions { reason, .. } => {
                        history_cell::new_guardian_timed_out_action_request(
                            permission_request_summary("codex could request permissions", reason),
                        )
                    }
                    GuardianAssessmentAction::Command { .. } => unreachable!(),
                    GuardianAssessmentAction::Execve { .. } => unreachable!(),
                }
            };

            self.add_boxed_history(cell);
            self.request_redraw();
            return;
        }

        if ev.status != GuardianAssessmentStatus::Denied {
            return;
        }
        self.review.recent_auto_review_denials.push(ev.clone());
        let cell = if let Some(command) = guardian_command(&ev.action) {
            history_cell::new_approval_decision_cell(
                history_cell::ApprovalDecisionSubject::Command(command),
                crate::history_cell::ReviewDecision::Denied,
                history_cell::ApprovalDecisionActor::Guardian,
            )
        } else {
            match &ev.action {
                GuardianAssessmentAction::ApplyPatch { files, .. } => {
                    let files = files
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>();
                    history_cell::new_guardian_denied_patch_request(files)
                }
                GuardianAssessmentAction::McpToolCall {
                    server, tool_name, ..
                } => history_cell::new_guardian_denied_action_request(format!(
                    "codex to call MCP tool {server}.{tool_name}"
                )),
                GuardianAssessmentAction::NetworkAccess { target, .. } => {
                    history_cell::new_guardian_denied_action_request(format!(
                        "codex to access {target}"
                    ))
                }
                GuardianAssessmentAction::RequestPermissions { reason, .. } => {
                    history_cell::new_guardian_denied_action_request(permission_request_summary(
                        "codex to request permissions",
                        reason,
                    ))
                }
                GuardianAssessmentAction::Command { .. } => unreachable!(),
                GuardianAssessmentAction::Execve { .. } => unreachable!(),
            }
        };

        self.add_boxed_history(cell);
        self.request_redraw();
    }

    pub(super) fn on_elicitation_request(
        &mut self,
        request_id: AppServerRequestId,
        params: McpServerElicitationRequestParams,
    ) {
        self.defer_or_handle(
            (request_id, params),
            |q, (request_id, params)| q.push_elicitation(request_id, params),
            |s, (request_id, params)| s.handle_elicitation_request_now(request_id, params),
        );
    }

    pub(super) fn on_request_user_input(&mut self, ev: ToolRequestUserInputParams) {
        self.defer_or_handle(
            ev,
            InterruptManager::push_user_input,
            Self::handle_request_user_input_now,
        );
    }

    pub(super) fn on_request_permissions(&mut self, ev: RequestPermissionsEvent) {
        self.defer_or_handle(
            ev,
            InterruptManager::push_request_permissions,
            Self::handle_request_permissions_now,
        );
    }

    pub(crate) fn handle_exec_approval_now(&mut self, ev: ExecApprovalRequestEvent) {
        self.flush_answer_stream_with_separator();
        let command = shlex::try_join(ev.command.iter().map(String::as_str))
            .unwrap_or_else(|_| ev.command.join(" "));
        self.notify(Notification::ExecApprovalRequested { command });

        let available_decisions = ev.effective_available_decisions();
        let request = ApprovalRequest::Exec(ExecApprovalRequest {
            thread_id: self.thread_id.unwrap_or_default(),
            thread_label: None,
            id: ev.effective_approval_id(),
            environment_id: ev.environment_id,
            command: ev.command,
            reason: ev.reason,
            available_decisions,
            network_approval_context: ev.network_approval_context,
            additional_permissions: ev.additional_permissions,
        });
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn handle_apply_patch_approval_now(&mut self, ev: ApplyPatchApprovalRequestEvent) {
        self.flush_answer_stream_with_separator();

        let changed_paths = ev.changes.keys().cloned().collect();
        let request = ApprovalRequest::ApplyPatch(ApplyPatchApprovalRequest {
            thread_id: self.thread_id.unwrap_or_default(),
            thread_label: None,
            id: ev.call_id,
            reason: ev.reason,
            changes: ev.changes,
            cwd: self.config.cwd.clone(),
        });
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
        self.notify(Notification::EditApprovalRequested {
            cwd: self.config.cwd.to_path_buf(),
            changes: changed_paths,
        });
    }

    pub(crate) fn handle_elicitation_request_now(
        &mut self,
        request_id: AppServerRequestId,
        params: McpServerElicitationRequestParams,
    ) {
        self.flush_answer_stream_with_separator();

        self.notify(Notification::ElicitationRequested {
            server_name: params.server_name.clone(),
        });

        let thread_id = ThreadId::from_string(&params.thread_id)
            .unwrap_or_else(|_| self.thread_id.unwrap_or_default());
        if let Some(params) = crate::bottom_pane::AppLinkViewParams::from_url_app_server_request(
            thread_id,
            &params.server_name,
            request_id.clone(),
            &params.request,
        ) {
            self.open_app_link_view(params);
        } else if let Some(request) = McpServerElicitationFormRequest::from_app_server_request(
            thread_id,
            request_id.clone(),
            &params,
        ) {
            self.bottom_pane
                .push_mcp_server_elicitation_request(request);
        } else {
            match params.request {
                McpServerElicitationRequest::Form { message, .. } => {
                    let request = ApprovalRequest::McpElicitation(McpElicitationApprovalRequest {
                        thread_id,
                        thread_label: None,
                        server_name: params.server_name,
                        request_id,
                        message,
                    });
                    self.bottom_pane
                        .push_approval_request(request, &self.config.features);
                }
                McpServerElicitationRequest::OpenAiForm { .. }
                | McpServerElicitationRequest::Url { .. } => {
                    self.app_event_tx.resolve_elicitation(
                        thread_id,
                        params.server_name,
                        request_id,
                        codex_app_server_protocol::McpServerElicitationAction::Decline,
                        /*content*/ None,
                        /*meta*/ None,
                    );
                }
            }
        }
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn push_approval_request(&mut self, request: ApprovalRequest) {
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn push_mcp_server_elicitation_request(
        &mut self,
        request: McpServerElicitationFormRequest,
    ) {
        self.bottom_pane
            .push_mcp_server_elicitation_request(request);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn handle_request_user_input_now(&mut self, ev: ToolRequestUserInputParams) {
        self.flush_answer_stream_with_separator();
        let question_count = ev.questions.len();
        let summary = Notification::user_input_request_summary(&ev.questions);
        let title = match (question_count, summary.as_deref()) {
            (1, Some(summary)) => summary.to_string(),
            (1, None) => "Question requested".to_string(),
            (count, _) => format!("{count} questions requested"),
        };
        self.notify(Notification::PlanModePrompt { title });
        self.bottom_pane.push_user_input_request(ev);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }

    pub(crate) fn handle_request_permissions_now(&mut self, ev: RequestPermissionsEvent) {
        self.flush_answer_stream_with_separator();
        let request = ApprovalRequest::Permissions(PermissionsApprovalRequest {
            thread_id: self.thread_id.unwrap_or_default(),
            thread_label: None,
            call_id: ev.call_id,
            environment_id: ev.environment_id,
            reason: ev.reason,
            permissions: ev.permissions,
        });
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.set_ambient_pet_notification(
            crate::pets::PetNotificationKind::Waiting,
            /*body*/ None,
        );
        self.request_redraw();
    }
}
