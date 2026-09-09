//! A host route ends at a structured final/error, not at the next local keystroke.
//! Exercise the notification handlers and the machine-message/human-steer sequence.

use super::*;
use pretty_assertions::assert_eq;

fn complete_remote_turn(chat: &mut ChatWidget, turn_id: &str, status: AppServerTurnStatus) {
    let mut turn = app_server_turn(
        turn_id, status, /*duration_ms*/ None, /*error*/ None,
    );
    if matches!(turn.status, AppServerTurnStatus::Completed) {
        turn.items.push(AppServerThreadItem::AgentMessage {
            id: format!("{turn_id}-answer"),
            text: "Finished this request.".to_string(),
            phase: Some(MessagePhase::FinalAnswer),
            memory_citation: None,
            delivery: None,
            questions: None,
        });
    }
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
            turn,
        }),
        /*replay_kind*/ None,
    );
}

#[tokio::test]
async fn completed_source_route_is_not_reused_by_machine_turn_before_human_steer() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let first_prompt = "First human request";
    chat.submit_user_message_from_remote_im(
        first_prompt.to_string(),
        first_prompt.to_string(),
        Vec::new(),
        /*remote_im_input*/ true,
        /*preserve_remote_im_route*/ false,
        Some("reply-a".to_string()),
        Some("task-a".to_string()),
    )
    .unwrap();
    handle_turn_started(&mut chat, "turn-1");
    complete_user_message(&mut chat, "user-a", first_prompt);
    assert_eq!(chat.remote_im_active_task_id.as_deref(), Some("task-a"));
    assert!(chat.remote_im_pending_replies.is_empty());

    complete_remote_turn(&mut chat, "turn-1", AppServerTurnStatus::Completed);
    // The host has removed A. Machine input must not resurrect it in turn-2.
    let machine_prompt = "Machine progress update";
    chat.submit_user_message_from_remote_im(
        machine_prompt.to_string(),
        machine_prompt.to_string(),
        Vec::new(),
        /*remote_im_input*/ false,
        /*preserve_remote_im_route*/ true,
        /*reply_id*/ None,
        /*task_id*/ None,
    )
    .unwrap();
    handle_turn_started(&mut chat, "turn-2");
    assert_eq!(chat.remote_im_route_for_turn("turn-2"), None);

    // A human now steers the machine-started turn. With no stale binding,
    // the committed human message can bind B instead of being stuck on A.
    let next_prompt = "Where is my reply?";
    chat.submit_user_message_from_remote_im(
        next_prompt.to_string(),
        next_prompt.to_string(),
        Vec::new(),
        /*remote_im_input*/ true,
        /*preserve_remote_im_route*/ false,
        Some("reply-b".to_string()),
        Some("task-b".to_string()),
    )
    .unwrap();
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
            turn_id: "turn-2".to_string(),
            completed_at_ms: 0,
            item: AppServerThreadItem::UserMessage {
                id: "user-b".to_string(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: next_prompt.to_string(),
                    text_elements: Vec::new(),
                }],
            },
        }),
        /*replay_kind*/ None,
    );
    assert_eq!(
        chat.remote_im_route_for_turn("turn-2"),
        Some(RemoteImTurnRoute {
            reply_id: "reply-b".to_string(),
            task_id: Some("task-b".to_string()),
            source_routed: true,
        })
    );
    complete_remote_turn(&mut chat, "turn-2", AppServerTurnStatus::Completed);
    assert_eq!(chat.remote_im_active_reply_id, None);
    assert_eq!(chat.remote_im_active_task_id, None);
}

#[tokio::test]
async fn source_route_terminal_notifications_clear_only_the_matching_active_route() {
    for status in [
        AppServerTurnStatus::Completed,
        AppServerTurnStatus::Interrupted,
        AppServerTurnStatus::Failed,
    ] {
        for active_task in ["task-a", "task-b"] {
            let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
            chat.remote_im_forwarding_active = true;
            let active_reply = format!("reply-{active_task}");
            chat.remote_im_active_reply_id = Some(active_reply.clone());
            chat.remote_im_active_task_id = Some(active_task.to_string());
            chat.remember_remote_im_turn_route_if_absent(
                "turn-a".to_string(),
                RemoteImTurnRoute {
                    reply_id: "reply-task-a".to_string(),
                    task_id: Some("task-a".to_string()),
                    source_routed: true,
                },
            );
            complete_remote_turn(&mut chat, "turn-a", status.clone());
            let expected = if active_task == "task-b" {
                (Some(active_reply.as_str()), Some(active_task))
            } else {
                (None, None)
            };
            assert_eq!(
                (
                    chat.remote_im_active_reply_id.as_deref(),
                    chat.remote_im_active_task_id.as_deref(),
                ),
                expected,
                "terminal status {status:?} must not clear a newer route"
            );
            assert!(chat.remote_im_turn_routes.is_empty());
        }
    }
}

#[tokio::test]
async fn source_goal_final_releases_route_while_legacy_goal_progress_preserves_it() {
    for source_routed in [false, true] {
        let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.remote_im_forwarding_active = source_routed;
        chat.remote_im_active_reply_id = Some("reply-goal".to_string());
        chat.remote_im_active_task_id = Some("task-goal".to_string());
        chat.remember_remote_im_turn_route_if_absent(
            "turn-goal".to_string(),
            RemoteImTurnRoute {
                reply_id: "reply-goal".to_string(),
                task_id: Some("task-goal".to_string()),
                source_routed,
            },
        );
        chat.current_goal_status = Some(GoalStatusState::new(
            codex_app_server_protocol::ThreadGoal {
                thread_id: "thread-1".to_string(),
                objective: "Continue working".to_string(),
                status: codex_app_server_protocol::ThreadGoalStatus::Active,
                token_budget: None,
                tokens_used: 100,
                time_used_seconds: 60,
                created_at: 0,
                updated_at: 0,
            },
            std::time::Instant::now(),
        ));
        complete_remote_turn(&mut chat, "turn-goal", AppServerTurnStatus::Completed);
        assert_eq!(
            chat.remote_im_active_reply_id.as_deref(),
            (!source_routed).then_some("reply-goal")
        );
        assert_eq!(
            chat.remote_im_active_task_id.as_deref(),
            (!source_routed).then_some("task-goal")
        );
    }
}
