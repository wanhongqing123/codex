//! Async questions are completed messages, not completed turns. Exercise the
//! live notification path through the real IM payload construction boundary.

use super::*;
use crate::multi_ai_code_im_bridge::test_capture::Capture;
use codex_protocol::items::AgentMessageDelivery;
use codex_protocol::items::AsyncUserInputQuestion;
use pretty_assertions::assert_eq;
use serde_json::json;

fn async_question() -> AppServerThreadItem {
    AppServerThreadItem::AgentMessage {
        id: "call-async-question".to_string(),
        text: "Which issue?\n- Issue A\n- Issue B\n\nAny other constraint?".to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: Some(AgentMessageDelivery::Async),
        questions: Some(vec![
            AsyncUserInputQuestion {
                title: "Which issue?".to_string(),
                options: Some(vec!["Issue A".to_string(), "Issue B".to_string()]),
            },
            AsyncUserInputQuestion {
                title: "Any other constraint?".to_string(),
                options: None,
            },
        ]),
    }
}

fn deliver(chat: &mut ChatWidget, item: AppServerThreadItem, replay_kind: Option<ReplayKind>) {
    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
            item,
        }),
        replay_kind,
    );
}

fn bind_source_route(chat: &mut ChatWidget) -> RemoteImTurnRoute {
    let route = RemoteImTurnRoute {
        reply_id: "reply-a".to_string(),
        task_id: Some("task-a".to_string()),
        source_routed: true,
    };
    chat.remote_im_forwarding_active = true;
    chat.remote_im_active_reply_id = Some(route.reply_id.clone());
    chat.remote_im_active_task_id = route.task_id.clone();
    handle_turn_started(chat, "turn-1");
    chat.remember_remote_im_turn_route_if_absent("turn-1".to_string(), route.clone());
    route
}

#[tokio::test]
async fn remote_im_async_question_is_sent_immediately_without_ending_the_route() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let route = bind_source_route(&mut chat);
    let capture = Capture::start();
    deliver(&mut chat, async_question(), /*replay_kind*/ None);
    assert_eq!(
        capture.drain(),
        vec![(
            json!({
                "kind": "assistant_text",
                "text": "Which issue?\n- Issue A\n- Issue B\n\nAny other constraint?",
                "replyId": "reply-a", "taskId": "task-a"
            }),
            "call-async-question".to_string()
        )]
    );
    assert_eq!(chat.remote_im_route_for_turn("turn-1"), Some(route));
    assert_eq!(chat.remote_im_active_task_id.as_deref(), Some("task-a"));

    let mut answer = async_question();
    if let AppServerThreadItem::AgentMessage {
        id,
        text,
        phase,
        delivery,
        questions,
        ..
    } = &mut answer
    {
        *id = "commentary-after-question".to_string();
        *text = "Continuing work.".to_string();
        *phase = Some(MessagePhase::Commentary);
        *delivery = None;
        *questions = None;
    }
    deliver(&mut chat, answer.clone(), /*replay_kind*/ None);
    assert_eq!(
        capture.drain(),
        vec![(
            json!({
                "kind": "assistant_text", "text": "Continuing work.",
                "replyId": "reply-a", "taskId": "task-a"
            }),
            "commentary-after-question".to_string()
        )]
    );
    if let AppServerThreadItem::AgentMessage {
        id, text, phase, ..
    } = &mut answer
    {
        *id = "real-final".to_string();
        *text = "Done.".to_string();
        *phase = Some(MessagePhase::FinalAnswer);
    }
    deliver(&mut chat, answer.clone(), /*replay_kind*/ None);
    assert_eq!(
        capture.drain(),
        vec![(
            json!({
                "kind": "assistant_text", "text": "Done.", "replyId": "reply-a", "taskId": "task-a"
            }),
            "real-final".to_string()
        )]
    );
    let mut turn = app_server_turn(
        "turn-1",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    turn.items = vec![async_question(), answer];
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn,
        }),
        /*replay_kind*/ None,
    );
    assert_eq!(
        capture.drain(),
        vec![(
            json!({
                "kind": "assistant_final", "text": "Done.",
                "replyId": "reply-a", "taskId": "task-a"
            }),
            "real-final:final".to_string()
        )]
    );
    assert_eq!(chat.remote_im_active_task_id.as_deref(), Some("task-a"));
}

#[tokio::test]
async fn remote_im_async_question_replay_is_not_resent() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    bind_source_route(&mut chat);
    let capture = Capture::start();
    chat.replay_thread_item(
        async_question(),
        "turn-1".to_string(),
        ReplayKind::ResumeInitialMessages,
    );
    assert_eq!(capture.drain(), Vec::new());
}

#[tokio::test]
async fn remote_im_async_question_without_a_route_is_not_sent_to_another_recipient() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let capture = Capture::start();
    deliver(&mut chat, async_question(), /*replay_kind*/ None);
    assert_eq!(capture.drain(), Vec::new());
}

#[tokio::test]
async fn remote_im_all_answers_are_ordinary_messages_and_idle_does_not_stop_forwarding() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    bind_source_route(&mut chat);
    let capture = Capture::start();
    let answer = |id: &str, text: &str| AppServerThreadItem::AgentMessage {
        id: id.to_string(),
        text: text.to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    };
    let first = answer("greeting", "你好，我在。");
    let last = answer("recall", "记得以前的任务。");
    for item in [first.clone(), last.clone()] {
        deliver(&mut chat, item, /*replay_kind*/ None);
    }
    assert_eq!(
        capture.drain(),
        vec![
            (
                json!({"kind":"assistant_text", "text":"你好，我在。", "replyId":"reply-a", "taskId":"task-a"}),
                "greeting".to_string()
            ),
            (
                json!({"kind":"assistant_text", "text":"记得以前的任务。", "replyId":"reply-a", "taskId":"task-a"}),
                "recall".to_string()
            ),
        ]
    );
    let mut turn = app_server_turn(
        "turn-1",
        AppServerTurnStatus::Completed,
        /*duration_ms*/ None,
        /*error*/ None,
    );
    turn.items = vec![first.clone(), last.clone()];
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: turn.clone(),
        }),
        /*replay_kind*/ None,
    );
    assert_eq!(
        capture.drain(),
        vec![(
            json!({"kind":"assistant_final", "text":"记得以前的任务。", "replyId":"reply-a", "taskId":"task-a"}),
            "recall:final".to_string()
        )]
    );
    assert!(chat.remote_im_forwarding_active);
    assert_eq!(chat.remote_im_active_task_id.as_deref(), Some("task-a"));
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn,
        }),
        Some(ReplayKind::ResumeInitialMessages),
    );
    assert_eq!(capture.drain(), Vec::new());
    chat.remember_remote_im_turn_route_if_absent(
        "turn-1".to_string(),
        RemoteImTurnRoute {
            reply_id: "reply-a".to_string(),
            task_id: Some("task-a".to_string()),
            source_routed: true,
        },
    );
    chat.set_remote_im_input_origin(false);
    capture.drain();
    deliver(
        &mut chat,
        answer("late", "Must not leak after input takeover"),
        /*replay_kind*/ None,
    );
    assert_eq!(capture.drain(), Vec::new());
}
