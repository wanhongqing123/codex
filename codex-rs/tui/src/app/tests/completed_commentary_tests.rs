use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn authoritative_commentary_survives_empty_stream_and_keeps_surrounding_history() -> Result<()>
{
    for delta in [
        "",
        "\n",
        "partial",
        "最新基线仍为主仓 `3361f3c501`、core `8d13788497`。\n",
    ] {
        let (mut app, mut rx, _op_rx) = make_test_app_with_channels().await;
        let thread_id = ThreadId::new();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        app.transcript_cells = vec![plain_line_cell("previous tool output")];
        app.chat_widget.handle_server_notification(
            agent_message_delta_notification(thread_id, "turn-1", "message-1", delta),
            /*replay_kind*/ None,
        );
        let response = "最新基线仍为主仓 `3361f3c501`、core `8d13788497`。";
        app.chat_widget.handle_server_notification(
            ServerNotification::ItemCompleted(codex_app_server_protocol::ItemCompletedNotification {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: 0,
                item: serde_json::from_value(serde_json::json!({
                    "type": "agentMessage", "id": "message-1", "text": response, "phase": "commentary"
                }))?,
            }),
            /*replay_kind*/ None,
        );
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::InsertHistoryCell(cell) => app.insert_history_cell(&mut tui, cell),
                AppEvent::ConsolidateAgentMessage {
                    message_id,
                    source,
                    cwd,
                    inline_visualization_context,
                    scrollback_reflow,
                    deferred_history_cell,
                } => app.handle_consolidate_agent_message(
                    &mut tui,
                    message_id,
                    source,
                    cwd,
                    inline_visualization_context,
                    scrollback_reflow,
                    deferred_history_cell,
                )?,
                _ => {}
            }
        }
        app.transcript_cells
            .push(plain_line_cell("next tool output"));
        let rendered = app
            .render_transcript_lines_for_reflow(/*width*/ 120)
            .lines
            .iter()
            .map(rendered_line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("previous tool output"), "{rendered}");
        assert!(
            rendered.contains("3361f3c501"),
            "delta={delta:?}, transcript={rendered}"
        );
        assert!(rendered.contains("next tool output"), "{rendered}");
        assert_eq!(rendered.matches("3361f3c501").count(), 1);
    }
    Ok(())
}
