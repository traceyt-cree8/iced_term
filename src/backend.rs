use crate::actions::Action;
use crate::settings::BackendSettings;
use alacritty_terminal::event::{
    Event, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{
    self, cell::Cell, test::TermSize, viewport_to_point, Term, TermMode,
};
use alacritty_terminal::tty;
use iced::keyboard::Modifiers;
use iced_core::Size;
use std::borrow::Cow;
use std::cmp::min;
use std::io::Result;
use std::ops::RangeInclusive;
use std::sync::Arc;
use tokio::sync::mpsc;

const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#;

/// Escape special regex characters for literal string matching
fn escape_regex(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len() * 2);
    for c in pattern.chars() {
        match c {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']'
            | '{' | '}' | '^' | '$' => {
                escaped.push('\\');
                escaped.push(c);
            },
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Represents a search match in the terminal scrollback
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub start: Point,
    pub end: Point,
}

#[derive(Debug, Clone)]
pub enum Command {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Option<Size<f32>>, Option<Size<f32>>),
    SelectStart(SelectionType, (f32, f32)),
    SelectUpdate((f32, f32)),
    ClearSelection,
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
    ProcessAlacrittyEvent(Event),
}

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalSize {
    pub cell_width: u16,
    pub cell_height: u16,
    num_cols: u16,
    num_lines: u16,
    layout_width: f32,
    layout_height: f32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: 1,
            cell_height: 1,
            num_cols: 80,
            num_lines: 50,
            layout_width: 80.0,
            layout_height: 50.0,
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn columns(&self) -> usize {
        self.num_cols as usize
    }

    fn last_column(&self) -> Column {
        Column(self.num_cols as usize - 1)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.num_lines as i32 - 1)
    }

    fn screen_lines(&self) -> usize {
        self.num_lines as usize
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_lines: size.num_lines,
            num_cols: size.num_cols,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

pub struct Backend {
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: TerminalSize,
    applied_size: TerminalSize,
    notifier: Notifier,
    pub(crate) last_content: RenderableContent,
    pub(crate) url_regex: RegexSearch,
}

impl Backend {
    pub fn new(
        id: u64,
        pty_event_proxy_sender: mpsc::Sender<Event>,
        settings: BackendSettings,
    ) -> Result<Self> {
        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(settings.program, settings.args)),
            working_directory: settings.working_directory,
            env: settings.env,
            ..tty::Options::default()
        };

        let config = term::Config {
            scrolling_history: settings.scrollback_lines,
            ..term::Config::default()
        };
        let terminal_size = TerminalSize::default();
        let pty = tty::new(&pty_config, terminal_size.into(), id)?;

        let event_proxy = EventProxy(pty_event_proxy_sender);

        let term = Term::new(config, &terminal_size, event_proxy.clone());
        let mut initial_content = RenderableContent::default();
        initial_content.sync_from(&term, terminal_size);

        let term = Arc::new(FairMutex::new(term));

        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, pty, false, false)?;

        let notifier = Notifier(pty_event_loop.channel());

        let _ = pty_event_loop.spawn();

        Ok(Self {
            term: term.clone(),
            size: terminal_size,
            applied_size: terminal_size,
            notifier,
            last_content: initial_content,
            url_regex: RegexSearch::new(URL_REGEX).expect("invalid url regexp"),
        })
    }

    pub fn handle(&mut self, cmd: Command) -> Action {
        // Handle commands that don't need the terminal lock first.
        // This avoids blocking the main thread when the PTY event loop
        // holds the FairMutex lease during bursts of terminal output.
        match cmd {
            Command::ProcessAlacrittyEvent(event) => {
                return match event {
                    Event::Exit => Action::Shutdown,
                    Event::Title(title) => Action::ChangeTitle(title),
                    Event::PtyWrite(pty) => {
                        self.notifier.notify(pty.into_bytes());
                        Action::default()
                    },
                    Event::ClipboardStore(_clipboard_type, text) => {
                        Action::ClipboardStore(text)
                    },
                    Event::ClipboardLoad(_clipboard_type, formatter) => {
                        Action::ClipboardLoad(formatter)
                    },
                    _ => Action::default(),
                };
            },
            Command::MouseReport(button, modifiers, point, pressed) => {
                self.process_mouse_report(button, modifiers, point, pressed);
                return Action::default();
            },
            Command::Write(input) => {
                // Write to PTY immediately (no lock needed).
                self.write(input);
                // Try to scroll to bottom without blocking. The caller owns
                // snapshot publication, so this command only mutates the live
                // terminal.
                let term = self.term.clone();
                if let Some(mut term) = term.try_lock_unfair() {
                    term.scroll_display(Scroll::Bottom);
                }
                return Action::default();
            },
            Command::Resize(layout_size, font_measure) => {
                self.update_size(layout_size, font_measure);
                let term = self.term.clone();
                if let Some(mut term) = term.try_lock_unfair() {
                    self.apply_resize(&mut term);
                }
                return Action::default();
            },
            // Commands that need the terminal lock — fall through below.
            _ => {},
        }

        // Scroll, Select, Link — need the terminal lock.
        // Use try_lock_unfair to avoid blocking the main thread if the PTY
        // event loop is holding the lease during a burst of output.
        let term_arc = self.term.clone();
        if let Some(mut term) = term_arc.try_lock_unfair() {
            match cmd {
                Command::Scroll(delta) => {
                    self.scroll(&mut term, delta);
                },
                Command::SelectStart(selection_type, (x, y)) => {
                    self.start_selection(&mut term, selection_type, x, y);
                },
                Command::SelectUpdate((x, y)) => {
                    self.update_selection(&mut term, x, y);
                },
                Command::ClearSelection => {
                    term.selection = None;
                },
                Command::ProcessLink(link_action, point) => {
                    self.process_link_action(&term, link_action, point);
                },
                // Already handled above — can't reach here.
                Command::ProcessAlacrittyEvent(_)
                | Command::MouseReport(..)
                | Command::Write(_)
                | Command::Resize(..) => {},
            };
        }
        // If try_lock_unfair() failed, the command is silently dropped.
        // This is acceptable: scroll/select/link events are continuous and
        // will be retried on the next mouse/keyboard event. Resize state is
        // retained and applied by the next successful sync.

        Action::default()
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                let hovered_hyperlink = self.regex_match_at(
                    terminal,
                    point,
                    &mut self.url_regex.clone(),
                );
                let hovered_url = hovered_hyperlink
                    .as_ref()
                    .map(|range| Self::text_for_range(terminal, range));
                self.last_content.hovered_hyperlink = hovered_hyperlink;
                self.last_content.hovered_url = hovered_url;
            },
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
                self.last_content.hovered_url = None;
            },
            LinkAction::Open => {
                self.open_link();
            },
        };
    }

    fn open_link(&self) {
        if let Some(url) = &self.last_content.hovered_url {
            open::that(url).unwrap_or_else(|_| {
                panic!("link opening is failed");
            })
        }
    }

    fn text_for_range<T: EventListener>(
        terminal: &Term<T>,
        range: &RangeInclusive<Point>,
    ) -> String {
        let start = *range.start();
        let end = *range.end();
        let mut text = String::from(terminal.grid()[start].c);
        if start == end {
            return text;
        }

        for indexed in terminal.grid().iter_from(*range.start()) {
            text.push(indexed.c);
            if indexed.point == end {
                break;
            }
        }
        text
    }

    fn process_mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
    ) {
        let mut mods = 0;
        if modifiers.contains(Modifiers::SHIFT) {
            mods += 4;
        }
        if modifiers.contains(Modifiers::ALT) {
            mods += 8;
        }
        if modifiers.contains(Modifiers::COMMAND) {
            mods += 16;
        }

        match MouseMode::from(self.last_content.terminal_mode) {
            MouseMode::Sgr => {
                self.sgr_mouse_report(point, button as u8 + mods, pressed)
            },
            MouseMode::Normal(is_utf8) => {
                if pressed {
                    self.normal_mouse_report(
                        point,
                        button as u8 + mods,
                        is_utf8,
                    )
                } else {
                    self.normal_mouse_report(point, 3 + mods, is_utf8)
                }
            },
        }
    }

    fn sgr_mouse_report(&self, point: Point, button: u8, pressed: bool) {
        let c = if pressed { 'M' } else { 'm' };

        let msg = format!(
            "\x1b[<{};{};{}{}",
            button,
            point.column + 1,
            point.line + 1,
            c
        );

        self.notifier.notify(msg.as_bytes().to_vec());
    }

    fn normal_mouse_report(&self, point: Point, button: u8, is_utf8: bool) {
        let Point { line, column } = point;
        let max_point = if is_utf8 { 2015 } else { 223 };

        if line >= max_point || column >= max_point {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if is_utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if is_utf8 && line >= 95 {
            msg.append(&mut mouse_pos_encode(line.0 as usize));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }

        self.notifier.notify(msg);
    }

    fn start_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        selection_type: SelectionType,
        x: f32,
        y: f32,
    ) {
        let location = Self::selection_point(
            x,
            y,
            &self.size,
            terminal.grid().display_offset(),
        );
        terminal.selection = Some(Selection::new(
            selection_type,
            location,
            self.selection_side(x),
        ));
    }

    fn update_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        x: f32,
        y: f32,
    ) {
        let display_offset = terminal.grid().display_offset();
        if let Some(ref mut selection) = terminal.selection {
            let location =
                Self::selection_point(x, y, &self.size, display_offset);
            selection.update(location, self.selection_side(x));
        }
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        let col = (x as usize) / (terminal_size.cell_width as usize);
        let col = min(Column(col), Column(terminal_size.num_cols as usize - 1));

        let line = (y as usize) / (terminal_size.cell_height as usize);
        let line = min(line, terminal_size.num_lines as usize - 1);

        viewport_to_point(display_offset, Point::new(line, col))
    }

    fn selection_side(&self, x: f32) -> Side {
        let cell_x = x as usize % self.size.cell_width as usize;
        let half_cell_width = (self.size.cell_width as f32 / 2.0) as usize;

        if cell_x > half_cell_width {
            Side::Right
        } else {
            Side::Left
        }
    }

    fn update_size(
        &mut self,
        layout_size: Option<Size<f32>>,
        font_measure: Option<Size<f32>>,
    ) {
        if let Some(size) = layout_size {
            self.size.layout_height = size.height;
            self.size.layout_width = size.width;
        };

        if let Some(size) = font_measure {
            self.size.cell_height = size.height as u16;
            self.size.cell_width = size.width as u16;
        }

        let lines = (self.size.layout_height / self.size.cell_height as f32)
            .floor() as u16;
        let cols = (self.size.layout_width / self.size.cell_width as f32)
            .floor() as u16;
        if lines > 0 && cols > 0 {
            self.size.num_lines = lines;
            self.size.num_cols = cols;
        }
    }

    fn apply_resize(&mut self, terminal: &mut Term<EventProxy>) {
        if self.size == self.applied_size {
            return;
        }

        self.notifier.on_resize(self.size.into());
        if self.size.num_cols != self.applied_size.num_cols
            || self.size.num_lines != self.applied_size.num_lines
        {
            terminal.resize(TermSize::new(
                self.size.num_cols as usize,
                self.size.num_lines as usize,
            ));
        }
        self.applied_size = self.size;
    }

    fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.notifier.notify(input);
    }

    fn scroll(&mut self, terminal: &mut Term<EventProxy>, delta_value: i32) {
        if delta_value != 0 {
            let scroll = Scroll::Delta(delta_value);
            if terminal
                .mode()
                .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN)
            {
                let line_cmd = if delta_value > 0 { b'A' } else { b'B' };
                let mut content = vec![];

                for _ in 0..delta_value.abs() {
                    content.push(0x1b);
                    content.push(b'O');
                    content.push(line_cmd);
                }

                self.notifier.notify(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }

    pub fn selectable_content(&self) -> String {
        // Use alacritty's selection_to_string() which properly handles
        // newlines, trailing whitespace, wrapped lines, wide chars, and tabs.
        let term_arc = self.term.clone();
        if let Some(term) = term_arc.try_lock_unfair() {
            if let Some(text) = term.selection_to_string() {
                return text;
            }
        }
        String::new()
    }

    pub fn sync(&mut self) -> bool {
        let term_arc = self.term.clone();
        // Use try_lock_unfair to avoid blocking the main thread.
        // If the PTY event loop holds the lock, we skip this sync —
        // the content will be slightly stale until the next frame.
        let Some(mut term) = term_arc.try_lock_unfair() else {
            return false;
        };
        self.apply_resize(&mut term);
        self.internal_sync(&term);
        true
    }

    fn internal_sync(&mut self, terminal: &Term<EventProxy>) {
        self.last_content.sync_from(terminal, self.size);
    }

    pub fn renderable_content(&self) -> &RenderableContent {
        &self.last_content
    }

    /// Search for all occurrences of a pattern in the terminal scrollback
    pub fn search_all(&mut self, pattern: &str) -> Vec<SearchMatch> {
        if pattern.is_empty() {
            return Vec::new();
        }

        // Escape special regex characters for literal search
        let escaped = escape_regex(pattern);
        let Ok(mut regex) = RegexSearch::new(&escaped) else {
            return Vec::new();
        };

        let term = self.term.clone();
        let Some(term) = term.try_lock_unfair() else {
            return Vec::new();
        };

        let mut matches = Vec::new();

        // Search from the beginning of history to the end of the viewport
        let history_start = Line(-(term.grid().history_size() as i32));
        let viewport_end = term.bottommost_line();

        let start = Point::new(history_start, Column(0));
        let end = Point::new(viewport_end, term.last_column());

        for rm in
            RegexIter::new(start, end, Direction::Right, &term, &mut regex)
        {
            matches.push(SearchMatch {
                start: *rm.start(),
                end: *rm.end(),
            });
        }

        matches
    }

    /// Get all terminal text content (including scrollback history)
    pub fn get_all_text(&self) -> String {
        let term = self.term.clone();
        let Some(term) = term.try_lock_unfair() else {
            return String::new();
        };

        let mut result = String::new();
        let mut current_line = None;
        let mut line_text = String::new();

        // Get the full range from history to viewport
        let history_start = Line(-(term.grid().history_size() as i32));
        let viewport_end = term.bottommost_line();

        let start = Point::new(history_start, Column(0));
        let end = Point::new(viewport_end, term.last_column());

        // Iterate through all cells using display_iter
        for indexed in term.grid().iter_from(start) {
            // Check if we've moved to a new line
            if Some(indexed.point.line) != current_line {
                if current_line.is_some() {
                    // Finish the previous line
                    result.push_str(line_text.trim_end());
                    result.push('\n');
                }
                current_line = Some(indexed.point.line);
                line_text.clear();
            }

            // Add character to current line
            line_text.push(indexed.c);

            // Stop if we've reached the end
            if indexed.point == end {
                break;
            }
        }

        // Add the final line
        if !line_text.is_empty() {
            result.push_str(line_text.trim_end());
            result.push('\n');
        }

        result
    }

    /// Scroll the terminal to show a specific line
    pub fn scroll_to_line(&mut self, line: i32) {
        let term = self.term.clone();
        let Some(mut term) = term.try_lock_unfair() else {
            return;
        };

        // Line is in history (negative) or viewport (positive)
        // We want to scroll so the line is near the top of the viewport
        let target_offset = if line < 0 {
            // Line is in history - convert to display offset
            (-line) as usize
        } else {
            0
        };

        // Calculate delta from current position
        let current_offset = term.grid().display_offset();
        if target_offset != current_offset {
            let delta = target_offset as i32 - current_offset as i32;
            term.grid_mut().scroll_display(Scroll::Delta(delta));
        }
    }

    /// Based on alacritty/src/display/hint.rs > regex_match_at
    /// Retrieve the match, if the specified point is inside the content matching the regex.
    fn regex_match_at(
        &self,
        terminal: &Term<EventProxy>,
        point: Point,
        regex: &mut RegexSearch,
    ) -> Option<Match> {
        let x = visible_regex_match_iter(terminal, regex)
            .find(|rm| rm.contains(&point));
        x
    }
}

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
fn visible_regex_match_iter<'a>(
    term: &'a Term<EventProxy>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start =
        term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

pub struct RenderableCell {
    pub point: Point,
    pub cell: Cell,
}

pub struct RenderableContent {
    pub cells: Vec<RenderableCell>,
    pub display_offset: usize,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor_point: Point,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
    hovered_url: Option<String>,
}

impl RenderableContent {
    fn sync_from<T: EventListener>(
        &mut self,
        terminal: &Term<T>,
        terminal_size: TerminalSize,
    ) {
        let renderable = terminal.renderable_content();
        let cursor_point = renderable.cursor.point;
        let cursor = terminal.grid()[cursor_point].clone();
        let selectable_range = renderable.selection;
        let display_offset = renderable.display_offset;
        let terminal_mode = renderable.mode;

        self.cells.clear();
        self.cells.extend(renderable.display_iter.map(|indexed| {
            RenderableCell {
                point: indexed.point,
                cell: indexed.cell.clone(),
            }
        }));
        self.display_offset = display_offset;
        self.selectable_range = selectable_range;
        self.cursor_point = cursor_point;
        self.cursor = cursor;
        self.terminal_mode = terminal_mode;
        self.terminal_size = terminal_size;
    }
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            cells: Vec::new(),
            display_offset: 0,
            hovered_hyperlink: None,
            selectable_range: None,
            cursor_point: Point::default(),
            cursor: Cell::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
            hovered_url: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::Color;

    fn test_term(
        columns: usize,
        lines: usize,
        history_limit: usize,
    ) -> Term<VoidListener> {
        let config = term::Config {
            scrolling_history: history_limit,
            ..term::Config::default()
        };
        let size = TermSize::new(columns, lines);
        Term::new(config, &size, VoidListener)
    }

    fn add_history(term: &mut Term<VoidListener>, lines: usize) {
        let screen_lines = term.screen_lines() as i32;
        let region = Line(0)..Line(screen_lines);
        for _ in 0..lines {
            term.grid_mut().scroll_up::<Color>(&region, 1);
        }
    }

    fn snapshot(term: &Term<VoidListener>) -> RenderableContent {
        let mut snapshot = RenderableContent::default();
        let size = TerminalSize {
            num_cols: term.columns() as u16,
            num_lines: term.screen_lines() as u16,
            ..TerminalSize::default()
        };
        snapshot.sync_from(term, size);
        snapshot
    }

    #[test]
    fn viewport_snapshot_size_is_independent_of_history() {
        let empty = test_term(4, 3, 30_000);
        let empty_snapshot = snapshot(&empty);

        let mut with_history = test_term(4, 3, 30_000);
        add_history(&mut with_history, 30_000);
        let history_snapshot = snapshot(&with_history);

        assert_eq!(with_history.grid().history_size(), 30_000);
        assert_eq!(empty_snapshot.cells.len(), 12);
        assert_eq!(history_snapshot.cells.len(), empty_snapshot.cells.len());
    }

    #[test]
    fn viewport_snapshot_reuses_capacity_at_the_same_size() {
        let mut term = test_term(4, 3, 30_000);
        let mut snapshot = snapshot(&term);
        let initial_capacity = snapshot.cells.capacity();

        add_history(&mut term, 100);
        snapshot.sync_from(&term, snapshot.terminal_size);

        assert_eq!(snapshot.cells.len(), 12);
        assert_eq!(snapshot.cells.capacity(), initial_capacity);
    }

    #[test]
    fn viewport_snapshot_uses_vi_mode_cursor() {
        let mut term = test_term(4, 3, 0);
        term.toggle_vi_mode();
        term.vi_mode_cursor.point = Point::new(Line(1), Column(2));

        let snapshot = snapshot(&term);

        assert_eq!(snapshot.cursor_point, Point::new(Line(1), Column(2)));
    }

    #[test]
    fn viewport_snapshot_normalizes_wide_character_spacer_cursor() {
        let mut term = test_term(4, 3, 0);
        let spacer = Point::new(Line(0), Column(1));
        term.grid_mut().cursor.point = spacer;
        term.grid_mut()[spacer]
            .flags
            .insert(Flags::WIDE_CHAR_SPACER);

        let snapshot = snapshot(&term);

        assert_eq!(snapshot.cursor_point, Point::new(Line(0), Column(0)));
    }

    #[test]
    fn text_for_range_includes_each_cell_once() {
        let mut term = test_term(4, 2, 0);
        term.grid_mut()[Line(0)][Column(0)].c = 'h';
        term.grid_mut()[Line(0)][Column(1)].c = 't';
        term.grid_mut()[Line(0)][Column(2)].c = 't';
        term.grid_mut()[Line(0)][Column(3)].c = 'p';
        let range =
            Point::new(Line(0), Column(0))..=Point::new(Line(0), Column(3));

        assert_eq!(Backend::text_for_range(&term, &range), "http");
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

#[derive(Clone)]
pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        // MUST NOT BLOCK. alacritty invokes this synchronously, and one of
        // the live callers is the Iced main thread inside `Backend::handle`
        // (any Term op — scroll/resize/select — can fire send_event while
        // we hold the term lock). The receiver is drained on a tokio worker
        // which then forwards via `output.send(...).await` back to the main
        // thread, so a full channel + main-thread `blocking_send` = deadlock.
        //
        // Wakeup / MouseCursorDirty / Bell / Title coalesce naturally — a
        // dropped Wakeup self-heals on the next PTY read or input event.
        // PtyWrite (terminal-program responses to OSC queries) is low rate
        // and the channel is sized large enough to never realistically drop.
        match self.0.try_send(event) {
            Ok(()) => {},
            Err(tokio::sync::mpsc::error::TrySendError::Full(ev)) => {
                eprintln!(
                    "[iced_term] send_event dropped (channel full): {:?}",
                    std::mem::discriminant(&ev),
                );
            },
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // Receiver gone — terminal is being torn down. Silent drop.
            },
        }
    }
}
