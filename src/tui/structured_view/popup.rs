use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::embedded::{EmbeddedEvent, EmbeddedView};
use super::input::Focus;
use crate::acp::client::http::{ChatSession, MessageResult};
use crate::tui::components::{ListPicker, ListPickerResult};
use crate::tui::dialogs::{DialogResult, SendMessageDialog};
use crate::tui::styles::Theme;

pub struct ChatStartup {
    pub command: String,
    pub profile: Option<String>,
    pub visible: bool,
    pub result: Option<tokio::sync::oneshot::Receiver<Result<ChatPopup>>>,
    pub error: Option<String>,
    title: String,
}

impl ChatStartup {
    pub fn new(command: String, title: String, profile: Option<String>) -> Self {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let chat_command = command.clone();
        let chat_profile = profile.clone();
        let chat_title = title.clone();
        tokio::spawn(async move {
            let result = async {
                let endpoint = crate::acp::client::require_daemon().await?;
                let http = crate::acp::client::HttpClient::new(endpoint.clone())?;
                let resolved_profile = crate::session::config::effective_profile(
                    chat_profile.as_deref().unwrap_or(""),
                );
                let session = http
                    .open_plugin_chat(&chat_command, &resolved_profile)
                    .await?;
                let view = EmbeddedView::connect(endpoint, &session.id).await?;
                Ok(ChatPopup::new(chat_command, chat_title, chat_profile, view))
            }
            .await;
            let _ = tx.send(result);
        });
        Self {
            command,
            profile,
            visible: true,
            result: Some(rx),
            error: None,
            title,
        }
    }

    pub fn render(&self, frame: &mut Frame, theme: &Theme) {
        if !self.visible {
            return;
        }
        let area = crate::tui::dialogs::centered_rect(frame.area(), 72, 7);
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.background).fg(theme.text))
            .border_style(Style::default().fg(theme.accent))
            .title(format!(" {} ", self.title));
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n\n{}",
                self.error.as_deref().unwrap_or("Starting agent…"),
                if self.error.is_some() {
                    "Esc closes; reopen to retry."
                } else {
                    "Esc closes; startup continues in the background."
                },
            ))
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: false }),
            area,
        );
    }
}

enum MessageStep {
    Chat,
    Picker(ListPicker),
    Editor {
        recipient: ChatSession,
        editor: Box<SendMessageDialog>,
        locked: bool,
    },
}

pub struct ChatPopup {
    pub view: EmbeddedView,
    pub command: String,
    pub profile: Option<String>,
    pub visible: bool,
    title: String,
    step: MessageStep,
    targets: Vec<ChatSession>,
    saved_editor: Option<(String, Box<SendMessageDialog>, bool)>,
    typed_draft: Option<String>,
    notice: Option<String>,
    message_result: Option<tokio::sync::oneshot::Receiver<Result<MessageResult>>>,
}

impl ChatPopup {
    pub fn new(
        command: String,
        title: String,
        profile: Option<String>,
        mut view: EmbeddedView,
    ) -> Self {
        view.activate();
        Self {
            view,
            command,
            profile,
            title,
            visible: true,
            step: MessageStep::Chat,
            targets: Vec::new(),
            saved_editor: None,
            typed_draft: None,
            notice: None,
            message_result: None,
        }
    }

    pub fn reopen(&mut self) {
        self.visible = true;
        self.view.activate();
    }

    fn picker(&mut self) {
        let mut picker = ListPicker::new("Message recipient");
        picker.activate(self.targets.iter().map(target_label).collect());
        self.step = MessageStep::Picker(picker);
    }

    pub async fn next_event(&mut self) -> Option<EmbeddedEvent> {
        tokio::select! {
            event = self.view.next_event() => Some(event),
            result = async {
                match self.message_result.as_mut() {
                    Some(result) => result.await,
                    None => std::future::pending().await,
                }
            } => {
                self.message_result = None;
                self.apply_message_result(result.unwrap_or_else(|error| Err(error.into())));
                None
            }
        }
    }

    fn apply_message_result(&mut self, result: Result<MessageResult>) {
        match result {
            Ok(result) => {
                let accepted = matches!(result.status.as_str(), "sent" | "steered" | "queued");
                self.notice = Some(result.message.unwrap_or_else(|| {
                    if accepted {
                        format!(
                            "{}: input accepted; execution is not confirmed.",
                            result.status
                        )
                    } else {
                        format!(
                            "{}: delivery not confirmed. Draft retained; Esc goes back.",
                            result.status
                        )
                    }
                }));
                if accepted {
                    self.step = MessageStep::Chat;
                } else if result.status == "error" {
                    if let MessageStep::Editor { locked, .. } = &mut self.step {
                        *locked = false;
                    }
                }
            }
            Err(error) => {
                self.notice = Some(format!(
                    "Result unknown: {error}. Draft retained; verify recipient before retrying. Esc goes back."
                ));
            }
        }
    }

    pub async fn handle_event(&mut self, event: Event) -> Result<()> {
        if self.message_result.is_some() {
            return Ok(());
        }
        if matches!(&event, Event::Key(key) if key.code == KeyCode::Esc)
            && matches!(self.step, MessageStep::Editor { .. })
        {
            if let MessageStep::Editor {
                recipient,
                editor,
                locked,
            } = std::mem::replace(&mut self.step, MessageStep::Chat)
            {
                self.saved_editor = Some((recipient.id, editor, locked));
            }
            self.picker();
            return Ok(());
        }
        match &mut self.step {
            MessageStep::Picker(picker) => {
                if let Event::Key(key) = event {
                    match picker.handle_key(key) {
                        ListPickerResult::Cancelled => self.step = MessageStep::Chat,
                        ListPickerResult::Selected(label) => {
                            if let Some(recipient) = self
                                .targets
                                .iter()
                                .find(|t| target_label(t) == label)
                                .cloned()
                            {
                                let (editor, locked) = match self.saved_editor.take() {
                                    Some((id, editor, locked)) if id == recipient.id => {
                                        (editor, locked)
                                    }
                                    _ => {
                                        (Box::new(SendMessageDialog::new(&recipient.title)), false)
                                    }
                                };
                                if !locked {
                                    self.notice = None;
                                }
                                self.step = MessageStep::Editor {
                                    recipient,
                                    editor,
                                    locked,
                                };
                            }
                        }
                        ListPickerResult::Continue => {}
                    }
                }
                return Ok(());
            }
            MessageStep::Editor {
                recipient,
                editor,
                locked,
            } => {
                match event {
                    Event::Paste(text) if !*locked => editor.handle_paste(&text),
                    Event::Key(key) if !*locked => {
                        if key.code == KeyCode::Enter && key.kind == KeyEventKind::Repeat {
                            return Ok(());
                        }
                        if let DialogResult::Submit(text) = editor.handle_exact_key(key) {
                            *locked = true;
                            let http = self.view.state.http.clone();
                            let source = self.view.session_id().to_string();
                            let target = recipient.id.clone();
                            let (tx, rx) = tokio::sync::oneshot::channel();
                            self.message_result = Some(rx);
                            self.notice = Some("Sending… Awaiting host acknowledgment.".into());
                            tokio::spawn(async move {
                                let result = http
                                    .send_session_message(&source, &target, &text)
                                    .await
                                    .map_err(Into::into);
                                let _ = tx.send(result);
                            });
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }
            MessageStep::Chat => {}
        }
        if let Event::Key(key) = &event {
            if key.code == KeyCode::Esc && self.view.state.choice.is_none() {
                self.visible = false;
                return Ok(());
            }
            if key.code == KeyCode::Enter
                && key.modifiers.is_empty()
                && self.view.state.focus == Focus::Composer
                && self.view.state.choice.is_none()
                && message_command(
                    &self.view.state.composer.lines().join("\n"),
                    self.typed_draft.as_deref(),
                )
            {
                match self
                    .view
                    .state
                    .http
                    .message_targets(self.view.session_id())
                    .await
                {
                    Ok(targets) => {
                        self.targets = targets;
                        if !self
                            .saved_editor
                            .as_ref()
                            .is_some_and(|(_, _, locked)| *locked)
                        {
                            self.notice = None;
                        }
                        self.picker();
                    }
                    Err(error) => self.notice = Some(format!("Recipients unavailable: {error}")),
                }
                return Ok(());
            }
        }
        let typed = matches!(&event, Event::Key(key)
            if matches!(key.code, KeyCode::Char(_))
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                && self.view.state.focus == Focus::Composer
                && self.view.state.choice.is_none());
        let before = self.view.state.composer.lines().join("\n");
        if self.view.handle_event(event).await? {
            self.visible = false;
        }
        let after = self.view.state.composer.lines().join("\n");
        self.typed_draft = (typed
            && (before.is_empty() || self.typed_draft.as_deref() == Some(before.as_str())))
        .then_some(after);
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, theme: &Theme) {
        if !self.visible {
            return;
        }
        let screen = frame.area();
        let area = crate::tui::dialogs::centered_rect(
            screen,
            screen.width.saturating_sub(4),
            screen.height.saturating_sub(2),
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.background).fg(theme.text))
            .border_style(Style::default().fg(theme.accent))
            .title(format!(
                " {} · {} · {} · {} ",
                self.title,
                if self.view.state.ws.is_none() {
                    "disconnected"
                } else if self.view.state.transcript.turn_active {
                    "running"
                } else {
                    "ready"
                },
                self.view
                    .state
                    .transcript
                    .agent_name
                    .as_deref()
                    .unwrap_or("starting"),
                self.view
                    .state
                    .transcript
                    .model_name
                    .as_deref()
                    .unwrap_or("model pending"),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if screen.width < 80 || screen.height < 24 {
            frame.render_widget(
                Paragraph::new("Resize to at least 80×24. Esc closes."),
                inner,
            );
            return;
        }
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
        let hints = match &mut self.step {
            MessageStep::Chat => {
                self.view.render(frame, rows[0], theme);
                "Enter send · Alt+Enter newline · Ctrl+C stop · Esc close · /message"
            }
            MessageStep::Picker(picker) => {
                picker.render_body(frame, rows[0], theme);
                "↑/↓ select · Enter edit · Esc back"
            }
            MessageStep::Editor {
                recipient,
                editor,
                locked,
            } => {
                let editor_rows =
                    Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(rows[0]);
                frame.render_widget(
                    Paragraph::new(format!(
                        "To: {} [{}]\n{}\n{}",
                        recipient.title, recipient.id, recipient.project_path, recipient.group_path,
                    )),
                    editor_rows[0],
                );
                editor.render_body(frame, editor_rows[1], theme);
                if self.message_result.is_some() {
                    "Sending · Waiting for acknowledgment"
                } else if *locked {
                    "Submission locked · Esc back"
                } else {
                    "Enter Send · Alt/Shift+Enter newline · Esc back"
                }
            }
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n{}",
                self.notice.as_deref().unwrap_or(""),
                hints
            ))
            .style(Style::default().fg(theme.dimmed)),
            rows[1],
        );
    }
}

fn target_label(target: &ChatSession) -> String {
    format!(
        "{} [{}] {} {} {}",
        target.title, target.id, target.status, target.project_path, target.group_path
    )
}

fn message_command(text: &str, typed_draft: Option<&str>) -> bool {
    typed_draft == Some(text) && text == "/message"
}

#[cfg(test)]
mod tests {
    use super::{message_command, ChatStartup};
    use crate::tui::styles::Theme;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn startup_and_error_keep_escape_visible_at_supported_sizes() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut startup = ChatStartup {
            command: "plugin.example.chat.open".into(),
            profile: None,
            visible: true,
            result: Some(rx),
            error: None,
            title: "Councilor".into(),
        };
        for (width, height) in [(80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for error in [None, Some("Configure a compatible agent".to_string())] {
                startup.error = error;
                terminal
                    .draw(|frame| startup.render(frame, &Theme::default()))
                    .unwrap();
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains("Councilor"));
                assert!(text.contains("Esc closes"));
                assert!(text.contains(startup.error.as_deref().unwrap_or("Starting agent…")));
            }
        }
    }

    #[test]
    fn only_direct_standalone_message_opens_picker() {
        for (text, typed_draft, expected) in [
            ("/message", Some("/message"), true),
            ("/message", None, false),
            ("/message", Some("draft before recall"), false),
            ("\"/message\"", Some("\"/message\""), false),
            ("/message hello", Some("/message hello"), false),
            ("transcript\n/message", None, false),
        ] {
            assert_eq!(message_command(text, typed_draft), expected, "{text:?}");
        }
    }
}
