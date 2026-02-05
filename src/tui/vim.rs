//! Vim mode emulation for tui-textarea
//!
//! Based on tui-textarea's vim.rs example:
//! https://github.com/rhysd/tui-textarea/blob/main/examples/vim.rs

use std::fmt;
use tui_textarea::{CursorMove, Input, Key, Scrolling, TextArea};

/// Vim editing mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
    Operator(char),
}

impl VimMode {
    /// Get a short string representation for the mode indicator
    pub fn indicator(&self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual => "VISUAL",
            Self::Operator(_) => "OPERATOR",
        }
    }
}

impl fmt::Display for VimMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            Self::Normal => write!(f, "NORMAL"),
            Self::Insert => write!(f, "INSERT"),
            Self::Visual => write!(f, "VISUAL"),
            Self::Operator(c) => write!(f, "OPERATOR({})", c),
        }
    }
}

/// Result of processing an input in vim mode
pub enum VimTransition {
    /// No state change
    Nop,
    /// Mode changed
    Mode(VimMode),
    /// Input is pending (part of a multi-key sequence like "gg")
    Pending(Input),
}

/// Vim emulation state machine
pub struct Vim {
    pub mode: VimMode,
    pending: Input,
}

impl Default for Vim {
    fn default() -> Self {
        Self::new(VimMode::Normal)
    }
}

impl Vim {
    pub fn new(mode: VimMode) -> Self {
        Self {
            mode,
            pending: Input::default(),
        }
    }

    /// Process an input event and potentially modify the textarea
    ///
    /// Returns a VimTransition indicating what happened
    pub fn transition(&mut self, input: Input, textarea: &mut TextArea<'_>) -> VimTransition {
        if input.key == Key::Null {
            return VimTransition::Nop;
        }

        let transition = self.transition_inner(input.clone(), textarea);

        // Update state based on transition
        match &transition {
            VimTransition::Mode(mode) => {
                self.mode = *mode;
                self.pending = Input::default();
            }
            VimTransition::Pending(input) => {
                self.pending = input.clone();
            }
            VimTransition::Nop => {
                self.pending = Input::default();
            }
        }

        transition
    }

    fn transition_inner(&self, input: Input, textarea: &mut TextArea<'_>) -> VimTransition {
        match self.mode {
            VimMode::Normal | VimMode::Visual | VimMode::Operator(_) => {
                match input {
                    // Basic movement
                    Input {
                        key: Key::Char('h'),
                        ..
                    } => textarea.move_cursor(CursorMove::Back),
                    Input {
                        key: Key::Char('j'),
                        ..
                    } => textarea.move_cursor(CursorMove::Down),
                    Input {
                        key: Key::Char('k'),
                        ..
                    } => textarea.move_cursor(CursorMove::Up),
                    Input {
                        key: Key::Char('l'),
                        ..
                    } => textarea.move_cursor(CursorMove::Forward),

                    // Word movement
                    Input {
                        key: Key::Char('w'),
                        ..
                    } => textarea.move_cursor(CursorMove::WordForward),
                    Input {
                        key: Key::Char('e'),
                        ctrl: false,
                        ..
                    } => {
                        textarea.move_cursor(CursorMove::WordEnd);
                        if matches!(self.mode, VimMode::Operator(_)) {
                            textarea.move_cursor(CursorMove::Forward);
                        }
                    }
                    Input {
                        key: Key::Char('b'),
                        ctrl: false,
                        ..
                    } => textarea.move_cursor(CursorMove::WordBack),

                    // Line movement
                    Input {
                        key: Key::Char('^'),
                        ..
                    }
                    | Input {
                        key: Key::Char('0'),
                        ..
                    } => textarea.move_cursor(CursorMove::Head),
                    Input {
                        key: Key::Char('$'),
                        ..
                    } => textarea.move_cursor(CursorMove::End),

                    // Delete to end of line
                    Input {
                        key: Key::Char('D'),
                        ..
                    } => {
                        textarea.delete_line_by_end();
                        return VimTransition::Mode(VimMode::Normal);
                    }

                    // Change to end of line
                    Input {
                        key: Key::Char('C'),
                        ..
                    } => {
                        textarea.delete_line_by_end();
                        textarea.cancel_selection();
                        return VimTransition::Mode(VimMode::Insert);
                    }

                    // Paste
                    Input {
                        key: Key::Char('p'),
                        ..
                    } => {
                        textarea.paste();
                        return VimTransition::Mode(VimMode::Normal);
                    }

                    // Undo
                    Input {
                        key: Key::Char('u'),
                        ctrl: false,
                        ..
                    } => {
                        textarea.undo();
                        return VimTransition::Mode(VimMode::Normal);
                    }

                    // Redo
                    Input {
                        key: Key::Char('r'),
                        ctrl: true,
                        ..
                    } => {
                        textarea.redo();
                        return VimTransition::Mode(VimMode::Normal);
                    }

                    // Delete char under cursor
                    Input {
                        key: Key::Char('x'),
                        ..
                    } => {
                        textarea.delete_next_char();
                        return VimTransition::Mode(VimMode::Normal);
                    }

                    // Insert mode entries
                    Input {
                        key: Key::Char('i'),
                        ..
                    } => {
                        textarea.cancel_selection();
                        return VimTransition::Mode(VimMode::Insert);
                    }
                    Input {
                        key: Key::Char('a'),
                        ctrl: false,
                        ..
                    } => {
                        textarea.cancel_selection();
                        textarea.move_cursor(CursorMove::Forward);
                        return VimTransition::Mode(VimMode::Insert);
                    }
                    Input {
                        key: Key::Char('A'),
                        ..
                    } => {
                        textarea.cancel_selection();
                        textarea.move_cursor(CursorMove::End);
                        return VimTransition::Mode(VimMode::Insert);
                    }
                    Input {
                        key: Key::Char('o'),
                        ..
                    } => {
                        textarea.move_cursor(CursorMove::End);
                        textarea.insert_newline();
                        return VimTransition::Mode(VimMode::Insert);
                    }
                    Input {
                        key: Key::Char('O'),
                        ..
                    } => {
                        textarea.move_cursor(CursorMove::Head);
                        textarea.insert_newline();
                        textarea.move_cursor(CursorMove::Up);
                        return VimTransition::Mode(VimMode::Insert);
                    }
                    Input {
                        key: Key::Char('I'),
                        ..
                    } => {
                        textarea.cancel_selection();
                        textarea.move_cursor(CursorMove::Head);
                        return VimTransition::Mode(VimMode::Insert);
                    }

                    // Scrolling
                    Input {
                        key: Key::Char('e'),
                        ctrl: true,
                        ..
                    } => textarea.scroll((1, 0)),
                    Input {
                        key: Key::Char('y'),
                        ctrl: true,
                        ..
                    } => textarea.scroll((-1, 0)),
                    Input {
                        key: Key::Char('d'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::HalfPageDown),
                    Input {
                        key: Key::Char('u'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::HalfPageUp),
                    Input {
                        key: Key::Char('f'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::PageDown),
                    Input {
                        key: Key::Char('b'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::PageUp),

                    // Visual mode
                    Input {
                        key: Key::Char('v'),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Normal => {
                        textarea.start_selection();
                        return VimTransition::Mode(VimMode::Visual);
                    }
                    Input {
                        key: Key::Char('V'),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Normal => {
                        textarea.move_cursor(CursorMove::Head);
                        textarea.start_selection();
                        textarea.move_cursor(CursorMove::End);
                        return VimTransition::Mode(VimMode::Visual);
                    }

                    // Exit visual mode
                    Input { key: Key::Esc, .. }
                    | Input {
                        key: Key::Char('v'),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Visual => {
                        textarea.cancel_selection();
                        return VimTransition::Mode(VimMode::Normal);
                    }

                    // Exit operator mode
                    Input { key: Key::Esc, .. } if matches!(self.mode, VimMode::Operator(_)) => {
                        textarea.cancel_selection();
                        return VimTransition::Mode(VimMode::Normal);
                    }

                    // gg - go to top
                    Input {
                        key: Key::Char('g'),
                        ctrl: false,
                        ..
                    } if matches!(
                        self.pending,
                        Input {
                            key: Key::Char('g'),
                            ctrl: false,
                            ..
                        }
                    ) =>
                    {
                        textarea.move_cursor(CursorMove::Top);
                    }

                    // G - go to bottom
                    Input {
                        key: Key::Char('G'),
                        ctrl: false,
                        ..
                    } => textarea.move_cursor(CursorMove::Bottom),

                    // Handle yy, dd, cc (line operations)
                    Input {
                        key: Key::Char(c),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Operator(c) => {
                        textarea.move_cursor(CursorMove::Head);
                        textarea.start_selection();
                        let cursor = textarea.cursor();
                        textarea.move_cursor(CursorMove::Down);
                        if cursor == textarea.cursor() {
                            textarea.move_cursor(CursorMove::End);
                        }
                    }

                    // Start operator mode (y, d, c)
                    Input {
                        key: Key::Char(op @ ('y' | 'd' | 'c')),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Normal => {
                        textarea.start_selection();
                        return VimTransition::Mode(VimMode::Operator(op));
                    }

                    // Visual mode operations
                    Input {
                        key: Key::Char('y'),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Visual => {
                        textarea.move_cursor(CursorMove::Forward);
                        textarea.copy();
                        return VimTransition::Mode(VimMode::Normal);
                    }
                    Input {
                        key: Key::Char('d'),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Visual => {
                        textarea.move_cursor(CursorMove::Forward);
                        textarea.cut();
                        return VimTransition::Mode(VimMode::Normal);
                    }
                    Input {
                        key: Key::Char('c'),
                        ctrl: false,
                        ..
                    } if self.mode == VimMode::Visual => {
                        textarea.move_cursor(CursorMove::Forward);
                        textarea.cut();
                        return VimTransition::Mode(VimMode::Insert);
                    }

                    // Pending input (for multi-key sequences)
                    input => return VimTransition::Pending(input),
                }

                // Handle the pending operator after movement
                match self.mode {
                    VimMode::Operator('y') => {
                        textarea.copy();
                        VimTransition::Mode(VimMode::Normal)
                    }
                    VimMode::Operator('d') => {
                        textarea.cut();
                        VimTransition::Mode(VimMode::Normal)
                    }
                    VimMode::Operator('c') => {
                        textarea.cut();
                        VimTransition::Mode(VimMode::Insert)
                    }
                    _ => VimTransition::Nop,
                }
            }
            VimMode::Insert => match input {
                Input { key: Key::Esc, .. }
                | Input {
                    key: Key::Char('c'),
                    ctrl: true,
                    ..
                } => VimTransition::Mode(VimMode::Normal),
                input => {
                    textarea.input(input);
                    VimTransition::Mode(VimMode::Insert)
                }
            },
        }
    }
}

/// A TextArea with vim mode support
pub struct VimTextArea<'a> {
    pub textarea: TextArea<'a>,
    pub vim: Vim,
}

impl<'a> Default for VimTextArea<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> VimTextArea<'a> {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            textarea,
            vim: Vim::default(),
        }
    }

    pub fn from_lines<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let lines: Vec<String> = lines.into_iter().map(Into::into).collect();
        let mut textarea = TextArea::from(lines.iter().map(|s| s.as_str()));
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            textarea,
            vim: Vim::default(),
        }
    }

    /// Get the current vim mode
    pub fn mode(&self) -> VimMode {
        self.vim.mode
    }

    /// Process an input event
    pub fn input(&mut self, input: Input) {
        self.vim.transition(input, &mut self.textarea);
    }

    /// Get all lines as a joined string
    pub fn lines_joined(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Get lines
    pub fn lines(&self) -> &[String] {
        self.textarea.lines()
    }

    /// Move cursor
    pub fn move_cursor(&mut self, cursor_move: CursorMove) {
        self.textarea.move_cursor(cursor_move);
    }

    /// Get cursor position
    pub fn cursor(&self) -> (usize, usize) {
        self.textarea.cursor()
    }

    /// Reset to insert mode (useful for starting fresh in insert mode)
    pub fn set_insert_mode(&mut self) {
        self.vim.mode = VimMode::Insert;
    }

    /// Reset to normal mode
    pub fn set_normal_mode(&mut self) {
        self.vim.mode = VimMode::Normal;
        self.textarea.cancel_selection();
    }

    /// Set content from string
    pub fn set_content(&mut self, content: &str) {
        self.textarea = TextArea::from(content.lines());
        self.textarea
            .set_cursor_line_style(ratatui::style::Style::default());
    }
}
