//! Approval-request focused tests extracted from the main chatwidget test file
//! to keep the primary module under blob-size policy limits.

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn exec_approval_emits_proposed_command_and_decision_history() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    // Trigger an exec approval request with a short, single-line command.
    let ev = ExecApprovalRequestEvent {
        call_id: "call-short".into(),
        approval_id: Some("call-short".into()),
        turn_id: "turn-short".into(),
        environment_id: Some("remote".to_string()),
        raw_command: None,
        command: vec!["bash".into(), "-lc".into(), "echo hello world".into()],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: Some(
            "this is a test reason such as one that would be produced by the model".into(),
        ),
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
    };
    handle_exec_approval_request(&mut chat, "sub-short", ev);

    let proposed_cells = drain_insert_history(&mut rx);
    assert!(
        proposed_cells.is_empty(),
        "expected approval request to render via modal without emitting history cells"
    );

    let area = Rect::new(0, 0, 80, chat.desired_height(/*width*/ 80));
    let mut buf = ratatui::buffer::Buffer::empty(area);
    chat.render(area, &mut buf);
    assert_snapshot!("exec_approval_modal_exec", format!("{buf:?}"));

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let decision = drain_insert_history(&mut rx)
        .pop()
        .expect("expected decision cell in history");
    assert_snapshot!(
        "exec_approval_history_decision_approved_short",
        lines_to_single_string(&decision)
    );
}

#[test]
fn app_server_exec_approval_request_splits_shell_wrapped_command() {
    let script = r#"python3 -c 'print("Hello, world!")'"#;
    let request = exec_approval_request_from_params(
        AppServerCommandExecutionRequestApprovalParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
            started_at_ms: 0,
            approval_id: Some("approval-1".to_string()),
            environment_id: None,
            reason: None,
            network_approval_context: None,
            command: Some(
                shlex::try_join(["/bin/zsh", "-lc", script])
                    .expect("round-trippable shell wrapper"),
            ),
            cwd: Some(test_path_buf("/tmp").abs().into()),
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        },
        &test_path_buf("/tmp").abs(),
    );

    assert_eq!(
        request.command,
        vec![
            "/bin/zsh".to_string(),
            "-lc".to_string(),
            script.to_string(),
        ]
    );
}

#[test]
fn app_server_exec_approval_request_preserves_permissions_context() {
    let read_path = AbsolutePathBuf::try_from(PathBuf::from(test_path_display("/tmp/read-only")))
        .expect("absolute read path");
    let write_path = AbsolutePathBuf::try_from(PathBuf::from(test_path_display("/tmp/write")))
        .expect("absolute write path");
    let read_api_path = LegacyAppPathString::from_abs_path(&read_path);
    let write_api_path = LegacyAppPathString::from_abs_path(&write_path);
    let request = exec_approval_request_from_params(
        AppServerCommandExecutionRequestApprovalParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
            started_at_ms: 0,
            approval_id: Some("approval-1".to_string()),
            environment_id: None,
            reason: None,
            network_approval_context: Some(codex_app_server_protocol::NetworkApprovalContext {
                host: "example.com".to_string(),
                protocol: codex_app_server_protocol::NetworkApprovalProtocol::Socks5Tcp,
            }),
            command: Some("ls".to_string()),
            cwd: Some(test_path_buf("/tmp").abs().into()),
            command_actions: None,
            additional_permissions: Some(AppServerAdditionalPermissionProfile {
                network: Some(AppServerAdditionalNetworkPermissions {
                    enabled: Some(true),
                }),
                file_system: Some(AppServerAdditionalFileSystemPermissions {
                    read: Some(vec![read_api_path.clone()]),
                    write: Some(vec![write_api_path.clone()]),
                    glob_scan_max_depth: None,
                    entries: None,
                }),
            }),
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        },
        &test_path_buf("/tmp").abs(),
    );

    assert_eq!(
        request.network_approval_context,
        Some(codex_app_server_protocol::NetworkApprovalContext {
            host: "example.com".to_string(),
            protocol: codex_app_server_protocol::NetworkApprovalProtocol::Socks5Tcp,
        })
    );
    assert_eq!(
        request.additional_permissions,
        Some(AppServerAdditionalPermissionProfile {
            network: Some(AppServerAdditionalNetworkPermissions {
                enabled: Some(true),
            }),
            file_system: Some(AppServerAdditionalFileSystemPermissions {
                read: Some(vec![read_api_path]),
                write: Some(vec![write_api_path]),
                glob_scan_max_depth: None,
                entries: None,
            }),
        })
    );
}

#[tokio::test]
async fn network_exec_approval_history_describes_session_host_allowance() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let request = exec_approval_request_from_params(
        AppServerCommandExecutionRequestApprovalParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
            started_at_ms: 0,
            approval_id: Some("approval-1".to_string()),
            environment_id: None,
            reason: None,
            network_approval_context: Some(codex_app_server_protocol::NetworkApprovalContext {
                host: "example.com".to_string(),
                protocol: codex_app_server_protocol::NetworkApprovalProtocol::Https,
            }),
            command: Some("network-access https://example.com:8443".to_string()),
            cwd: None,
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: Some(vec![
                codex_app_server_protocol::CommandExecutionApprovalDecision::AcceptForSession,
                codex_app_server_protocol::CommandExecutionApprovalDecision::Cancel,
            ]),
        },
        &test_path_buf("/tmp").abs(),
    );

    handle_exec_approval_request(&mut chat, "sub-network", request);
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));

    let decision = drain_insert_history(&mut rx)
        .pop()
        .expect("expected decision cell in history");
    assert_snapshot!(
        "network_exec_approval_history_session_host_allowance",
        lines_to_single_string(&decision)
    );
}

#[tokio::test]
async fn network_exec_approval_history_describes_one_time_host_allowance() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let request = exec_approval_request_from_params(
        AppServerCommandExecutionRequestApprovalParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
            started_at_ms: 0,
            approval_id: Some("approval-1".to_string()),
            environment_id: None,
            reason: None,
            network_approval_context: Some(codex_app_server_protocol::NetworkApprovalContext {
                host: "example.com".to_string(),
                protocol: codex_app_server_protocol::NetworkApprovalProtocol::Http,
            }),
            command: None,
            cwd: None,
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: Some(vec![
                codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
                codex_app_server_protocol::CommandExecutionApprovalDecision::Cancel,
            ]),
        },
        &test_path_buf("/tmp").abs(),
    );

    handle_exec_approval_request(&mut chat, "sub-network", request);
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

    let decision = drain_insert_history(&mut rx)
        .pop()
        .expect("expected decision cell in history");
    assert_snapshot!(
        "network_exec_approval_history_one_time_host_allowance",
        lines_to_single_string(&decision)
    );
}

#[tokio::test]
async fn network_exec_approval_history_describes_canceled_host_request() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let request = exec_approval_request_from_params(
        AppServerCommandExecutionRequestApprovalParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
            started_at_ms: 0,
            approval_id: Some("approval-1".to_string()),
            environment_id: None,
            reason: None,
            network_approval_context: Some(codex_app_server_protocol::NetworkApprovalContext {
                host: "example.com".to_string(),
                protocol: codex_app_server_protocol::NetworkApprovalProtocol::Socks5Tcp,
            }),
            command: Some("network-access socks5-tcp://example.com:1080".to_string()),
            cwd: None,
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: Some(vec![
                codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
                codex_app_server_protocol::CommandExecutionApprovalDecision::Cancel,
            ]),
        },
        &test_path_buf("/tmp").abs(),
    );

    handle_exec_approval_request(&mut chat, "sub-network", request);
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

    let decision = drain_insert_history(&mut rx)
        .pop()
        .expect("expected decision cell in history");
    assert_snapshot!(
        "network_exec_approval_history_canceled_host_request",
        lines_to_single_string(&decision)
    );
}

#[test]
fn app_server_request_permissions_preserves_file_system_permissions() {
    let read_path = AbsolutePathBuf::try_from(PathBuf::from(test_path_display("/tmp/read-only")))
        .expect("absolute read path");
    let write_path = AbsolutePathBuf::try_from(PathBuf::from(test_path_display("/tmp/write")))
        .expect("absolute write path");
    let read_api_path = LegacyAppPathString::from_abs_path(&read_path);
    let write_api_path = LegacyAppPathString::from_abs_path(&write_path);
    let cwd =
        AbsolutePathBuf::try_from(PathBuf::from(test_path_display("/tmp"))).expect("absolute cwd");

    let request = request_permissions_from_params(AppServerPermissionsRequestApprovalParams {
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        item_id: "item-1".to_string(),
        environment_id: Some("remote".to_string()),
        started_at_ms: 0,
        cwd: cwd.clone(),
        reason: Some("Select a workspace root".to_string()),
        permissions: codex_app_server_protocol::RequestPermissionProfile {
            network: Some(AppServerAdditionalNetworkPermissions {
                enabled: Some(true),
            }),
            file_system: Some(AppServerAdditionalFileSystemPermissions {
                read: Some(vec![read_api_path]),
                write: Some(vec![write_api_path]),
                glob_scan_max_depth: None,
                entries: None,
            }),
        },
    })
    .expect("API paths should convert to native paths");

    assert_eq!(
        request.permissions,
        RequestPermissionProfile {
            network: Some(NetworkPermissions {
                enabled: Some(true),
            }),
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                Some(vec![read_path]),
                Some(vec![write_path]),
            )),
        }
    );
    assert_eq!(request.cwd, Some(cwd));
    assert_eq!(request.environment_id.as_deref(), Some("remote"));
}

#[tokio::test]
async fn exec_approval_uses_approval_id_when_present() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    handle_exec_approval_request(
        &mut chat,
        "sub-short",
        ExecApprovalRequestEvent {
            call_id: "call-parent".into(),
            approval_id: Some("approval-subcommand".into()),
            turn_id: "turn-short".into(),
            environment_id: None,
            raw_command: None,
            command: vec!["bash".into(), "-lc".into(), "echo hello world".into()],
            cwd: AbsolutePathBuf::current_dir().expect("current dir"),
            reason: Some(
                "this is a test reason such as one that would be produced by the model".into(),
            ),
            network_approval_context: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: None,
        },
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

    let mut found = false;
    while let Ok(app_ev) = rx.try_recv() {
        if let AppEvent::SubmitThreadOp {
            op: Op::ExecApproval { id, decision, .. },
            ..
        } = app_ev
        {
            assert_eq!(id, "approval-subcommand");
            assert_matches!(
                decision,
                codex_app_server_protocol::CommandExecutionApprovalDecision::Accept
            );
            found = true;
            break;
        }
    }
    assert!(found, "expected ExecApproval op to be sent");
}

#[tokio::test]
async fn remote_im_exec_approval_requires_task_identity_and_validates_decision() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.remote_im_forwarding_active = true;
    let persistent_decision =
        codex_app_server_protocol::CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
            execpolicy_amendment: codex_app_server_protocol::ExecPolicyAmendment {
                command: vec!["Remove-Item".into(), "-Force".into()],
            },
        };
    let display = |message: &str| UserMessageDisplay {
        message: message.to_string(),
        remote_image_urls: Vec::new(),
        local_images: Vec::new(),
        text_elements: Vec::new(),
    };
    chat.remote_im_pending_replies
        .push_back(PendingRemoteImReply {
            committed_echo: display("A"),
            reply_id: "reply-a".to_string(),
            task_id: Some("task-a".to_string()),
            source_routed: true,
            bound_turn_id: None,
        });
    chat.remote_im_pending_replies
        .push_back(PendingRemoteImReply {
            committed_echo: display("B"),
            reply_id: "reply-b".to_string(),
            task_id: Some("task-b".to_string()),
            source_routed: true,
            bound_turn_id: None,
        });
    // TurnStarted for A binds the oldest submitted-but-uncommitted route.
    chat.bind_pending_remote_im_route_to_turn("turn-1");
    // A second TurnStarted can precede both committed echoes; it must consume
    // B's unbound entry instead of binding A twice.
    chat.bind_pending_remote_im_route_to_turn("turn-2");
    assert_eq!(
        chat.remote_im_route_for_turn("turn-1"),
        Some(RemoteImTurnRoute {
            reply_id: "reply-a".to_string(),
            task_id: Some("task-a".to_string()),
            source_routed: true,
        })
    );
    assert_eq!(
        chat.remote_im_route_for_turn("turn-2"),
        Some(RemoteImTurnRoute {
            reply_id: "reply-b".to_string(),
            task_id: Some("task-b".to_string()),
            source_routed: true,
        })
    );
    // A later remote prompt must not steal turn A's security approval.
    chat.remote_im_active_reply_id = Some("reply-b".to_string());
    chat.remote_im_active_task_id = Some("task-b".to_string());

    handle_exec_approval_request(
        &mut chat,
        "request-1",
        ExecApprovalRequestEvent {
            call_id: "item-1".into(),
            approval_id: Some("approval-1".into()),
            turn_id: "turn-1".into(),
            environment_id: None,
            raw_command: None,
            command: vec!["powershell".into(), "Remove-Item -Force target".into()],
            cwd: AbsolutePathBuf::current_dir().expect("current dir"),
            reason: Some("dangerous command".into()),
            network_approval_context: None,
            proposed_execpolicy_amendment: Some(codex_app_server_protocol::ExecPolicyAmendment {
                command: vec!["Remove-Item".into(), "-Force".into()],
            }),
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: Some(vec![
                codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
                persistent_decision.clone(),
                codex_app_server_protocol::CommandExecutionApprovalDecision::Cancel,
            ]),
        },
    );

    assert_eq!(
        chat.remote_im_exec_approval_decision("approval-1", "accept-persistent"),
        Ok(persistent_decision.clone())
    );
    assert_eq!(
        chat.remote_im_exec_approval_decision("approval-1", "unsupported"),
        Err("approval decision is not available for this request".to_string())
    );

    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-1",
            "task-a",
            "approval-1",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Ok(())
    );
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-1",
            "task-a",
            "approval-1",
            &persistent_decision,
        ),
        Ok(())
    );
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            ThreadId::new(),
            "turn-1",
            "task-a",
            "approval-1",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval belongs to a different Codex thread".to_string())
    );
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-other",
            "task-a",
            "approval-1",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval belongs to a different Codex turn".to_string())
    );
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-1",
            "task-b",
            "approval-1",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval belongs to a different remote IM task".to_string())
    );
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-1",
            "task-a",
            "approval-1",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Decline,
        ),
        Err("approval decision is not available for this request".to_string())
    );

    chat.note_remote_im_exec_approval_resolved("approval-1");
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-1",
            "task-a",
            "approval-1",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval is no longer pending".to_string())
    );
    handle_turn_completed(&mut chat, "turn-1", None);
    assert!(!chat.remote_im_turn_routes.contains_key("turn-1"));
    assert!(chat.remote_im_turn_routes.contains_key("turn-2"));
    assert_eq!(chat.remote_im_active_reply_id.as_deref(), Some("reply-b"));
    assert_eq!(chat.remote_im_active_task_id.as_deref(), Some("task-b"));
}

#[tokio::test]
async fn local_takeover_revokes_remote_approval_authority_synchronously() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.remote_im_forwarding_active = true;
    chat.remember_remote_im_turn_route_if_absent(
        "turn-local-takeover".to_string(),
        RemoteImTurnRoute {
            reply_id: "reply-a".to_string(),
            task_id: Some("task-a".to_string()),
            source_routed: true,
        },
    );

    let approval = |approval_id: &str| ExecApprovalRequestEvent {
        call_id: format!("item-{approval_id}"),
        approval_id: Some(approval_id.to_string()),
        turn_id: "turn-local-takeover".into(),
        environment_id: None,
        raw_command: Some("Remove-Item -LiteralPath target -Recurse -Force".into()),
        command: vec!["powershell".into(), "Remove-Item target -Force".into()],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: Some("dangerous command".into()),
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: Some(vec![
            codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
            codex_app_server_protocol::CommandExecutionApprovalDecision::Cancel,
        ]),
    };

    handle_exec_approval_request(&mut chat, "request-before", approval("approval-before"));
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-local-takeover",
            "task-a",
            "approval-before",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Ok(())
    );

    chat.set_remote_im_input_origin(false);
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-local-takeover",
            "task-a",
            "approval-before",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval is no longer pending".to_string())
    );

    // The immutable turn route remains as a correlation tombstone, but it no
    // longer grants the remote requester authority over commands initiated by
    // the local steer.
    handle_exec_approval_request(&mut chat, "request-after", approval("approval-after"));
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-local-takeover",
            "task-a",
            "approval-after",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval is no longer pending".to_string())
    );
}

#[tokio::test]
async fn non_remote_or_uncorrelated_exec_approval_is_not_forwardable() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.remember_remote_im_turn_route_if_absent(
        "turn-2".to_string(),
        RemoteImTurnRoute {
            reply_id: "local-reply".to_string(),
            task_id: Some("local-task".to_string()),
            source_routed: false,
        },
    );

    handle_exec_approval_request(
        &mut chat,
        "request-2",
        ExecApprovalRequestEvent {
            call_id: "item-2".into(),
            approval_id: Some("approval-2".into()),
            turn_id: "turn-2".into(),
            environment_id: None,
            raw_command: None,
            command: vec!["rm".into(), "-f".into(), "target".into()],
            cwd: AbsolutePathBuf::current_dir().expect("current dir"),
            reason: None,
            network_approval_context: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: None,
        },
    );

    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-2",
            "local-task",
            "approval-2",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval is no longer pending".to_string())
    );

    chat.remember_remote_im_turn_route_if_absent(
        "turn-3".to_string(),
        RemoteImTurnRoute {
            reply_id: "reply-without-task".to_string(),
            task_id: None,
            source_routed: true,
        },
    );
    handle_exec_approval_request(
        &mut chat,
        "request-3",
        ExecApprovalRequestEvent {
            call_id: "item-3".into(),
            approval_id: Some("approval-3".into()),
            turn_id: "turn-3".into(),
            environment_id: None,
            raw_command: None,
            command: vec!["rm".into(), "-f".into(), "target".into()],
            cwd: AbsolutePathBuf::current_dir().expect("current dir"),
            reason: None,
            network_approval_context: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: None,
        },
    );
    assert_eq!(
        chat.validate_remote_im_exec_approval(
            thread_id,
            "turn-3",
            "missing-task",
            "approval-3",
            &codex_app_server_protocol::CommandExecutionApprovalDecision::Accept,
        ),
        Err("approval is no longer pending".to_string())
    );
}

#[tokio::test]
async fn exec_approval_decision_truncates_multiline_and_long_commands() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;

    let ev_multi = ExecApprovalRequestEvent {
        call_id: "call-multi".into(),
        approval_id: Some("call-multi".into()),
        turn_id: "turn-multi".into(),
        environment_id: None,
        raw_command: None,
        command: vec!["bash".into(), "-lc".into(), "echo line1\necho line2".into()],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: Some(
            "this is a test reason such as one that would be produced by the model".into(),
        ),
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
    };
    handle_exec_approval_request(&mut chat, "sub-multi", ev_multi);
    let proposed_multi = drain_insert_history(&mut rx);
    assert!(
        proposed_multi.is_empty(),
        "expected multiline approval request to render via modal without emitting history cells"
    );

    let area = Rect::new(0, 0, 80, chat.desired_height(/*width*/ 80));
    let mut buf = ratatui::buffer::Buffer::empty(area);
    chat.render(area, &mut buf);
    let mut saw_first_line = false;
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        if row.contains("echo line1") {
            saw_first_line = true;
            break;
        }
    }
    assert!(
        saw_first_line,
        "expected modal to show first line of multiline snippet"
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    let aborted_multi = drain_insert_history(&mut rx)
        .pop()
        .expect("expected aborted decision cell (multiline)");
    assert_snapshot!(
        "exec_approval_history_decision_aborted_multiline",
        lines_to_single_string(&aborted_multi)
    );

    let long = format!("echo {}", "a".repeat(200));
    let ev_long = ExecApprovalRequestEvent {
        call_id: "call-long".into(),
        approval_id: Some("call-long".into()),
        turn_id: "turn-long".into(),
        environment_id: None,
        raw_command: None,
        command: vec!["bash".into(), "-lc".into(), long],
        cwd: AbsolutePathBuf::current_dir().expect("current dir"),
        reason: None,
        network_approval_context: None,
        proposed_execpolicy_amendment: None,
        proposed_network_policy_amendments: None,
        additional_permissions: None,
        available_decisions: None,
    };
    handle_exec_approval_request(&mut chat, "sub-long", ev_long);
    let proposed_long = drain_insert_history(&mut rx);
    assert!(
        proposed_long.is_empty(),
        "expected long approval request to avoid emitting history cells before decision"
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    let aborted_long = drain_insert_history(&mut rx)
        .pop()
        .expect("expected aborted decision cell (long)");
    assert_snapshot!(
        "exec_approval_history_decision_aborted_long",
        lines_to_single_string(&aborted_long)
    );
}
