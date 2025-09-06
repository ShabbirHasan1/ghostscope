use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    Terminal,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::{
    events::{EventRegistry, RuntimeCommand, RuntimeStatus, SourceCodeInfo, TuiEvent},
    panels::{
        CommandAction, EbpfInfoPanel, InteractiveCommandPanel, ResponseType, SourceCodePanel,
    },
};

pub struct TuiApp {
    should_quit: bool,

    // UI panels
    source_panel: SourceCodePanel,
    ebpf_info_panel: EbpfInfoPanel,
    interactive_command_panel: InteractiveCommandPanel,

    // Layout state
    layout_mode: LayoutMode,
    focused_panel: FocusedPanel,
    expecting_window_nav: bool,

    // Ctrl+w timeout mechanism
    ctrl_w_timeout: Option<std::time::Instant>,

    // Event communication
    event_registry: EventRegistry,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FocusedPanel {
    Source,             // Source Code Panel
    EbpfInfo,           // eBPF Information Display Panel
    InteractiveCommand, // Command Interactive Window Panel
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayoutMode {
    Horizontal,
    Vertical,
}

impl TuiApp {
    pub async fn new(event_registry: EventRegistry, layout_mode: LayoutMode) -> Result<Self> {
        Ok(Self {
            should_quit: false,
            source_panel: SourceCodePanel::new(),
            ebpf_info_panel: EbpfInfoPanel::new(),
            interactive_command_panel: InteractiveCommandPanel::new(),
            layout_mode,
            focused_panel: FocusedPanel::InteractiveCommand,
            expecting_window_nav: false,
            ctrl_w_timeout: None,
            event_registry,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        info!("Starting TUI mode");

        // Create async event stream
        let mut event_stream = EventStream::new();
        let mut needs_render = true;

        // Initial render
        if let Err(e) = terminal.draw(|f| self.render(f)) {
            error!("Failed to render initial TUI: {}", e);
            return Err(e.into());
        }

        // Main event loop with event-driven rendering
        let result = loop {
            tokio::select! {
                // Check for Ctrl+w timeout
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if let Some(timeout) = self.ctrl_w_timeout {
                        if timeout.elapsed().as_millis() >= 300 {
                            // Timeout reached, execute delete word action
                            debug!("Ctrl+w timeout reached, executing delete word action");
                            if self.focused_panel == FocusedPanel::InteractiveCommand {
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.delete_previous_word();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel.delete_previous_word_in_script();
                                    }
                                    _ => {} // Should not happen
                                }
                            }
                            self.ctrl_w_timeout = None;
                            self.expecting_window_nav = false;
                            needs_render = true;
                        }
                    }
                }

                // Handle crossterm events (keyboard, mouse, resize)
                Some(event_result) = event_stream.next() => {
                    match event_result {
                        Ok(event) => {
                            match event {
                                Event::Key(key) => {
                                    if self.handle_key_event(key).await? {
                                        break Ok(()); // Quit requested
                                    }
                                    needs_render = true;
                                }
                                Event::Mouse(mouse) => {
                                    debug!("Mouse event: {:?}", mouse);
                                    // TODO: Handle mouse events for panel resizing
                                }
                                Event::Resize(_, _) => {
                                    needs_render = true;
                                }
                                _ => {}
                            }
                        }
                        Err(e) => {
                            error!("Error reading terminal events: {}", e);
                            break Err(e.into());
                        }
                    }
                }

                // Handle runtime status updates
                Some(status) = self.event_registry.status_receiver.recv() => {
                    self.handle_runtime_status(status).await;
                    needs_render = true;
                }

                // Handle trace events
                Some(trace_event) = self.event_registry.trace_receiver.recv() => {
                    self.handle_trace_event(trace_event).await;
                    needs_render = true;
                }
            }

            // Render only when needed (event-driven)
            if needs_render {
                if let Err(e) = terminal.draw(|f| self.render(f)) {
                    error!("Failed to render TUI: {}", e);
                    break Err(e.into());
                }
                needs_render = false;
            }

            if self.should_quit {
                break Ok(());
            }
        };

        // Cleanup terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        info!("TUI mode exited");
        result
    }

    async fn handle_key_event(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
        // Handle window navigation keys if expecting them
        if self.expecting_window_nav {
            match key.code {
                KeyCode::Char('j') => {
                    // Vertical layout: j moves focus down
                    if self.layout_mode == LayoutMode::Vertical {
                        self.move_focus("down");
                    }
                    debug!("Window nav: moved focus down to {:?}", self.focused_panel);
                    self.expecting_window_nav = false;
                    self.ctrl_w_timeout = None; // Clear timeout
                    return Ok(false);
                }
                KeyCode::Char('k') => {
                    // Vertical layout: k moves focus up
                    if self.layout_mode == LayoutMode::Vertical {
                        self.move_focus("up");
                    }
                    debug!("Window nav: moved focus up to {:?}", self.focused_panel);
                    self.expecting_window_nav = false;
                    self.ctrl_w_timeout = None; // Clear timeout
                    return Ok(false);
                }
                KeyCode::Char('h') => {
                    // Horizontal layout: h moves focus left
                    if self.layout_mode == LayoutMode::Horizontal {
                        self.move_focus("left");
                    }
                    debug!("Window nav: moved focus left to {:?}", self.focused_panel);
                    self.expecting_window_nav = false;
                    self.ctrl_w_timeout = None; // Clear timeout
                    return Ok(false);
                }
                KeyCode::Char('l') => {
                    // Horizontal layout: l moves focus right
                    if self.layout_mode == LayoutMode::Horizontal {
                        self.move_focus("right");
                    }
                    debug!("Window nav: moved focus right to {:?}", self.focused_panel);
                    self.expecting_window_nav = false;
                    self.ctrl_w_timeout = None; // Clear timeout
                    return Ok(false);
                }
                KeyCode::Char('v') => {
                    // Switch layout mode
                    self.switch_layout();
                    debug!("Switched layout to {:?}", self.layout_mode);
                    self.expecting_window_nav = false;
                    self.ctrl_w_timeout = None; // Clear timeout
                    return Ok(false);
                }
                _ => {
                    // Any other key cancels window navigation expectation
                    self.expecting_window_nav = false;
                    self.ctrl_w_timeout = None; // Clear timeout
                                                // Fall through to normal key handling
                }
            }
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                info!("Quit requested via Ctrl+C");
                let _ = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::Shutdown);
                return Ok(true);
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                info!("Quit requested via Ctrl+Q");
                let _ = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::Shutdown);
                return Ok(true);
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Handle Ctrl+w with timeout mechanism
                if self.focused_panel == FocusedPanel::InteractiveCommand {
                    // In Interactive Command panel, check mode
                    match self.interactive_command_panel.mode {
                        crate::panels::InteractionMode::Input => {
                            // Set timeout for potential window navigation
                            self.ctrl_w_timeout = Some(std::time::Instant::now());
                            self.expecting_window_nav = true;
                            debug!("Ctrl+w pressed in Input mode, waiting for navigation key or timeout");
                        }
                        crate::panels::InteractionMode::ScriptEditor => {
                            // Set timeout for potential window navigation
                            self.ctrl_w_timeout = Some(std::time::Instant::now());
                            self.expecting_window_nav = true;
                            debug!("Ctrl+w pressed in Script mode, waiting for navigation key or timeout");
                        }
                        _ => {
                            // In command mode, use for window navigation immediately
                            self.expecting_window_nav = true;
                            debug!("Expecting window navigation key (j/k)");
                        }
                    }
                } else {
                    // In other panels, use for window navigation immediately
                    self.expecting_window_nav = true;
                    debug!("Expecting window navigation key (j/k)");
                }
            }
            KeyCode::Tab => {
                // In Script Editor mode, Tab should insert 4 spaces instead of cycling focus
                if self.focused_panel == FocusedPanel::InteractiveCommand
                    && self.interactive_command_panel.mode
                        == crate::panels::InteractionMode::ScriptEditor
                {
                    debug!("Tab pressed in Script Editor mode, inserting 4 spaces");
                    self.interactive_command_panel.insert_tab_in_script();
                } else {
                    // Normal Tab behavior: cycle focus between panels
                    self.cycle_focus();
                }
            }
            KeyCode::BackTab => {
                // Shift+Tab: reverse cycle focus between panels
                // In Script Editor mode, we still allow reverse cycling for navigation
                debug!("Shift+Tab pressed, reverse cycling focus");
                self.cycle_focus_reverse();
            }
            _ => {
                self.handle_panel_input(key).await?;
            }
        }
        Ok(false)
    }

    async fn handle_panel_input(&mut self, key: crossterm::event::KeyEvent) -> Result<()> {
        match self.focused_panel {
            FocusedPanel::InteractiveCommand => match key.code {
                KeyCode::Char(c) => {
                    // Handle Ctrl+key combinations
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match c {
                            's' => {
                                if self.interactive_command_panel.mode
                                    == crate::panels::InteractionMode::ScriptEditor
                                {
                                    // Submit script with Ctrl+S
                                    if let Some(action) =
                                        self.interactive_command_panel.submit_script()
                                    {
                                        self.handle_command_action(action).await?;
                                    }
                                }
                            }
                            'a' => {
                                // Move to beginning of line
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.move_to_beginning();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel
                                            .move_to_beginning_in_script();
                                    }
                                    _ => {} // Ignore in command mode
                                }
                            }
                            'e' => {
                                // Move to end of line
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.move_to_end();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel.move_to_end_in_script();
                                    }
                                    _ => {} // Ignore in command mode
                                }
                            }
                            'h' => {
                                // Delete character before cursor (Backspace)
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.delete_char();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel.delete_char_in_script();
                                    }
                                    _ => {} // Ignore in command mode
                                }
                            }
                            'b' => {
                                // Move cursor left one character
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.move_cursor_left();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel.move_cursor_left_in_script();
                                    }
                                    _ => {} // Ignore in command mode
                                }
                            }
                            'f' => {
                                // Move cursor right one character
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.move_cursor_right();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel
                                            .move_cursor_right_in_script();
                                    }
                                    _ => {} // Ignore in command mode
                                }
                            }
                            'k' => {
                                // Delete from cursor to end of line
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.delete_to_end();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel.delete_to_end_in_script();
                                    }
                                    _ => {} // Ignore in command mode
                                }
                            }
                            'u' => {
                                // Delete from cursor to beginning of line
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Input => {
                                        self.interactive_command_panel.delete_to_beginning();
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        self.interactive_command_panel
                                            .delete_to_beginning_in_script();
                                    }
                                    _ => {} // Ignore in command mode
                                }
                            }
                            'i' => {
                                // Insert tab (4 spaces) - only in Script Editor mode
                                debug!(
                                    "Ctrl+i pressed, mode: {:?}",
                                    self.interactive_command_panel.mode
                                );
                                if self.interactive_command_panel.mode
                                    == crate::panels::InteractionMode::ScriptEditor
                                {
                                    debug!("Inserting tab in script editor");
                                    self.interactive_command_panel.insert_tab_in_script();
                                } else {
                                    debug!("Not in script editor mode, ignoring Ctrl+i");
                                }
                            }
                            _ => {
                                // For other Ctrl+key combinations, treat as regular character input
                                match self.interactive_command_panel.mode {
                                    crate::panels::InteractionMode::Command => {
                                        // Handle command navigation keys
                                        let key_str = c.to_string();
                                        self.interactive_command_panel
                                            .handle_vim_navigation(&key_str);
                                    }
                                    crate::panels::InteractionMode::ScriptEditor => {
                                        // Insert character in script
                                        self.interactive_command_panel.insert_char_in_script(c);
                                    }
                                    crate::panels::InteractionMode::Input => {
                                        // In input mode, all characters are inserted as input
                                        self.interactive_command_panel.insert_char(c);
                                    }
                                }
                            }
                        }
                    } else {
                        // Regular character input (no Ctrl modifier)
                        match self.interactive_command_panel.mode {
                            crate::panels::InteractionMode::Command => {
                                // Handle command navigation keys
                                let key_str = c.to_string();
                                self.interactive_command_panel
                                    .handle_vim_navigation(&key_str);
                            }
                            crate::panels::InteractionMode::ScriptEditor => {
                                // Insert character in script
                                self.interactive_command_panel.insert_char_in_script(c);
                            }
                            crate::panels::InteractionMode::Input => {
                                // In input mode, all characters are inserted as input
                                self.interactive_command_panel.insert_char(c);
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    match self.interactive_command_panel.mode {
                        crate::panels::InteractionMode::Input => {
                            self.interactive_command_panel.delete_char();
                        }
                        crate::panels::InteractionMode::ScriptEditor => {
                            self.interactive_command_panel.delete_char_in_script();
                        }
                        crate::panels::InteractionMode::Command => {
                            // Ignore backspace in command mode
                        }
                    }
                }
                KeyCode::Left => match self.interactive_command_panel.mode {
                    crate::panels::InteractionMode::Command => {
                        self.interactive_command_panel.handle_vim_navigation("h");
                    }
                    crate::panels::InteractionMode::ScriptEditor => {
                        self.interactive_command_panel.move_cursor_left_in_script();
                    }
                    crate::panels::InteractionMode::Input => {
                        self.interactive_command_panel.move_cursor_left();
                    }
                },
                KeyCode::Right => match self.interactive_command_panel.mode {
                    crate::panels::InteractionMode::Command => {
                        self.interactive_command_panel.handle_vim_navigation("l");
                    }
                    crate::panels::InteractionMode::ScriptEditor => {
                        self.interactive_command_panel.move_cursor_right_in_script();
                    }
                    crate::panels::InteractionMode::Input => {
                        self.interactive_command_panel.move_cursor_right();
                    }
                },
                KeyCode::Up => match self.interactive_command_panel.mode {
                    crate::panels::InteractionMode::Command => {
                        self.interactive_command_panel.handle_vim_navigation("k");
                    }
                    crate::panels::InteractionMode::ScriptEditor => {
                        self.interactive_command_panel.move_cursor_up_in_script();
                    }
                    crate::panels::InteractionMode::Input => {
                        self.interactive_command_panel.history_up();
                    }
                },
                KeyCode::Down => match self.interactive_command_panel.mode {
                    crate::panels::InteractionMode::Command => {
                        self.interactive_command_panel.handle_vim_navigation("j");
                    }
                    crate::panels::InteractionMode::ScriptEditor => {
                        self.interactive_command_panel.move_cursor_down_in_script();
                    }
                    crate::panels::InteractionMode::Input => {
                        self.interactive_command_panel.history_down();
                    }
                },
                KeyCode::Enter => {
                    match self.interactive_command_panel.mode {
                        crate::panels::InteractionMode::ScriptEditor => {
                            // Normal Enter creates a new line
                            self.interactive_command_panel.insert_newline_in_script();
                        }
                        crate::panels::InteractionMode::Input
                        | crate::panels::InteractionMode::Command => {
                            // In other modes, Enter submits the command
                            if let Some(action) = self.interactive_command_panel.submit_command() {
                                self.handle_command_action(action).await?;
                            }
                        }
                    }
                }
                KeyCode::Esc => {
                    match self.interactive_command_panel.mode {
                        crate::panels::InteractionMode::Input => {
                            // Input mode: Enter command mode
                            self.interactive_command_panel.enter_command_mode();
                            debug!("Entered command mode via ESC from input mode");
                        }
                        crate::panels::InteractionMode::ScriptEditor => {
                            // Script editor mode: Cancel script editing and return to input mode
                            self.interactive_command_panel.cancel_script_editor();
                            info!("Script editing cancelled");
                        }
                        crate::panels::InteractionMode::Command => {
                            // Command mode: Do nothing (already in command mode)
                            debug!("ESC ignored in command mode");
                        }
                    }
                }
                KeyCode::F(2) => {
                    // F2: Re-enter script editing mode if script is submitted
                    if self.interactive_command_panel.can_edit_script() {
                        self.interactive_command_panel.edit_script_again();
                    }
                }
                KeyCode::F(3) => {
                    // F3: Clear current script
                    if self.interactive_command_panel.mode
                        == crate::panels::InteractionMode::ScriptEditor
                    {
                        self.interactive_command_panel.clear_current_script();
                    }
                }

                _ => {}
            },
            FocusedPanel::EbpfInfo => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.ebpf_info_panel.move_cursor_up();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.ebpf_info_panel.move_cursor_down();
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.ebpf_info_panel.move_cursor_down_10();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.ebpf_info_panel.move_cursor_up_10();
                }
                KeyCode::Esc => {
                    self.ebpf_info_panel.hide_cursor();
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.ebpf_info_panel.scroll_offset = 0;
                    self.ebpf_info_panel.auto_scroll = false;
                    self.ebpf_info_panel.show_cursor = false;
                    self.ebpf_info_panel.cursor_trace_index = 0;
                    self.ebpf_info_panel.display_mode = crate::panels::DisplayMode::AutoRefresh;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.ebpf_info_panel.scroll_to_bottom();
                }
                _ => {}
            },
            FocusedPanel::Source => {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.source_panel.move_up();
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.source_panel.move_down();
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        self.source_panel.move_left();
                    }
                    KeyCode::Right | KeyCode::Char('l') => {
                        self.source_panel.move_right();
                    }
                    KeyCode::Char('g') => {
                        self.source_panel.move_to_top();
                    }
                    KeyCode::Char('G') => {
                        self.source_panel.move_to_bottom();
                    }
                    _ => {}
                }

                // Handle Ctrl+key combinations
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('d') => {
                            self.source_panel.move_down_fast();
                        }
                        KeyCode::Char('u') => {
                            self.source_panel.move_up_fast();
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn parse_trace_script<'a>(&self, script: &'a str) -> Option<(&'a str, &'a str)> {
        // Expected format: "trace <target> <script_content>"
        if let Some(rest) = script.strip_prefix("trace ") {
            if let Some(space_pos) = rest.find(' ') {
                let target = &rest[..space_pos];
                let script_content = &rest[space_pos + 1..];
                Some((target, script_content))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn add_script_display(&mut self, target: &str, script_content: &str) {
        // Format the script display similar to script editor mode
        let mut script_lines = Vec::new();

        // Add header with target info
        script_lines.push(format!("Script for target: {}", target));
        script_lines.push("─".repeat(50));

        // Add script content with line numbers (similar to script editor)
        for (line_idx, line) in script_content.lines().enumerate() {
            script_lines.push(format!("{:3} │ {}", line_idx + 1, line));
        }

        script_lines.push("─".repeat(50));

        // Add the formatted script display as a response
        let script_display = script_lines.join("\n");
        self.interactive_command_panel
            .add_response(script_display, ResponseType::Info);
    }

    async fn handle_command_action(&mut self, action: CommandAction) -> Result<()> {
        match action {
            CommandAction::ExecuteCommand(command) => {
                self.execute_user_command(command).await?;
            }
            CommandAction::EnterScriptMode(message) => {
                info!("Entering script mode: {}", message);
                self.interactive_command_panel
                    .add_response(message, ResponseType::Success);
            }
            CommandAction::AddScriptLine(line) => {
                info!("Added script line: {}", line);
                // Script line added to panel, additional processing logic can be added here
            }
            CommandAction::SubmitScript { script, trace_id } => {
                info!("Submitting script id {}: {}", trace_id, script);

                // Parse the script to extract target and content
                let parsed_script = self.parse_trace_script(&script);
                if let Some((target, script_content)) = parsed_script {
                    // Copy the strings to avoid borrowing issues
                    let target_owned = target.to_string();
                    let script_content_owned = script_content.to_string();

                    // Add the script content to display with formatting
                    self.add_script_display(&target_owned, &script_content_owned);

                    // Send script to main program for processing (legacy mode without trace_id)
                    if let Err(e) =
                        self.event_registry
                            .command_sender
                            .send(RuntimeCommand::ExecuteScript {
                                command: script.clone(),
                                trace_id,
                            })
                    {
                        error!("Failed to send script: {}", e);
                        self.interactive_command_panel.add_response(
                            format!("✗ Failed to send script: {}", e),
                            ResponseType::Error,
                        );
                    } else {
                        self.interactive_command_panel.add_response(
                            "⏳ Compiling and loading script...".to_string(),
                            ResponseType::Progress,
                        );
                    }
                } else {
                    self.interactive_command_panel
                        .add_response("✗ Invalid script format".to_string(), ResponseType::Error);
                }
            }
            CommandAction::CancelScript => {
                info!("Script cancelled");
                self.interactive_command_panel.add_response(
                    "⚠ Script input cancelled".to_string(),
                    ResponseType::Warning,
                );
            }
            CommandAction::DisableTrace(trace_id) => {
                info!("Disabling trace {}", trace_id);
                if let Err(e) = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::DisableTrace(trace_id))
                {
                    error!("Failed to send disable trace command: {}", e);
                    self.interactive_command_panel.add_response(
                        format!("✗ Failed to disable trace {}: {}", trace_id, e),
                        ResponseType::Error,
                    );
                } else {
                    self.interactive_command_panel.add_response(
                        format!("⏳ Disabling trace {}...", trace_id),
                        ResponseType::Progress,
                    );
                }
            }
            CommandAction::EnableTrace(trace_id) => {
                info!("Enabling trace {}", trace_id);
                if let Err(e) = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::EnableTrace(trace_id))
                {
                    error!("Failed to send enable trace command: {}", e);
                    self.interactive_command_panel.add_response(
                        format!("✗ Failed to enable trace {}: {}", trace_id, e),
                        ResponseType::Error,
                    );
                } else {
                    self.interactive_command_panel.add_response(
                        format!("⏳ Enabling trace {}...", trace_id),
                        ResponseType::Progress,
                    );
                }
            }
            CommandAction::DisableAllTraces => {
                info!("Disabling all traces");
                if let Err(e) = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::DisableAllTraces)
                {
                    error!("Failed to send disable all traces command: {}", e);
                    self.interactive_command_panel.add_response(
                        format!("✗ Failed to disable all traces: {}", e),
                        ResponseType::Error,
                    );
                } else {
                    self.interactive_command_panel.add_response(
                        "⏳ Disabling all traces...".to_string(),
                        ResponseType::Progress,
                    );
                }
            }
            CommandAction::EnableAllTraces => {
                info!("Enabling all traces");
                if let Err(e) = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::EnableAllTraces)
                {
                    error!("Failed to send enable all traces command: {}", e);
                    self.interactive_command_panel.add_response(
                        format!("✗ Failed to enable all traces: {}", e),
                        ResponseType::Error,
                    );
                } else {
                    self.interactive_command_panel.add_response(
                        "⏳ Enabling all traces...".to_string(),
                        ResponseType::Progress,
                    );
                }
            }
            CommandAction::DeleteTrace(trace_id) => {
                info!("Deleting trace {}", trace_id);
                if let Err(e) = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::DeleteTrace(trace_id))
                {
                    error!("Failed to send delete trace command: {}", e);
                    self.interactive_command_panel.add_response(
                        format!("✗ Failed to delete trace {}: {}", trace_id, e),
                        ResponseType::Error,
                    );
                } else {
                    self.interactive_command_panel.add_response(
                        format!("⏳ Deleting trace {}...", trace_id),
                        ResponseType::Progress,
                    );
                }
            }
            CommandAction::DeleteAllTraces => {
                info!("Deleting all traces");
                if let Err(e) = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::DeleteAllTraces)
                {
                    error!("Failed to send delete all traces command: {}", e);
                    self.interactive_command_panel.add_response(
                        format!("✗ Failed to delete all traces: {}", e),
                        ResponseType::Error,
                    );
                } else {
                    self.interactive_command_panel.add_response(
                        "⏳ Deleting all traces...".to_string(),
                        ResponseType::Progress,
                    );
                }
            }
            CommandAction::InfoTarget { target } => {
                info!("Getting info for target: {}", target);
                if let Err(e) =
                    self.event_registry
                        .command_sender
                        .send(RuntimeCommand::InfoTarget {
                            target: target.clone(),
                        })
                {
                    error!("Failed to send info target command: {}", e);
                    self.interactive_command_panel.add_response(
                        format!("✗ Failed to get info for target '{}': {}", target, e),
                        ResponseType::Error,
                    );
                } else {
                    self.interactive_command_panel.add_response(
                        format!("⏳ Getting debug info for '{}'...", target),
                        ResponseType::Progress,
                    );
                }
            }
        }
        Ok(())
    }

    async fn execute_user_command(&mut self, command: String) -> Result<()> {
        let trimmed = command.trim();

        if trimmed == "help" {
            self.show_help();
        } else if trimmed.starts_with("attach ") {
            // Parse PID from "attach <pid>"
            if let Some(pid_str) = trimmed.strip_prefix("attach ") {
                if let Ok(pid) = pid_str.parse::<u32>() {
                    let _ = self
                        .event_registry
                        .command_sender
                        .send(RuntimeCommand::AttachToProcess(pid));
                    self.interactive_command_panel.add_response(
                        format!("⏳ Attaching to process {}", pid),
                        ResponseType::Progress,
                    );
                } else {
                    self.interactive_command_panel
                        .add_response(format!("✗ Invalid PID: {}", pid_str), ResponseType::Error);
                }
            }
        } else if trimmed == "detach" {
            let _ = self
                .event_registry
                .command_sender
                .send(RuntimeCommand::DetachFromProcess);
            self.interactive_command_panel
                .add_response("✓ Detached from process".to_string(), ResponseType::Success);
        } else if trimmed == "quit" || trimmed == "exit" {
            let _ = self
                .event_registry
                .command_sender
                .send(RuntimeCommand::Shutdown);
            self.should_quit = true;
        } else if trimmed.is_empty() {
            // Ignore empty commands
        } else {
            // Unknown command
            self.interactive_command_panel.add_response(
                format!("✗ Unknown command: {}", trimmed),
                ResponseType::Error,
            );
        }

        Ok(())
    }

    fn show_help(&mut self) {
        let help_text = r#"Available commands:
  help     - Show this help message
  trace    - Start tracing a function (enters script mode)
  attach   - Attach to a process by PID
  detach   - Detach from current process
  quit     - Exit ghostscope
  exit     - Exit ghostscope"#;

        self.interactive_command_panel
            .add_response(help_text.to_string(), ResponseType::Info);
    }

    fn cycle_focus(&mut self) {
        // Tab navigation follows same order as visual layout: Source -> Output -> Input
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Source => FocusedPanel::EbpfInfo,
            FocusedPanel::EbpfInfo => FocusedPanel::InteractiveCommand,
            FocusedPanel::InteractiveCommand => FocusedPanel::Source,
        };
    }

    fn cycle_focus_reverse(&mut self) {
        // Shift+Tab navigation follows reverse order: Input -> Output -> Source
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Source => FocusedPanel::InteractiveCommand,
            FocusedPanel::EbpfInfo => FocusedPanel::Source,
            FocusedPanel::InteractiveCommand => FocusedPanel::EbpfInfo,
        };
    }

    fn switch_layout(&mut self) {
        self.layout_mode = match self.layout_mode {
            LayoutMode::Horizontal => LayoutMode::Vertical,
            LayoutMode::Vertical => LayoutMode::Horizontal,
        };
    }

    fn move_focus(&mut self, direction: &str) {
        match self.layout_mode {
            LayoutMode::Horizontal => match direction {
                "left" => {
                    self.focused_panel = match self.focused_panel {
                        FocusedPanel::Source => FocusedPanel::InteractiveCommand,
                        FocusedPanel::EbpfInfo => FocusedPanel::Source,
                        FocusedPanel::InteractiveCommand => FocusedPanel::EbpfInfo,
                    };
                }
                "right" => {
                    self.focused_panel = match self.focused_panel {
                        FocusedPanel::Source => FocusedPanel::EbpfInfo,
                        FocusedPanel::EbpfInfo => FocusedPanel::InteractiveCommand,
                        FocusedPanel::InteractiveCommand => FocusedPanel::Source,
                    };
                }
                _ => {}
            },
            LayoutMode::Vertical => match direction {
                "up" => {
                    self.focused_panel = match self.focused_panel {
                        FocusedPanel::Source => FocusedPanel::InteractiveCommand,
                        FocusedPanel::EbpfInfo => FocusedPanel::Source,
                        FocusedPanel::InteractiveCommand => FocusedPanel::EbpfInfo,
                    };
                }
                "down" => {
                    self.focused_panel = match self.focused_panel {
                        FocusedPanel::Source => FocusedPanel::EbpfInfo,
                        FocusedPanel::EbpfInfo => FocusedPanel::InteractiveCommand,
                        FocusedPanel::InteractiveCommand => FocusedPanel::Source,
                    };
                }
                _ => {}
            },
        }
    }

    async fn handle_runtime_status(&mut self, status: RuntimeStatus) {
        debug!("Runtime status: {:?}", status);

        match &status {
            RuntimeStatus::SourceCodeLoaded(source_info) => {
                self.source_panel
                    .load_source(source_info.file_path.clone(), source_info.current_line);
            }
            RuntimeStatus::SourceCodeLoadFailed(error_message) => {
                self.source_panel.show_error(error_message.clone());
            }
            RuntimeStatus::DwarfLoadingCompleted { .. } => {
                // Auto-request source code when DWARF loading completes
                let _ = self
                    .event_registry
                    .command_sender
                    .send(RuntimeCommand::RequestSourceCode);
            }
            RuntimeStatus::ScriptCompilationCompleted { trace_id } => {
                // Update trace status in the interactive panel
                self.interactive_command_panel.update_trace_status(
                    crate::trace::TraceStatus::Active,
                    *trace_id,
                    None,
                );

                info!(
                    "✅ Script compilation completed successfully for trace_id: {:?}",
                    trace_id
                );
            }
            RuntimeStatus::ScriptCompilationFailed { error, trace_id } => {
                // Update trace status in the interactive panel
                self.interactive_command_panel.update_trace_status(
                    crate::trace::TraceStatus::Failed,
                    *trace_id,
                    Some(error.clone()),
                );

                error!(
                    "💔 Script compilation failed for trace_id: {:?}, error: {}",
                    trace_id, error
                );
            }
            RuntimeStatus::TraceEnabled { trace_id } => {
                // Handle sync response for enable command and update trace status
                self.interactive_command_panel.handle_command_completed();
                self.interactive_command_panel.update_trace_status(
                    crate::trace::TraceStatus::Active,
                    *trace_id,
                    None,
                );
                info!("Trace {} enabled successfully", trace_id);
            }
            RuntimeStatus::TraceDisabled { trace_id } => {
                // Handle sync response for disable command and update trace status
                self.interactive_command_panel.handle_command_completed();
                self.interactive_command_panel.update_trace_status(
                    crate::trace::TraceStatus::Disabled,
                    *trace_id,
                    None,
                );
                info!("Trace {} disabled successfully", trace_id);
            }
            RuntimeStatus::AllTracesEnabled { count } => {
                // Handle sync response for enable all command
                self.interactive_command_panel.handle_command_completed();
                info!("All traces enabled successfully ({} traces)", count);
            }
            RuntimeStatus::AllTracesDisabled { count } => {
                // Handle sync response for disable all command
                self.interactive_command_panel.handle_command_completed();
                info!("All traces disabled successfully ({} traces)", count);
            }
            RuntimeStatus::TraceEnableFailed { trace_id, error } => {
                // Handle sync failure for enable command
                self.interactive_command_panel.handle_command_failed(error);
                error!("Failed to enable trace {}: {}", trace_id, error);
            }
            RuntimeStatus::TraceDisableFailed { trace_id, error } => {
                // Handle sync failure for disable command
                self.interactive_command_panel.handle_command_failed(error);
                error!("Failed to disable trace {}: {}", trace_id, error);
            }
            RuntimeStatus::TraceDeleted { trace_id } => {
                // Handle sync response for delete command and remove trace from UI
                self.interactive_command_panel.handle_command_completed();
                self.interactive_command_panel
                    .trace_manager
                    .remove_trace(*trace_id);
                info!(
                    "Trace {} deleted successfully and removed from UI",
                    trace_id
                );
            }
            RuntimeStatus::AllTracesDeleted { count } => {
                // Handle sync response for delete all command and clear all traces
                self.interactive_command_panel.handle_command_completed();
                self.interactive_command_panel
                    .trace_manager
                    .clear_all_traces();
                info!("All traces deleted successfully ({} traces)", count);
            }
            RuntimeStatus::TraceDeleteFailed { trace_id, error } => {
                // Handle sync failure for delete command
                self.interactive_command_panel.handle_command_failed(error);
                error!("Failed to delete trace {}: {}", trace_id, error);
            }

            RuntimeStatus::InfoTargetResult { target, info } => {
                self.interactive_command_panel.handle_command_completed();
                let formatted_info = format_target_debug_info(target, info);
                self.interactive_command_panel
                    .add_response(formatted_info, ResponseType::Success);
            }
            RuntimeStatus::InfoTargetFailed { target, error } => {
                self.interactive_command_panel.handle_command_completed();
                self.interactive_command_panel.add_response(
                    format!("✗ Failed to get debug info for '{}': {}", target, error),
                    ResponseType::Error,
                );
            }
            RuntimeStatus::Error(error) => {
                // Handle sync failure for batch operations that send generic errors
                self.interactive_command_panel.handle_command_failed(error);
                error!("Runtime error: {}", error);
            }
            _ => {}
        }

        // TODO: Consider moving status messages to the interactive command panel
        // for better separation of concerns. The eBPF output panel should focus
        // on actual eBPF events, while status messages could be shown in a
        // dedicated status/log area in the command panel.

        // Currently not showing any status messages in eBPF output panel
        // to keep it focused on actual eBPF trace events only
    }

    async fn handle_trace_event(&mut self, trace_event: ghostscope_protocol::EventData) {
        debug!("Trace event: {:?}", trace_event);
        self.ebpf_info_panel.add_trace_event(trace_event);
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let size = frame.area();

        let chunks = match self.layout_mode {
            LayoutMode::Horizontal => {
                Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Ratio(4, 10), // Source code panel - 40%
                        Constraint::Ratio(3, 10), // eBPF output panel - 30%
                        Constraint::Ratio(3, 10), // Interactive command panel - 30%
                    ])
                    .split(size)
            }
            LayoutMode::Vertical => {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Ratio(4, 10), // Source code panel - 40%
                        Constraint::Ratio(3, 10), // eBPF output panel - 30%
                        Constraint::Ratio(3, 10), // Interactive command panel - 30%
                    ])
                    .split(size)
            }
        };

        // Render panels with focus indication
        self.source_panel
            .render(frame, chunks[0], self.focused_panel == FocusedPanel::Source);
        self.ebpf_info_panel.render(
            frame,
            chunks[1],
            self.focused_panel == FocusedPanel::EbpfInfo,
        );
        self.interactive_command_panel.render(
            frame,
            chunks[2],
            self.focused_panel == FocusedPanel::InteractiveCommand,
        );
    }
}

/// Format target debug info for display
fn format_target_debug_info(target: &str, info: &crate::events::TargetDebugInfo) -> String {
    use crate::events::TargetType;

    let mut result = String::new();

    // Header with icon
    match info.target_type {
        TargetType::Function => {
            result.push_str(&format!("🔧 Function Debug Info: '{}'\n", target));
        }
        TargetType::SourceLocation => {
            result.push_str(&format!("📍 Source Location: '{}'\n", target));
        }
        TargetType::Address => {
            result.push_str(&format!("🎯 Address Debug Info: '{}'\n", target));
        }
    }

    // Location info section
    result.push_str("\n📂 Location:\n");
    if let Some(ref file_path) = info.file_path {
        result.push_str(&format!("   File: {}\n", file_path));
    }
    if let Some(line_number) = info.line_number {
        result.push_str(&format!("   Line: {}\n", line_number));
    }
    if let Some(ref func_name) = info.function_name {
        result.push_str(&format!("   Function: {}\n", func_name));
    }
    if let Some(address) = info.address {
        result.push_str(&format!("   Address: 0x{:x}\n", address));
    }

    // Parameters section
    if !info.parameters.is_empty() {
        result.push_str("\n📥 Parameters:\n");
        for (i, param) in info.parameters.iter().enumerate() {
            let prefix = if i == info.parameters.len() - 1 {
                "   └─"
            } else {
                "   ├─"
            };
            result.push_str(&format!("{} {} {}", prefix, param.name, param.type_name));

            // Add location and size info in a compact format
            let mut details = Vec::new();
            if !param.location_description.is_empty() && param.location_description != "None" {
                details.push(format!("loc: {}", param.location_description));
            }
            if let Some(size) = param.size {
                details.push(format!("{} bytes", size));
            }

            if !details.is_empty() {
                result.push_str(&format!(" ({})", details.join(", ")));
            }
            result.push('\n');
        }
    }

    // Variables section
    if !info.variables.is_empty() {
        result.push_str("\n📦 Local Variables:\n");
        for (i, var) in info.variables.iter().enumerate() {
            let prefix = if i == info.variables.len() - 1 {
                "   └─"
            } else {
                "   ├─"
            };
            result.push_str(&format!("{} {} {}", prefix, var.name, var.type_name));

            // Add location and size info in a compact format
            let mut details = Vec::new();
            if !var.location_description.is_empty() && var.location_description != "None" {
                details.push(format!("loc: {}", var.location_description));
            }
            if let Some(size) = var.size {
                details.push(format!("{} bytes", size));
            }

            if !details.is_empty() {
                result.push_str(&format!(" ({})", details.join(", ")));
            }
            result.push('\n');
        }
    }

    if info.parameters.is_empty() && info.variables.is_empty() {
        result.push_str("\n❌ No variables or parameters found in scope\n");
    }

    result
}
