//! Transcript consolidation for finalized streaming agent messages.
//!
//! During streaming, the chat widget emits transient `AgentMessageCell`s so it
//! can animate stable lines into scrollback while keeping the active mutable
//! tail in the bottom pane. Once the answer finishes, the app replaces that
//! trailing run with a single source-backed `AgentMarkdownCell`. This makes the
//! transcript the canonical owner of the raw markdown source used for future
//! resize re-renders. The retained view preserves its displayed revision before that
//! replacement, then repaints without replaying terminal scrollback.

use std::sync::Arc;

use color_eyre::eyre::Result;

use super::App;
use super::resize_reflow::trailing_run_start;
use crate::app_event::ConsolidationScrollbackReflow;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::pager_overlay::Overlay;
use crate::tui;

impl App {
    pub(super) fn handle_consolidate_agent_message(
        &mut self,
        tui: &mut tui::Tui,
        message_id: Option<String>,
        source: history_cell::AgentMarkdownCell,
        scrollback_reflow: ConsolidationScrollbackReflow,
        deferred_history_cell: Option<Box<dyn HistoryCell>>,
    ) -> Result<()> {
        // Some finalize paths must preserve a last provisional stream cell long
        // enough for queue ordering, then fold it into the canonical
        // source-backed cell during consolidation.
        if let Some(cell) = deferred_history_cell {
            let cell: Arc<dyn HistoryCell> = cell.into();
            if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                t.insert_cell(cell.clone());
            }
            if !tui.is_owned_screen() {
                self.native_history.defer(&cell);
            }
            self.transcript_cells.push(cell);
        }

        // Walk backward to find the contiguous run of streaming AgentMessageCells that
        // belong to the just-finalized stream.
        let end = self.transcript_cells.len();
        let source_length = source.copy_source().map_or(/*default*/ 0, str::len);
        let has_source = source
            .copy_source()
            .is_some_and(|text| !text.trim().is_empty());
        tracing::debug!(
            "ConsolidateAgentMessage: transcript_cells.len()={end}, source_len={}",
            source_length
        );
        let start = trailing_run_start::<history_cell::AgentMessageCell>(&self.transcript_cells);
        let outcome = if start < end {
            "history_replaced"
        } else if has_source {
            "history_inserted"
        } else {
            "empty_completion"
        };
        if start < end {
            tracing::debug!(
                "ConsolidateAgentMessage: replacing cells [{start}..{end}] with AgentMarkdownCell"
            );
            let consolidated: Arc<dyn HistoryCell> = Arc::new(source);
            self.native_history
                .consolidate(&self.transcript_cells[start..end], &consolidated);
            if tui.is_owned_screen() {
                self.transcript_view.replace_group(
                    &self.transcript_cells,
                    start..end,
                    &consolidated,
                );
            } else {
                self.transcript_view.replace_range(
                    &self.transcript_cells,
                    start..end,
                    &consolidated,
                );
            }
            self.transcript_cells
                .splice(start..end, std::iter::once(consolidated.clone()));

            if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                t.regroup_cells(start..end, consolidated.clone());
                tui.frame_requester().schedule_frame();
            }

            self.finish_agent_message_consolidation(tui, scrollback_reflow)?;
        } else if has_source {
            // A stream controller can exist without having emitted a history
            // cell (for example after an empty delta). Completion still carries
            // authoritative text: preserve it in both scrollback and Ctrl+T.
            tracing::warn!(
                source_len = source_length,
                transcript_cells = end,
                "ConsolidateAgentMessage: inserting completion without provisional cells"
            );
            self.insert_history_cell(tui, Box::new(source));
            self.finish_agent_message_consolidation(tui, scrollback_reflow)?;
        } else {
            self.maybe_finish_stream_reflow(tui)?;
        }

        tracing::info!(target: "codex_tui::history_diagnostics", "tui_history {}", serde_json::json!({
            "event": "tui_history", "stage": outcome, "messageId": message_id,
            "textLength": source_length, "cellCount": self.transcript_cells.len(), "replacedCells": end - start
        }));
        Ok(())
    }

    fn finish_agent_message_consolidation(
        &mut self,
        tui: &mut tui::Tui,
        scrollback_reflow: ConsolidationScrollbackReflow,
    ) -> Result<()> {
        if tui.is_owned_screen() {
            self.transcript_reflow.clear_pending_reflow();
            self.transcript_reflow.clear_stream_flags();
            tui.frame_requester().schedule_frame();
            return Ok(());
        }
        match scrollback_reflow {
            ConsolidationScrollbackReflow::IfResizeReflowRan => {
                self.maybe_finish_stream_reflow(tui)?;
            }
            ConsolidationScrollbackReflow::Required => {
                self.finish_required_stream_reflow(tui)?;
            }
        }

        Ok(())
    }
}
