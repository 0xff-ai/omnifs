//! The inspector's keymap: one table of bindings drives both input
//! dispatch and the footer/help text, so a key can never be documented
//! without being wired or wired without being documented.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, AppView, PaneFocus};

#[derive(Clone, Copy)]
enum BindingScope {
    Global,
    Activity,
    Sandbox,
    Paused,
    Replay,
}

impl BindingScope {
    fn active(self, app: &App) -> bool {
        match self {
            Self::Global => true,
            Self::Activity => app.view == AppView::Activity,
            Self::Sandbox => app.view == AppView::Sandbox,
            Self::Paused => app.paused(),
            Self::Replay => app.is_replay,
        }
    }
}

#[derive(Clone, Copy)]
enum Command {
    Quit,
    ToggleView,
    CycleFocus,
    TogglePause,
    Navigate,
    Activate,
    SelectNext,
    SelectPrev,
    ToggleErrors,
    ToggleIdle,
    EditFilter,
    Reset,
    CycleMount,
    GoLive,
    StepScrub,
    NextError,
    ToggleOrder,
    Yank,
    Help,
    ReplaySlower,
    ReplayFaster,
}

impl Command {
    fn matches(self, key: &KeyEvent) -> bool {
        match self {
            Self::Quit => {
                matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
            },
            Self::ToggleView => key.code == KeyCode::Char('v'),
            Self::CycleFocus => key.code == KeyCode::Tab,
            Self::TogglePause => key.code == KeyCode::Char(' '),
            Self::Navigate => matches!(key.code, KeyCode::Up | KeyCode::Down),
            Self::Activate => key.code == KeyCode::Enter,
            Self::SelectNext => matches!(key.code, KeyCode::Char('j' | 'n')),
            Self::SelectPrev => matches!(key.code, KeyCode::Char('k' | 'p')),
            Self::ToggleErrors => key.code == KeyCode::Char('e'),
            Self::ToggleIdle => key.code == KeyCode::Char('i'),
            Self::EditFilter => key.code == KeyCode::Char('/'),
            Self::Reset => key.code == KeyCode::Char('r'),
            Self::CycleMount => key.code == KeyCode::Char('m'),
            Self::GoLive => key.code == KeyCode::Char('g'),
            Self::StepScrub => matches!(key.code, KeyCode::Left | KeyCode::Right),
            Self::NextError => key.code == KeyCode::Char('E'),
            Self::ToggleOrder => key.code == KeyCode::Char('l'),
            Self::Yank => key.code == KeyCode::Char('y'),
            Self::Help => key.code == KeyCode::Char('?'),
            Self::ReplaySlower => key.code == KeyCode::Char('['),
            Self::ReplayFaster => key.code == KeyCode::Char(']'),
        }
    }

    fn run(self, app: &mut App, key: &KeyEvent) {
        match self {
            Self::Quit => app.quit = true,
            Self::ToggleView => app.view = app.view.toggle(),
            Self::CycleFocus => app.focus = app.focus.cycle(),
            Self::TogglePause => app.toggle_pause(),
            Self::Navigate => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                match app.view {
                    AppView::Activity => app.move_focus_cursor(delta),
                    AppView::Sandbox => app.move_port_cursor(delta),
                }
            },
            Self::Activate if app.focus == PaneFocus::Tree => {
                app.toggle_tree_cursor_collapse();
            },
            Self::Activate => {},
            Self::SelectNext => app.select_next(),
            Self::SelectPrev => app.select_prev(),
            Self::ToggleErrors => app.toggle_errors_only(),
            Self::ToggleIdle => app.toggle_idle(),
            Self::EditFilter => app.start_filter_edit(),
            Self::Reset => app.reset_recent(),
            Self::CycleMount => app.cycle_active_mount(),
            Self::GoLive => app.go_live(),
            Self::StepScrub => {
                let direction = if key.code == KeyCode::Left { -1 } else { 1 };
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    app.jump_scrub_seconds(direction);
                } else if direction < 0 {
                    app.step_scrub_backward();
                } else {
                    app.step_scrub_forward();
                }
            },
            Self::NextError => app.select_next_error(),
            Self::ToggleOrder => app.toggle_operation_order(),
            Self::Yank => app.request_yank(),
            Self::Help => app.help_open = true,
            Self::ReplaySlower => app.replay_speed = app.replay_speed.slower(),
            Self::ReplayFaster => app.replay_speed = app.replay_speed.faster(),
        }
    }
}

struct KeyBinding {
    scope: BindingScope,
    command: Command,
    label: &'static str,
    description: &'static str,
    hidden: bool,
}

impl KeyBinding {
    const fn visible(
        scope: BindingScope,
        command: Command,
        label: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            scope,
            command,
            label,
            description,
            hidden: false,
        }
    }

    const fn hidden(
        scope: BindingScope,
        command: Command,
        label: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            scope,
            command,
            label,
            description,
            hidden: true,
        }
    }

    fn handles(&self, app: &App, key: &KeyEvent) -> bool {
        self.scope.active(app) && self.command.matches(key)
    }
}

const KEYMAP: &[KeyBinding] = &[
    KeyBinding::visible(BindingScope::Global, Command::Quit, "q", "quit"),
    KeyBinding::visible(BindingScope::Global, Command::Help, "?", "help"),
    KeyBinding::visible(BindingScope::Global, Command::ToggleView, "v", "view"),
    KeyBinding::visible(BindingScope::Activity, Command::CycleFocus, "tab", "focus"),
    KeyBinding::visible(BindingScope::Activity, Command::Navigate, "↑/↓", "navigate"),
    KeyBinding::visible(BindingScope::Activity, Command::Activate, "↵", "collapse"),
    KeyBinding::hidden(
        BindingScope::Activity,
        Command::SelectNext,
        "j/n",
        "next op",
    ),
    KeyBinding::hidden(
        BindingScope::Activity,
        Command::SelectPrev,
        "k/p",
        "prev op",
    ),
    KeyBinding::visible(BindingScope::Sandbox, Command::Navigate, "↑/↓", "port"),
    KeyBinding::visible(BindingScope::Sandbox, Command::CycleMount, "m", "mount"),
    KeyBinding::visible(BindingScope::Global, Command::TogglePause, "space", "pause"),
    KeyBinding::visible(BindingScope::Global, Command::ToggleErrors, "e", "errors"),
    KeyBinding::hidden(
        BindingScope::Activity,
        Command::NextError,
        "E",
        "next error",
    ),
    KeyBinding::visible(BindingScope::Activity, Command::ToggleOrder, "l", "latency"),
    KeyBinding::visible(BindingScope::Activity, Command::Yank, "y", "copy path"),
    KeyBinding::visible(BindingScope::Global, Command::ToggleIdle, "i", "idle"),
    KeyBinding::visible(BindingScope::Global, Command::EditFilter, "/", "filter"),
    KeyBinding::visible(BindingScope::Global, Command::Reset, "r", "reset"),
    KeyBinding::visible(BindingScope::Paused, Command::StepScrub, "←/→", "step"),
    KeyBinding::visible(BindingScope::Paused, Command::GoLive, "g", "live"),
    KeyBinding::visible(BindingScope::Replay, Command::ReplaySlower, "[", "slower"),
    KeyBinding::visible(BindingScope::Replay, Command::ReplayFaster, "]", "faster"),
];

/// Find the first binding whose scope is active and whose command matches
/// `key`, then run it. The single dispatch entry point `App::handle_key`
/// calls once the filter-edit and help-popup overlays have had first crack
/// at the key.
pub(crate) fn dispatch(app: &mut App, key: &KeyEvent) {
    if let Some(binding) = KEYMAP.iter().find(|binding| binding.handles(app, key)) {
        binding.command.run(app, key);
    }
}

/// Whether `key` is the quit shortcut, independent of any binding's scope.
/// Used by the help popup, which intercepts every other key itself but
/// still has to let `q`/Esc/Ctrl-C through.
pub(crate) fn quit_requested(key: &KeyEvent) -> bool {
    Command::Quit.matches(key)
}

/// Context-sensitive footer text generated from the same bindings that
/// dispatch input.
pub(crate) fn footer_text(app: &App) -> String {
    let parts: Vec<String> = KEYMAP
        .iter()
        .filter(|binding| binding.scope.active(app) && !binding.hidden)
        .map(|binding| format!("{} {}", binding.label, binding.description))
        .collect();
    format!(" {} ", parts.join("  "))
}

pub(crate) fn help_lines(app: &App) -> Vec<String> {
    KEYMAP
        .iter()
        .filter(|binding| binding.scope.active(app))
        .map(|binding| format!("{:<8} {}", binding.label, binding.description))
        .collect()
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn footer_and_dispatch_use_the_same_contextual_keymap() {
        let mut app = App::new(true, "test", None, Some("/omnifs".into()));
        let activity_footer = footer_text(&app);
        assert!(activity_footer.contains("tab focus"));
        assert!(!activity_footer.contains("m mount"));
        assert!(!activity_footer.contains("←/→ step"));

        let activity_samples = [
            KeyCode::Char('q'),
            KeyCode::Char('v'),
            KeyCode::Tab,
            KeyCode::Up,
            KeyCode::Enter,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char(' '),
            KeyCode::Char('e'),
            KeyCode::Char('i'),
            KeyCode::Char('/'),
            KeyCode::Char('r'),
        ];
        for code in activity_samples {
            let event = key(code);
            assert_eq!(
                KEYMAP
                    .iter()
                    .filter(|binding| binding.handles(&app, &event))
                    .count(),
                1,
                "{event:?}"
            );
        }

        app.view = AppView::Sandbox;
        let sandbox_footer = footer_text(&app);
        assert!(sandbox_footer.contains("m mount"));
        assert!(!sandbox_footer.contains("tab focus"));
        for code in [KeyCode::Up, KeyCode::Char('m')] {
            let event = key(code);
            assert_eq!(
                KEYMAP
                    .iter()
                    .filter(|binding| binding.handles(&app, &event))
                    .count(),
                1,
                "{event:?}"
            );
        }

        app.toggle_pause();
        let paused_footer = footer_text(&app);
        assert!(paused_footer.contains("←/→ step"));
        assert!(paused_footer.contains("g live"));
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Char('g')] {
            let event = key(code);
            assert_eq!(
                KEYMAP
                    .iter()
                    .filter(|binding| binding.handles(&app, &event))
                    .count(),
                1,
                "{event:?}"
            );
        }
    }
}
