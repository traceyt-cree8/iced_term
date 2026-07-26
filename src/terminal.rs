use crate::actions::Action;
use crate::backend;
use crate::bindings::{Binding, BindingAction, BindingsLayout, InputKind};
use crate::font::TermFont;
use crate::settings::{FontSettings, Settings, ThemeSettings};
use crate::theme::{ColorPalette, Theme};
use crate::AlacrittyEvent;
use iced::futures::stream::BoxStream;
use iced::futures::{SinkExt, StreamExt};
use iced::widget::canvas::Cache;
use iced::Subscription;
use std::hash::{Hash, Hasher};
use std::io::Result;
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub enum Event {
    BackendCall(u64, backend::Command),
}

#[derive(Debug, Clone)]
pub enum Command {
    ChangeTheme(Box<ColorPalette>),
    ChangeFont(FontSettings),
    AddBindings(Vec<(Binding<InputKind>, BindingAction)>),
    ProxyToBackend(backend::Command),
}

pub struct Terminal {
    pub id: u64,
    widget_id: iced::widget::Id,
    pub(crate) font: TermFont,
    pub(crate) theme: Theme,
    pub(crate) cache: Cache,
    pub(crate) bindings: BindingsLayout,
    pub(crate) backend: backend::Backend,
    backend_event_rx: Arc<Mutex<Receiver<AlacrittyEvent>>>,
}

impl Terminal {
    pub fn new(id: u64, settings: Settings) -> Result<Self> {
        // Sized large to make `send_event` drops vanishingly rare under
        // burst load. The send side is non-blocking (`try_send`) — see
        // `EventProxy::send_event` in backend.rs for the deadlock that
        // motivated removing `blocking_send`.
        let (backend_event_tx, backend_event_rx) = mpsc::channel(4096);
        let theme = Theme::new(settings.theme);
        let font = TermFont::new(settings.font);

        Ok(Self {
            id,
            widget_id: iced::widget::Id::unique(),
            font,
            theme,
            bindings: BindingsLayout::default(),
            cache: Cache::default(),
            backend: backend::Backend::new(
                id,
                backend_event_tx,
                settings.backend,
            )?,
            backend_event_rx: Arc::new(Mutex::new(backend_event_rx)),
        })
    }

    pub fn widget_id(&self) -> &iced::widget::Id {
        &self.widget_id
    }

    pub fn subscription(&self) -> Subscription<Event> {
        let data = TerminalSubscriptionData {
            id: self.id,
            event_receiver: self.backend_event_rx.clone(),
        };

        Subscription::run_with(data, terminal_subscription_stream)
    }

    pub fn handle(&mut self, cmd: Command) -> Action {
        self.handle_internal(cmd, true)
    }

    /// Handle a command without syncing or redrawing. Use when the terminal
    /// is not visible (e.g. window unfocused / minimized) to avoid expensive
    /// font rasterization on a cold cache. Call `sync_and_redraw()` once
    /// when the window regains focus.
    pub fn handle_no_redraw(&mut self, cmd: Command) -> Action {
        self.handle_internal(cmd, false)
    }

    fn handle_internal(&mut self, cmd: Command, redraw: bool) -> Action {
        let mut action = Action::default();
        let mut visual_change = false;
        let mut should_sync = redraw;

        match cmd {
            Command::ChangeTheme(color_pallete) => {
                self.theme = Theme::new(ThemeSettings::new(color_pallete));
                visual_change = true;
            },
            Command::ChangeFont(font_settings) => {
                self.font = TermFont::new(font_settings);
                self.backend.handle(backend::Command::Resize(
                    None,
                    Some(self.font.measure),
                ));
                visual_change = true;
            },
            Command::AddBindings(bindings) => {
                self.bindings.add_bindings(bindings);
            },
            Command::ProxyToBackend(cmd) => {
                if let backend::Command::Resize(layout_size, font_measure) = cmd
                {
                    should_sync &=
                        self.backend.resize(layout_size, font_measure);
                } else {
                    action = self.backend.handle(cmd);
                }
            },
        };

        if should_sync {
            let content_changed = self.backend.sync();
            if content_changed || visual_change {
                self.redraw();
            }
        } else if visual_change {
            self.redraw();
        }
        action
    }

    /// Force a sync and redraw. Call after regaining focus if
    /// handle_no_redraw() was used while the window was unfocused.
    pub fn sync_and_redraw(&mut self) {
        if self.backend.sync() {
            self.redraw();
        }
    }

    fn redraw(&mut self) {
        self.cache.clear();
    }

    /// Search for all occurrences of a pattern in the terminal scrollback
    pub fn search_all(&mut self, pattern: &str) -> Vec<backend::SearchMatch> {
        let matches = self.backend.search_all(pattern);
        self.sync_and_redraw();
        matches
    }

    /// Get all terminal text content (including scrollback history)
    pub fn get_all_text(&self) -> String {
        self.backend.get_all_text()
    }

    /// Returns the current terminal mode flags (e.g. BRACKETED_PASTE, ALT_SCREEN).
    pub fn terminal_mode(&self) -> crate::TermMode {
        self.backend.last_content.terminal_mode
    }

    /// Scroll the terminal to show a specific line
    pub fn scroll_to_line(&mut self, line: i32) {
        self.backend.scroll_to_line(line);
        self.sync_and_redraw();
    }
}

#[derive(Clone)]
struct TerminalSubscriptionData {
    id: u64,
    event_receiver: Arc<Mutex<Receiver<AlacrittyEvent>>>,
}

impl Hash for TerminalSubscriptionData {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

fn terminal_subscription_stream(
    data: &TerminalSubscriptionData,
) -> BoxStream<'static, Event> {
    let id = data.id;
    let event_receiver = data.event_receiver.clone();
    iced::stream::channel(1000, async move |mut output| {
        let mut shutdown = false;
        loop {
            let mut event_receiver = event_receiver.lock().await;
            match event_receiver.recv().await {
                Some(event) => {
                    if let AlacrittyEvent::Exit = event {
                        shutdown = true
                    };

                    output
                        .send(Event::BackendCall(id, backend::Command::ProcessAlacrittyEvent(event)))
                        .await
                        .unwrap_or_else(|_| {
                            panic!("iced_term stream {}: sending BackendEventReceived event is failed", id)
                        });
                },
                None => {
                    if !shutdown {
                        panic!("iced_term stream {}: terminal event channel closed unexpected", id);
                    }
                },
            }
        }
    })
    .boxed()
}
