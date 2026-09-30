use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::diagnostics::{Entry, Level, ReadError, Reader, Source};

use super::{DiagnosticError, DiagnosticRow, MainWindow};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
struct Request {
    generation: u64,
    source: Source,
    manual: bool,
}

struct Completion {
    request: Request,
    result: Result<Vec<Entry>, ReadError>,
}

#[derive(Default)]
struct State {
    generation: u64,
    pending: bool,
    in_flight: bool,
    entries: Vec<Entry>,
    last_request: Option<Instant>,
}

impl State {
    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
        self.pending = true;
    }

    fn accepts(&self, request: Request, paused: bool) -> bool {
        self.generation == request.generation && (request.manual || !paused)
    }
}

pub(super) struct Controller {
    window: slint::Weak<MainWindow>,
    sender: mpsc::Sender<Request>,
    receiver: mpsc::Receiver<Completion>,
    state: RefCell<State>,
    timer: slint::Timer,
}

pub(super) fn attach(window: &MainWindow, reader: Reader, development: bool) -> Rc<Controller> {
    let (sender, requests) = mpsc::channel::<Request>();
    let (completions, receiver) = mpsc::channel();
    if let Err(error) = std::thread::Builder::new()
        .name("dogi-diagnostics".into())
        .spawn(move || {
            while let Ok(request) = requests.recv() {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reader.read(request.source)
                }))
                .unwrap_or(Err(ReadError::InvalidData));
                if completions.send(Completion { request, result }).is_err() {
                    break;
                }
            }
        })
    {
        log::error!("Could not start diagnostics reader: {error}");
    }
    let controller = Rc::new(Controller {
        window: window.as_weak(),
        sender,
        receiver,
        state: RefCell::new(State::default()),
        timer: slint::Timer::default(),
    });
    window.set_diagnostics_development(development);

    let weak = Rc::downgrade(&controller);
    window.on_open_diagnostics(move |source| {
        let Some(controller) = weak.upgrade() else {
            return;
        };
        let Some(window) = controller.window.upgrade() else {
            return;
        };
        window.set_diagnostics_source_index(source.clamp(0, 1));
        window.set_diagnostics_visible(true);
        window.set_diagnostics_paused(false);
        controller.reset(&window);
        let weak = Rc::downgrade(&controller);
        controller.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some(controller) = weak.upgrade() {
                    controller.tick();
                }
            },
        );
    });
    let weak = Rc::downgrade(&controller);
    window.on_close_diagnostics(move || {
        let Some(controller) = weak.upgrade() else {
            return;
        };
        if let Some(window) = controller.window.upgrade() {
            window.set_diagnostics_visible(false);
        }
        controller.state.borrow_mut().invalidate();
        controller.timer.stop();
    });
    let weak = Rc::downgrade(&controller);
    window.on_diagnostics_source_changed(move || {
        if let Some(controller) = weak.upgrade()
            && let Some(window) = controller.window.upgrade()
        {
            controller.reset(&window);
        }
    });
    let weak = Rc::downgrade(&controller);
    window.on_refresh_diagnostics(move || {
        if let Some(controller) = weak.upgrade()
            && let Some(window) = controller.window.upgrade()
        {
            controller.request(&window, true);
        }
    });
    let weak = Rc::downgrade(&controller);
    window.on_diagnostics_filter_changed(move || {
        if let Some(controller) = weak.upgrade()
            && let Some(window) = controller.window.upgrade()
        {
            present(&window, &controller.state.borrow().entries);
        }
    });
    controller
}

impl Controller {
    fn reset(&self, window: &MainWindow) {
        self.state.borrow_mut().invalidate();
        window.set_diagnostics_error(DiagnosticError::None);
        present(window, &[]);
        self.request(window, true);
    }

    fn request(&self, window: &MainWindow, manual: bool) {
        let mut state = self.state.borrow_mut();
        if state.in_flight || !window.get_diagnostics_visible() {
            return;
        }
        let request = Request {
            generation: state.generation,
            source: if window.get_diagnostics_source_index() == 1 {
                Source::Background
            } else {
                Source::Application
            },
            manual,
        };
        state.pending = false;
        state.last_request = Some(Instant::now());
        state.in_flight = self.sender.send(request).is_ok();
        window.set_diagnostics_busy(state.in_flight);
        if !state.in_flight {
            window.set_diagnostics_error(DiagnosticError::InvalidData);
        }
    }

    fn tick(&self) {
        let Some(window) = self.window.upgrade() else {
            self.timer.stop();
            return;
        };
        if !window.get_diagnostics_visible() {
            self.timer.stop();
            return;
        }
        while let Ok(completion) = self.receiver.try_recv() {
            let mut state = self.state.borrow_mut();
            state.in_flight = false;
            window.set_diagnostics_busy(false);
            if !state.accepts(completion.request, window.get_diagnostics_paused()) {
                continue;
            }
            match completion.result {
                Ok(entries) => {
                    window.set_diagnostics_error(DiagnosticError::None);
                    if entries != state.entries {
                        state.entries = entries;
                        present(&window, &state.entries);
                    }
                }
                Err(error) => window.set_diagnostics_error(error_kind(error)),
            }
        }
        let (pending, elapsed) = {
            let state = self.state.borrow();
            (
                state.pending,
                state
                    .last_request
                    .is_none_or(|time| time.elapsed() >= REFRESH_INTERVAL),
            )
        };
        if pending || (!window.get_diagnostics_paused() && elapsed) {
            self.request(&window, pending);
        }
    }
}

fn error_kind(error: ReadError) -> DiagnosticError {
    match error {
        ReadError::BackgroundUnavailable => DiagnosticError::BackgroundUnavailable,
        ReadError::JournalUnavailable => DiagnosticError::JournalUnavailable,
        ReadError::TimedOut => DiagnosticError::TimedOut,
        ReadError::TooLarge => DiagnosticError::TooLarge,
        ReadError::InvalidData => DiagnosticError::InvalidData,
    }
}

fn present(window: &MainWindow, entries: &[Entry]) {
    let filtered = entries
        .iter()
        .filter(|entry| entry.level.matches(window.get_diagnostics_level_index()))
        .collect::<Vec<_>>();
    let copy_text = filtered
        .iter()
        .map(|entry| entry.text())
        .collect::<Vec<_>>()
        .join("\n");
    let rows = filtered
        .into_iter()
        .rev()
        .map(|entry| DiagnosticRow {
            timestamp: entry.time().into(),
            level: entry.level.label().into(),
            severity: match entry.level {
                Level::Error => 0,
                Level::Warning => 1,
                Level::Info => 2,
                Level::Debug => 3,
            },
            message: entry.message.clone().into(),
            detail: entry.text().into(),
        })
        .collect::<Vec<_>>();
    window.set_diagnostics_copy_text(copy_text.into());
    window.set_diagnostic_rows(ModelRc::new(VecModel::from(rows)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_sources_and_paused_auto_refresh_cannot_replace_visible_logs() {
        let mut state = State::default();
        let request = Request {
            generation: 0,
            source: Source::Application,
            manual: false,
        };
        assert!(state.accepts(request, false));
        assert!(!state.accepts(request, true));
        assert!(state.accepts(
            Request {
                manual: true,
                ..request
            },
            true
        ));
        state.invalidate();
        assert!(!state.accepts(request, false));
        assert!(state.pending);
    }

    #[test]
    fn viewer_filters_copies_and_closes_with_a_headless_backend() {
        let runtime = slint_snapshot::SnapshotRuntime::builder()
            .clock_mode(slint_snapshot::runtime::ClockMode::Manual)
            .build()
            .unwrap();
        let window = MainWindow::new().unwrap();
        let _controller = attach(&window, Reader::default(), true);
        window.show().unwrap();
        runtime.set_size(window.window(), (1260, 780), 1.0).unwrap();
        window.invoke_open_diagnostics(0);
        assert!(window.get_diagnostics_visible());
        assert!(window.get_diagnostics_development());
        window.set_diagnostics_level_index(1);
        let entries = [
            Entry {
                timestamp_ms: 1000,
                level: Level::Info,
                message: "Started".into(),
            },
            Entry {
                timestamp_ms: 2000,
                level: Level::Warning,
                message: "Reconnecting".into(),
            },
        ];
        present(&window, &entries);
        use slint::Model;
        assert_eq!(window.get_diagnostic_rows().row_count(), 1);
        let copied = window.get_diagnostics_copy_text();
        assert!(copied.contains("Reconnecting"));
        assert!(!copied.contains("Started"));
        runtime.render(window.window()).unwrap();
        let position = slint::LogicalPosition { x: 460.0, y: 258.0 };
        let button = slint::platform::PointerEventButton::Left;
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
        assert!(
            window.get_diagnostics_paused(),
            "Selecting an entry freezes refresh"
        );
        runtime.render(window.window()).unwrap();
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
        assert!(!window.get_diagnostics_visible());
    }
}
