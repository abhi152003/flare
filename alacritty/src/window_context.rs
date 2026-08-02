//! Terminal window context.

use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use glutin::config::Config as GlutinConfig;
use glutin::display::GetGlDisplay;
#[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
use glutin::platform::x11::X11GlConfigExt;
use log::info;
use serde_json as json;
use winit::event::{ElementState, Event as WinitEvent, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::{CursorIcon, ResizeDirection, WindowId};

use alacritty_terminal::event::Event as TerminalEvent;
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Direction;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty;

use crate::cli::{ParsedOptions, WindowOptions};
use crate::clipboard::Clipboard;
use crate::config::UiConfig;
use crate::config::window::Decorations;
use crate::display::window::Window;
use crate::display::{Display, tab_bar_close_button_bounds};
use crate::event::{
    ActionContext, Event, EventProxy, InlineSearchState, Mouse, SearchState, TabAction,
    TouchPurpose,
};
#[cfg(unix)]
use crate::logging::LOG_TARGET_IPC_CONFIG;
use crate::message_bar::MessageBuffer;
use crate::scheduler::Scheduler;
use crate::tab::{self, TabManager, Pane};
use crate::{input, renderer};

/// Event context for one individual Alacritty window.
pub struct WindowContext {
    pub message_buffer: MessageBuffer,
    pub display: Display,
    pub dirty: bool,
    event_queue: Vec<WinitEvent<Event>>,
    terminal: Arc<FairMutex<Term<EventProxy>>>,
    cursor_blink_timed_out: bool,
    prev_bell_cmd: Option<Instant>,
    modifiers: Modifiers,
    inline_search_state: InlineSearchState,
    search_state: SearchState,
    notifier: Notifier,
    mouse: Mouse,
    touch: TouchPurpose,
    occluded: bool,
    preserve_title: bool,
    #[cfg(not(windows))]
    master_fd: RawFd,
    #[cfg(not(windows))]
    shell_pid: u32,
    window_config: ParsedOptions,
    config: Rc<UiConfig>,
    tab_manager: TabManager,
    close_button_hovered: bool,
    palette_state: crate::palette::PaletteState,
    /// Active pane-border drag, if the left button went down on a split border.
    pane_drag: Option<tab::PaneDrag>,
    /// Durable window number (`w<number>`), unique per app lifetime (#28).
    pub window_number: crate::pane_address::PaneId,
}

impl WindowContext {
    const BORDERLESS_RESIZE_HANDLE_SIZE: f32 = 8.0;

    /// An agent whose last PTY output is older than this (ms) reads as Idle (#15).
    /// Picked above the 2s `AgentDetect` cadence so an agent emitting every 1-2s
    /// reliably stays Working.
    const IDLE_THRESHOLD_MILLIS: u64 = 3000;

    /// Create initial window context that does bootstrapping the graphics API we're going to use.
    pub fn initial(
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let raw_display_handle = event_loop.display_handle().unwrap().as_raw();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Windows has different order of GL platform initialization compared to any other platform;
        // it requires the window first.
        #[cfg(windows)]
        let window = Window::new(event_loop, &config, &identity, &mut options)?;
        #[cfg(windows)]
        let raw_window_handle = Some(window.raw_window_handle());

        #[cfg(not(windows))]
        let raw_window_handle = None;

        let gl_display = renderer::platform::create_gl_display(
            raw_display_handle,
            raw_window_handle,
            config.debug.prefer_egl,
        )?;
        let gl_config = renderer::platform::pick_gl_config(&gl_display, raw_window_handle)?;

        #[cfg(not(windows))]
        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, &gl_config, raw_window_handle)?;

        let display = Display::new(window, gl_context, &config, false)?;

        Self::new(display, config, options, proxy)
    }

    /// Create additional context with the graphics platform other windows are using.
    pub fn additional(
        gl_config: &GlutinConfig,
        event_loop: &ActiveEventLoop,
        proxy: EventLoopProxy<Event>,
        config: Rc<UiConfig>,
        mut options: WindowOptions,
        config_overrides: ParsedOptions,
    ) -> Result<Self, Box<dyn Error>> {
        let gl_display = gl_config.display();

        let mut identity = config.window.identity.clone();
        options.window_identity.override_identity_config(&mut identity);

        // Check if new window will be opened as a tab.
        // This must be done before `Window::new()`, which unsets `window_tabbing_id`.
        #[cfg(target_os = "macos")]
        let tabbed = options.window_tabbing_id.is_some();
        #[cfg(not(target_os = "macos"))]
        let tabbed = false;

        let window = Window::new(
            event_loop,
            &config,
            &identity,
            &mut options,
            #[cfg(all(feature = "x11", not(any(target_os = "macos", windows))))]
            gl_config.x11_visual(),
        )?;

        // Create context.
        let raw_window_handle = window.raw_window_handle();
        let gl_context =
            renderer::platform::create_gl_context(&gl_display, gl_config, Some(raw_window_handle))?;

        let display = Display::new(window, gl_context, &config, tabbed)?;

        let mut window_context = Self::new(display, config, options, proxy)?;

        // Set the config overrides at startup.
        //
        // These are already applied to `config`, so no update is necessary.
        window_context.window_config = config_overrides;

        Ok(window_context)
    }

    /// Create a new terminal window context.
    fn new(
        display: Display,
        config: Rc<UiConfig>,
        options: WindowOptions,
        proxy: EventLoopProxy<Event>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut pty_config = config.pty_config();
        options.terminal_options.override_pty_config(&mut pty_config);

        let preserve_title = options.window_identity.title.is_some();

        info!(
            "PTY dimensions: {:?} x {:?}",
            display.size_info.screen_lines(),
            display.size_info.columns()
        );

        let event_proxy = EventProxy::new(proxy, display.window.id());

        // Create the terminal.
        //
        // This object contains all of the state about what's being displayed. It's
        // wrapped in a clonable mutex since both the I/O loop and display need to
        // access it.
        let terminal = Term::new(config.term_options(), &display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        // Create the PTY.
        //
        // The PTY forks a process to run the shell on the slave side of the
        // pseudoterminal. A file descriptor for the master side is retained for
        // reading/writing to the shell.
        let pty = tty::new(&pty_config, display.size_info.into(), display.window.id().into())?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        // Create the pseudoterminal I/O loop.
        //
        // PTY I/O is ran on another thread as to not occupy cycles used by the
        // renderer and input processing. Note that access to the terminal state is
        // synchronized since the I/O loop updates the state, and the display
        // consumes it periodically.
        let last_output = Arc::new(AtomicU64::new(0));
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
            Some(Arc::clone(&last_output)),
        )?;

        // The event loop channel allows write requests from the event processor
        // to be sent to the pty loop and ultimately written to the pty.
        let loop_tx = event_loop.channel();

        // Kick off the I/O thread.
        let _io_thread = event_loop.spawn();

        // Start cursor blinking, in case `Focused` isn't sent on startup.
        if config.cursor.style().blinking {
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        // Create the initial tab from this terminal.
        let initial_pane = tab::Pane {
            terminal: Arc::clone(&terminal),
            notifier: Notifier(loop_tx.clone()),
            active: true,
            id: crate::pane_address::id::next_pane_id(),
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            agent: None,
            agent_status: Default::default(),
            last_output,
        };
        let initial_tab = tab::Tab { root: tab::PaneNode::Leaf(initial_pane), name: None, zoomed: false };
        let mut tab_manager = TabManager::new();
        tab_manager.add_tab(initial_tab);

        // Create context for the Alacritty window.
        Ok(WindowContext {
            preserve_title,
            terminal,
            display,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            config,
            notifier: Notifier(loop_tx),
            cursor_blink_timed_out: Default::default(),
            prev_bell_cmd: Default::default(),
            inline_search_state: Default::default(),
            message_buffer: Default::default(),
            window_config: Default::default(),
            search_state: Default::default(),
            event_queue: Default::default(),
            modifiers: Default::default(),
            occluded: Default::default(),
            mouse: Default::default(),
            touch: Default::default(),
            dirty: Default::default(),
            tab_manager,
            close_button_hovered: false,
            palette_state: Default::default(),
            pane_drag: None,
            window_number: crate::pane_address::id::next_window_id(),
        })
    }

    /// Update the terminal window to the latest config.
    pub fn update_config(&mut self, new_config: Rc<UiConfig>) {
        let old_config = mem::replace(&mut self.config, new_config);

        // Apply ipc config if there are overrides.
        self.config = self.window_config.override_config_rc(self.config.clone());
        if self.config.window.theme_preset.is_some() {
            let mut themed = (*self.config).clone();
            themed.apply_theme_preset();
            self.config = Rc::new(themed);
        }

        self.display.update_config(&self.config);
        self.terminal.lock().set_options(self.config.term_options());

        // Reload cursor if its thickness has changed.
        if (old_config.cursor.thickness() - self.config.cursor.thickness()).abs() > f32::EPSILON {
            self.display.pending_update.set_cursor_dirty();
        }

        if old_config.font != self.config.font {
            let scale_factor = self.display.window.scale_factor as f32;
            // Do not update font size if it has been changed at runtime.
            if self.display.font_size == old_config.font.size().scale(scale_factor) {
                self.display.font_size = self.config.font.size().scale(scale_factor);
            }

            let font = self.config.font.clone().with_size(self.display.font_size);
            self.display.pending_update.set_font(font);
        }

        // Always reload the theme to account for auto-theme switching.
        self.display.window.set_theme(self.config.window.theme());

        // Update display if either padding options or resize increments were changed.
        let window_config = &old_config.window;
        if window_config.padding(1.) != self.config.window.padding(1.)
            || window_config.dynamic_padding != self.config.window.dynamic_padding
            || window_config.resize_increments != self.config.window.resize_increments
        {
            self.display.pending_update.dirty = true;
        }

        // Update title on config reload according to the following table.
        //
        // │cli │ dynamic_title │ current_title == old_config ││ set_title │
        // │ Y  │       _       │              _              ││     N     │
        // │ N  │       Y       │              Y              ││     Y     │
        // │ N  │       Y       │              N              ││     N     │
        // │ N  │       N       │              _              ││     Y     │
        if !self.preserve_title
            && (!self.config.window.dynamic_title
                || self.display.window.title() == old_config.window.identity.title)
        {
            self.display.window.set_title(self.config.window.identity.title.clone());
        }

        let opaque = self.config.window_opacity() >= 1.;

        // Disable shadows for transparent windows on macOS.
        #[cfg(target_os = "macos")]
        self.display.window.set_has_shadow(opaque);

        #[cfg(target_os = "macos")]
        self.display.window.set_option_as_alt(self.config.window.option_as_alt());

        // Change opacity and blur state.
        self.display.window.set_transparent(!opaque);
        self.display.window.set_blur(self.config.window.blur);

        // Update hint keys.
        self.display.hint_state.update_alphabet(self.config.hints.alphabet());

        // Update cursor blinking.
        let event = Event::new(TerminalEvent::CursorBlinkingChange.into(), None);
        self.event_queue.push(event.into());

        self.dirty = true;
    }

    /// Get reference to the window's configuration.
    #[cfg(unix)]
    pub fn config(&self) -> &UiConfig {
        &self.config
    }

    /// Clear the window config overrides.
    #[cfg(unix)]
    pub fn reset_window_config(&mut self, config: Rc<UiConfig>) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.clear();

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Add new window config overrides.
    #[cfg(unix)]
    pub fn add_window_config(&mut self, config: Rc<UiConfig>, options: &ParsedOptions) {
        // Clear previous window errors.
        self.message_buffer.remove_target(LOG_TARGET_IPC_CONFIG);

        self.window_config.extend_from_slice(options);

        // Reload current config to pull new IPC config.
        self.update_config(config);
    }

    /// Check if the current mouse position is inside the close button area.
    /// Returns `Some(true)` if the mouse is over the close button, `Some(false)` if in
    /// borderless mode but not over the button, `None` if not in borderless mode.
    fn mouse_over_close_button(&self, _display_offset: usize) -> Option<bool> {
        if self.config.window.decorations != Decorations::None {
            return None;
        }

        let size_info = &self.display.size_info;
        let tabs_visible = self.tab_manager.tab_count() > 1;

        if tabs_visible {
            // When tabs are visible, the close button is at the right edge of the tab bar.
            let (btn_x, btn_y, btn_w, btn_h) = tab_bar_close_button_bounds(size_info, &self.config);

            let mouse_x = self.mouse.x as f32;
            let mouse_y = self.mouse.y as f32;

            Some(
                mouse_x >= btn_x
                    && mouse_x <= btn_x + btn_w
                    && mouse_y >= btn_y
                    && mouse_y <= btn_y + btn_h,
            )
        } else {
            // No tabs: close button is at viewport row 0, last 3 columns.
            let btn_columns = 3;
            let point = self.mouse.point(size_info, 0);

            Some(
                point.line == 0
                    && point.column.0 >= size_info.columns().saturating_sub(btn_columns),
            )
        }
    }

    fn full_pane_viewport(&self) -> tab::PaneViewport {
        let size_info = self.display.size_info;
        let content_y = size_info.padding_y() + size_info.tab_bar_offset_y();
        let content_height =
            size_info.height() - 2.0 * size_info.padding_y() - size_info.tab_bar_offset_y();

        tab::PaneViewport::new(
            size_info.padding_x(),
            content_y,
            size_info.width() - 2.0 * size_info.padding_x(),
            content_height,
        )
    }

    fn active_pane_size_info(&self) -> Option<crate::display::SizeInfo<f32>> {
        let active_tab = self.tab_manager.active_tab();
        if !active_tab.is_split() {
            return None;
        }

        let active_pane = active_tab.active_pane();
        let base = self.display.size_info;
        let viewport = active_tab
            .pane_viewports(self.full_pane_viewport())
            .into_iter()
            .find_map(|(viewport, pane)| std::ptr::eq(pane, active_pane).then_some(viewport))?;

        Some(crate::display::SizeInfo::new(
            // Match the pane-local SizeInfo used during rendering so input,
            // cursor movement, and selection use the same pane geometry.
            viewport.width + 2.0 * viewport.x,
            viewport.height + 2.0 * viewport.y,
            base.cell_width(),
            base.cell_height(),
            viewport.x,
            viewport.y,
            false,
            0.0,
        ))
    }

    fn focus_pane_at_mouse(&mut self, proxy: &EventLoopProxy<Event>) -> bool {
        let full_viewport = self.full_pane_viewport();
        let mouse_x = self.mouse.x as f32;
        let mouse_y = self.mouse.y as f32;

        let focused =
            self.tab_manager.active_tab_mut().focus_pane_at_point(full_viewport, mouse_x, mouse_y);

        if focused {
            self.activate_current_pane(proxy);
        }

        focused
    }

    /// If the cursor is on a split border, begin a pane-border drag and return
    /// `true` (the press is consumed by the drag). Reads `self.mouse` for the press
    /// position — `CursorMoved` precedes `MouseInput`, so it is current.
    fn maybe_start_pane_drag(&mut self) -> bool {
        if self.pane_drag.is_some() || !self.tab_manager.active_tab().is_split() {
            return false;
        }
        let viewport = self.full_pane_viewport();
        let x = self.mouse.x as f32;
        let y = self.mouse.y as f32;
        let hit = self
            .tab_manager
            .active_tab()
            .root
            .border_at_point(viewport, x, y, tab::SPLIT_GAP);
        if let Some(drag) = hit {
            self.pane_drag = Some(drag);
            true
        } else {
            false
        }
    }

    /// Apply an in-progress drag: recompute the grabbed split's ratio from the
    /// cursor position so the divider tracks the pointer. Clamps to [0.1, 0.9].
    fn update_pane_drag(&mut self, x: f32, y: f32) {
        let Some(drag) = self.pane_drag.clone() else { return };
        self.tab_manager.active_tab_mut().root.set_ratio_at(&drag, x, y);
        self.display.pending_update.dirty = true;
        self.dirty = true;
    }

    /// The cursor the window should show over / during a border drag on `drag`.
    /// A junction (two perpendicular borders) shows a 4-way move cursor; a single
    /// border shows the axis-appropriate resize arrow.
    fn drag_cursor(drag: &tab::PaneDrag) -> CursorIcon {
        if drag.is_junction() {
            CursorIcon::AllScroll
        } else {
            match drag.single_direction() {
                Some(tab::SplitDirection::Horizontal) => CursorIcon::ColResize,
                Some(tab::SplitDirection::Vertical) => CursorIcon::RowResize,
                None => CursorIcon::Default,
            }
        }
    }

    /// If the pointer is over a split border (but not dragging), return the resize
    /// cursor for it so the border is discoverable.
    fn hover_border_cursor(&self) -> Option<CursorIcon> {
        if self.pane_drag.is_some() || !self.tab_manager.active_tab().is_split() {
            return None;
        }
        let viewport = self.full_pane_viewport();
        let x = self.mouse.x as f32;
        let y = self.mouse.y as f32;
        self.tab_manager
            .active_tab()
            .root
            .border_at_point(viewport, x, y, tab::SPLIT_GAP)
            .as_ref()
            .map(Self::drag_cursor)
    }

    fn borderless_resize_direction(&self) -> Option<ResizeDirection> {
        if self.config.window.decorations != Decorations::None {
            return None;
        }

        let size = self.display.size_info;
        let x = self.mouse.x as f32;
        let y = self.mouse.y as f32;
        let margin = Self::BORDERLESS_RESIZE_HANDLE_SIZE;

        let near_left = x <= margin;
        let near_right = x >= size.width() - margin;
        let near_top = y <= margin;
        let near_bottom = y >= size.height() - margin;

        match (near_left, near_right, near_top, near_bottom) {
            (true, false, true, false) => Some(ResizeDirection::NorthWest),
            (true, false, false, true) => Some(ResizeDirection::SouthWest),
            (false, true, true, false) => Some(ResizeDirection::NorthEast),
            (false, true, false, true) => Some(ResizeDirection::SouthEast),
            (true, false, false, false) => Some(ResizeDirection::West),
            (false, true, false, false) => Some(ResizeDirection::East),
            (false, false, true, false) => Some(ResizeDirection::North),
            (false, false, false, true) => Some(ResizeDirection::South),
            _ => None,
        }
    }

    /// Draw the window.
    pub fn draw(&mut self, scheduler: &mut Scheduler) {
        self.display.window.requested_redraw = false;

        if self.occluded {
            return;
        }

        self.dirty = false;

        // Force the display to process any pending display update.
        self.display.process_renderer_update();

        // Request immediate re-draw if visual bell animation is not finished yet.
        if !self.display.visual_bell.completed() {
            // We can get an OS redraw which bypasses alacritty's frame throttling, thus
            // marking the window as dirty when we don't have frame yet.
            if self.display.window.has_frame {
                self.display.window.request_redraw();
            } else {
                self.dirty = true;
            }
        }

        // Collect tab bar info for rendering. Tabs are filtered by the current
        // agent-status filter (#16); the active tab is always included so the tab
        // you're viewing never disappears, and active_index is recomputed against
        // the filtered list.
        let filter = self.tab_manager.filter();
        let active_index = self.tab_manager.active_tab_index();
        let show_bar = self.tab_manager.tab_count() > 1 || filter.label().is_some();
        let tab_bar_info = if show_bar {
            let entries: Vec<tab::TabBarEntry> = self
                .tab_manager
                .tabs()
                .iter()
                .enumerate()
                .filter(|(index, tab)| {
                    // Always show the active tab; otherwise apply the status filter.
                    *index == active_index
                        || filter.matches(tab.active_pane().agent.map(|_| tab.active_pane().agent_status))
                })
                .map(|(index, tab)| {
                    let pane = tab.active_pane();
                    // Tag split tabs with the focused pane's durable address (#28).
                    let title = if tab.is_split() {
                        let address = crate::pane_address::PaneAddress::new(self.window_number, pane.id);
                        format!("{} · {address}", self.smart_tab_title(tab, index))
                    } else {
                        self.smart_tab_title(tab, index)
                    };
                    tab::TabBarEntry {
                        title,
                        agent: pane.agent,
                        agent_status: pane.agent.map(|_| pane.agent_status),
                    }
                })
                .collect();
            // Recompute the active tab's position within the filtered list: count
            // how many shown entries precede it. (The active tab is always shown.)
            let active_pos = self
                .tab_manager
                .tabs()
                .iter()
                .take(active_index)
                .filter(|t| {
                    let p = t.active_pane();
                    filter.matches(p.agent.map(|_| p.agent_status))
                })
                .count();
            Some((entries, active_pos, filter))
        } else {
            None
        };

        let active_tab = self.tab_manager.active_tab();

        if active_tab.is_split() {
            // Split pane rendering: draw each pane in its viewport region.
            self.display.draw_panes(
                active_tab,
                scheduler,
                &self.message_buffer,
                &self.config,
                &mut self.search_state,
                tab_bar_info.as_ref().map(|(e, i, f)| (e.as_slice(), *i, *f)),
                self.close_button_hovered,
                &self.palette_state,
            );
        } else {
            // Single pane: use the standard draw path.
            let terminal = self.terminal.lock();
            self.display.draw(
                terminal,
                scheduler,
                &self.message_buffer,
                &self.config,
                &mut self.search_state,
                tab_bar_info.as_ref().map(|(e, i, f)| (e.as_slice(), *i, *f)),
                self.close_button_hovered,
                &self.palette_state,
            );
        }
    }

    /// Process events for this terminal window.
    pub fn handle_event(
        &mut self,
        #[cfg(target_os = "macos")] event_loop: &ActiveEventLoop,
        event_proxy: &EventLoopProxy<Event>,
        clipboard: &mut Clipboard,
        scheduler: &mut Scheduler,
        event: WinitEvent<Event>,
    ) {
        // Check for close button click in borderless mode.
        if self.config.window.decorations == Decorations::None {
            if let WinitEvent::WindowEvent {
                event:
                    WindowEvent::MouseInput {
                        state: ElementState::Pressed,
                        button: winit::event::MouseButton::Left,
                        ..
                    },
                ..
            } = &event
            {
                let mut terminal = self.terminal.lock();
                let display_offset = terminal.grid().display_offset();
                if let Some(true) = self.mouse_over_close_button(display_offset) {
                    terminal.exit();
                    return;
                }

                drop(terminal);
                if let Some(direction) = self.borderless_resize_direction() {
                    let _ = self.display.window.drag_resize_window(direction);
                    return;
                }
            }
        }

        // Pane-border drag: while dragging, cursor moves resize the grabbed split
        // and left-release ends the drag. Intercepted here (before queueing) because
        // the input processor has no &mut access to the pane tree.
        if self.pane_drag.is_some() {
            if let WinitEvent::WindowEvent {
                event: WindowEvent::CursorMoved { position, .. },
                ..
            } = &event
            {
                let size_info = self.display.size_info;
                let x = position.x as f32 - size_info.padding_x();
                let y = position.y as f32 - size_info.padding_y() - size_info.tab_bar_offset_y();
                self.update_pane_drag(x, y);
                // Keep the resize cursor during the drag.
                let icon = self
                    .pane_drag
                    .as_ref()
                    .map(Self::drag_cursor)
                    .unwrap_or(CursorIcon::Text);
                self.display.window.set_mouse_cursor(icon);
                // We return early (skip queueing), so request the redraw ourselves —
                // otherwise the dirty flag set in update_pane_drag never draws.
                if self.display.window.has_frame && !self.occluded {
                    self.display.window.request_redraw();
                }
                return;
            }
            if let WinitEvent::WindowEvent {
                event:
                    WindowEvent::MouseInput {
                        state: ElementState::Released,
                        button: winit::event::MouseButton::Left,
                        ..
                    },
                ..
            } = &event
            {
                self.pane_drag = None;
                return;
            }
        }

        if let WinitEvent::WindowEvent {
            event:
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: winit::event::MouseButton::Left,
                    ..
                },
            ..
        } = &event
        {
            // A press on a split border starts a drag instead of focusing a pane.
            if self.maybe_start_pane_drag() {
                return;
            }
            if self.tab_manager.active_tab().is_split() {
                self.focus_pane_at_mouse(event_proxy);
            }
        }

        match event {
            WinitEvent::AboutToWait => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Skip further event handling with no staged updates.
                if self.event_queue.is_empty() {
                    return;
                }

                // Continue to process all pending events.
            },
            event => {
                self.event_queue.push(event);
                return;
            },
        }

        let mut terminal = self.terminal.lock();

        let old_is_searching = self.search_state.history_index.is_some();
        let mut pending_tab_action = None;
        let mut pending_session_restore = None;

        let pending_events = mem::take(&mut self.event_queue);
        let mut pending_events = pending_events.into_iter();
        while let Some(event) = pending_events.next() {
            let pane_size_info = self.active_pane_size_info();
            let context = ActionContext {
                cursor_blink_timed_out: &mut self.cursor_blink_timed_out,
                prev_bell_cmd: &mut self.prev_bell_cmd,
                message_buffer: &mut self.message_buffer,
                inline_search_state: &mut self.inline_search_state,
                search_state: &mut self.search_state,
                modifiers: &mut self.modifiers,
                notifier: &mut self.notifier,
                display: &mut self.display,
                mouse: &mut self.mouse,
                touch: &mut self.touch,
                dirty: &mut self.dirty,
                occluded: &mut self.occluded,
                terminal: &mut terminal,
                pane_size_info,
                #[cfg(not(windows))]
                master_fd: self.master_fd,
                #[cfg(not(windows))]
                shell_pid: self.shell_pid,
                preserve_title: self.preserve_title,
                config: &self.config,
                event_proxy,
                #[cfg(target_os = "macos")]
                event_loop,
                clipboard,
                scheduler,
                pending_tab_action: &mut pending_tab_action,
                palette_state: &mut self.palette_state,
                pending_session_restore: &mut pending_session_restore,
            };
            let mut processor = input::Processor::new(context);
            processor.handle_event(event);
            drop(processor);

            if pending_tab_action.is_some() {
                self.event_queue.extend(pending_events);
                break;
            }
        }

        // Update close button hover state to trigger redraws on color change.
        if self.config.window.decorations == Decorations::None {
            let display_offset = terminal.grid().display_offset();
            let is_hovered = self.mouse_over_close_button(display_offset).unwrap_or(false);

            if is_hovered != self.close_button_hovered {
                self.close_button_hovered = is_hovered;
                self.dirty = true;
            }
        }

        // Drop the terminal lock before processing tab actions (which may create new PTYs).
        drop(terminal);

        // Process pending tab/pane actions.
        if let Some(tab_action) = pending_tab_action {
            let old_tab_count = self.tab_manager.tab_count();
            self.handle_tab_action(tab_action, event_proxy);
            let new_tab_count = self.tab_manager.tab_count();

            // When the tab count changes the tab bar appears/disappears, so the display
            // must be resized to account for the new tab_bar_offset_y.
            if old_tab_count != new_tab_count {
                self.display.pending_update.dirty = true;
            }
        }

        // Process a pending palette-triggered session restore.
        if let Some(session) = pending_session_restore {
            self.restore_session(&session, event_proxy, false);
            self.display.pending_update.dirty = true;
        }

        // Re-acquire the terminal lock for display updates.
        let mut terminal = self.terminal.lock();

        // Process DisplayUpdate events.
        if self.display.pending_update.dirty {
            Self::submit_display_update(
                &mut terminal,
                &mut self.display,
                &mut self.notifier,
                &self.message_buffer,
                &mut self.search_state,
                old_is_searching,
                &self.config,
                self.tab_manager.tab_count(),
            );
            self.dirty = true;
        }

        if self.dirty || self.mouse.hint_highlight_dirty {
            self.dirty |= self.display.update_highlighted_hints(
                &terminal,
                &self.config,
                &self.mouse,
                self.modifiers.state(),
            );
            self.mouse.hint_highlight_dirty = false;
        }

        // Cursor feedback: resize icon when hovering a split border (discoverable
        // borders), pointer over the close button in borderless mode. Hints may have
        // reset the cursor, so this runs last.
        if let Some(icon) = self.hover_border_cursor() {
            self.display.window.set_mouse_cursor(icon);
        } else if let Some(direction) = self.borderless_resize_direction() {
            self.display.window.set_mouse_cursor(direction.into());
        } else if self.config.window.decorations == Decorations::None && self.close_button_hovered {
            self.display.window.set_mouse_cursor(CursorIcon::Pointer);
        }

        // Don't call `request_redraw` when event is `RedrawRequested` since the `dirty` flag
        // represents the current frame, but redraw is for the next frame.
        if self.dirty
            && self.display.window.has_frame
            && !self.occluded
            && !matches!(event, WinitEvent::WindowEvent { event: WindowEvent::RedrawRequested, .. })
        {
            self.display.window.request_redraw();
        }
    }

    /// ID of this terminal context.
    pub fn id(&self) -> WindowId {
        self.display.window.id()
    }

    /// Get a reference to the tab manager.
    #[allow(dead_code)]
    pub fn tab_manager(&self) -> &TabManager {
        &self.tab_manager
    }

    /// Get a mutable reference to the tab manager.
    #[allow(dead_code)]
    pub fn tab_manager_mut(&mut self) -> &mut TabManager {
        &mut self.tab_manager
    }

    /// Create a new tab with a fresh PTY and terminal.
    pub fn create_new_tab(&mut self, proxy: &EventLoopProxy<Event>) {
        let cwd = self.active_pane_cwd();
        self.create_new_tab_with_cwd(proxy, cwd, 0);
    }

    fn create_new_tab_with_cwd(
        &mut self,
        proxy: &EventLoopProxy<Event>,
        cwd: Option<PathBuf>,
        requested_id: u64,
    ) {
        let mut pty_config = self.config.pty_config();
        if cwd.is_some() {
            pty_config.working_directory = cwd;
        }

        let event_proxy = EventProxy::new(proxy.clone(), self.display.window.id());

        let future_tab_count = self.tab_manager.tab_count() + 1;
        let tab_bar_offset = crate::display::reserved_tab_bar_height(
            &self.config,
            self.display.size_info.cell_height(),
            future_tab_count,
        );

        let terminal_size = crate::display::SizeInfo::new(
            self.display.size_info.width(),
            self.display.size_info.height(),
            self.display.size_info.cell_width(),
            self.display.size_info.cell_height(),
            self.display.size_info.padding_x(),
            self.display.size_info.padding_y(),
            false,
            tab_bar_offset,
        );

        let terminal =
            Term::new(self.config.term_options(), &terminal_size, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        let pty = match tty::new(
            &pty_config,
            terminal_size.into(),
            self.display.window.id().into(),
        ) {
            Ok(pty) => pty,
            Err(err) => {
                log::error!("Failed to create PTY for new tab: {err}");
                return;
            },
        };

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        let last_output = Arc::new(AtomicU64::new(0));
        let event_loop = match PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy,
            pty,
            pty_config.drain_on_exit,
            self.config.debug.ref_test,
            Some(Arc::clone(&last_output)),
        ) {
            Ok(el) => el,
            Err(err) => {
                log::error!("Failed to create PTY event loop for new tab: {err}");
                return;
            },
        };

        let loop_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        let pane = tab::Pane {
            terminal: Arc::clone(&terminal),
            notifier: Notifier(loop_tx),
            active: true,
            id: resolve_pane_id(requested_id),
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            agent: None,
            agent_status: Default::default(),
            last_output,
        };

        let new_tab = tab::Tab { root: tab::PaneNode::Leaf(pane), name: None, zoomed: false };

        // Store the new tab's terminal state for later activation.
        self.tab_manager.add_tab(new_tab);

        // Now swap the active terminal into the WindowContext fields.
        self.activate_tab(self.tab_manager.active_tab_index(), proxy);
    }

    /// Switch to a specific tab by index.
    pub fn activate_tab(&mut self, index: usize, proxy: &EventLoopProxy<Event>) {
        if index >= self.tab_manager.tab_count() {
            return;
        }

        // Mark the previous terminal as unfocused.
        self.terminal.lock().is_focused = false;

        self.tab_manager.select_tab(index);

        let active_tab = self.tab_manager.active_tab();
        let active_pane = active_tab.active_pane();

        // Replace the active terminal with the one from the selected tab.
        self.terminal = Arc::clone(&active_pane.terminal);
        self.notifier = active_pane.notifier.clone();

        // Mark the new terminal as focused.
        self.terminal.lock().is_focused = true;
        #[cfg(not(windows))]
        {
            self.master_fd = active_pane.master_fd;
            self.shell_pid = active_pane.shell_pid;
        }

        // Start cursor blinking for the new terminal.
        if self.config.cursor.style().blinking {
            let event_proxy = EventProxy::new(proxy.clone(), self.display.window.id());
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        // Force a full repaint — the new tab's pixels completely replace the old
        // tab's, so every cell must be redrawn regardless of per-terminal damage.
        self.display.damage_tracker.frame().mark_fully_damaged();
        self.display.damage_tracker.next_frame().mark_fully_damaged();

        // Force a display update so the newly active terminal is resized to match
        // the current display dimensions (e.g. after the tab bar appeared/disappeared).
        self.display.pending_update.dirty = true;
        self.dirty = true;
    }

    /// Close the currently active tab.
    pub fn close_active_tab(&mut self, proxy: &EventLoopProxy<Event>) {
        if self.tab_manager.tab_count() <= 1 {
            return;
        }

        // Shut down all PTYs owned by the tab.
        let active_tab = self.tab_manager.active_tab();
        for pane in active_tab.root.iter_leaves() {
            let _ = pane.notifier.0.send(Msg::Shutdown);
        }

        let current_index = self.tab_manager.active_tab_index();
        self.tab_manager.close_tab(current_index);

        // Activate the now-current tab.
        self.activate_tab(self.tab_manager.active_tab_index(), proxy);
    }

    /// Split the active pane in the current tab, spawning a new PTY.
    pub fn split_active_pane(
        &mut self,
        direction: tab::SplitDirection,
        proxy: &EventLoopProxy<Event>,
    ) {
        let cwd = self.active_pane_cwd();
        let new_pane = match self.create_pane(proxy, cwd, 0) {
            Some(pane) => pane,
            None => return,
        };

        // Compute the viewport before borrowing the tab mutably.
        let viewport = self.full_pane_viewport();
        self.tab_manager.active_tab_mut().root.split(direction, new_pane, viewport);

        self.activate_current_pane(proxy);
    }

    /// Close the active pane in the current tab.
    ///
    /// If only one pane remains, does nothing (the last pane persists).
    /// If multiple panes exist, removes the active one and activates its sibling.
    pub fn close_active_pane(&mut self, proxy: &EventLoopProxy<Event>) {
        let tab = self.tab_manager.active_tab_mut();
        if tab.pane_count() <= 1 {
            return;
        }

        // Shut down the active pane's PTY.
        let _ = tab.active_pane().notifier.0.send(Msg::Shutdown);

        // Remove the active pane from the tree.
        tab.root.close_active();
        // Borrow of `tab` ends here, allowing `activate_current_pane` to borrow `tab_manager`.

        // Force a display update so the remaining pane's terminal is resized to fill
        // the space that was freed when the closed pane was removed.
        self.display.pending_update.dirty = true;

        // Un-zoom if closing the pane left a single pane.
        if !self.tab_manager.active_tab().is_split() {
            self.tab_manager.active_tab_mut().zoomed = false;
        }

        // Activate the new active pane.
        self.activate_current_pane(proxy);
    }

    /// Activate (focus) the active pane of the active tab, syncing the
    /// WindowContext's terminal/notifier fields.
    fn activate_current_pane(&mut self, proxy: &EventLoopProxy<Event>) {
        let active_pane = self.tab_manager.active_tab().active_pane();

        // Mark the old terminal as unfocused.
        self.terminal.lock().is_focused = false;

        self.terminal = Arc::clone(&active_pane.terminal);
        self.notifier = active_pane.notifier.clone();

        #[cfg(not(windows))]
        {
            self.master_fd = active_pane.master_fd;
            self.shell_pid = active_pane.shell_pid;
        }

        self.terminal.lock().is_focused = true;

        if self.config.cursor.style().blinking {
            let event_proxy = EventProxy::new(proxy.clone(), self.display.window.id());
            event_proxy.send_event(TerminalEvent::CursorBlinkingChange.into());
        }

        self.dirty = true;
    }

    fn active_pane_cwd(&self) -> Option<PathBuf> {
        #[cfg(not(windows))]
        {
            crate::daemon::foreground_process_path(self.master_fd, self.shell_pid).ok()
        }
        #[cfg(windows)]
        {
            None
        }
    }

    fn smart_tab_title(&self, tab: &tab::Tab, index: usize) -> String {
        // 1. Explicit user-set name wins.
        if let Some(name) = &tab.name {
            if !name.trim().is_empty() {
                return name.clone();
            }
        }
        let pane = tab.active_pane();
        let term = pane.terminal.lock();
        // Shell-reported title (OSC 0/2) — kept as the base when present.
        let shell_title = term.title.as_ref().filter(|t| !t.trim().is_empty()).cloned();
        // CWD the shell reported via OSC 7 (most accurate).
        let cwd = term.cwd.clone();
        drop(term);

        // Fall back to /proc if the shell hasn't reported OSC 7 yet.
        let cwd = cwd.or_else(|| self.pane_cwd(pane));

        // Resolve the git branch from whatever CWD we have (walks up to .git/HEAD).
        let branch = cwd.as_ref().and_then(|c| crate::path_util::git_branch(c.as_path()));

        // Base title: prefer the shell-reported title, then the shortened CWD.
        let title = if let Some(shell_title) = shell_title {
            shell_title
        } else if let Some(cwd) = cwd.as_ref() {
            crate::path_util::shorten_path(cwd)
        } else {
            // Fallback.
            return tab::Tab::auto_title(index);
        };

        // Append the branch unless the title already contains it (e.g. a prompt
        // that embeds the branch) to avoid duplication.
        let title = match branch {
            Some(branch) if !title.contains(&branch) => format!("{title} · {branch}"),
            _ => title,
        };

        // Append the detected agent's name (e.g. `· claude`) so the agent is identifiable
        // in the title, complementing the colored status dot.
        match pane.agent {
            Some(agent) if !title.contains(agent.label()) => {
                format!("{title} · {}", agent.label())
            },
            _ => title,
        }
    }

    fn pane_cwd(&self, pane: &tab::Pane) -> Option<PathBuf> {
        // Prefer the CWD the shell reported via OSC 7 (always accurate, even at exit time).
        if let Some(cwd) = pane.terminal.lock().cwd.clone() {
            return Some(cwd);
        }
        // Fallback: read /proc/<pid>/cwd while the shell process is still alive.
        #[cfg(not(windows))]
        {
            crate::daemon::foreground_process_path(pane.master_fd, pane.shell_pid).ok()
        }
        #[cfg(windows)]
        {
            let _ = pane;
            None
        }
    }

    /// The agent detected in the active pane, if any. Public accessor for the status-dot UI (#10).
    #[allow(dead_code)] // wired by the status-dot UI in #10.
    pub fn active_agent(&self) -> Option<crate::agent::AgentKind> {
        self.tab_manager.active_tab().active_pane().agent
    }

    /// Re-run agent detection across every pane in every tab.
    ///
    /// Reads each pane's foreground process name and matches it against the agent profiles in
    /// `agent::detect`. Returns true if any pane's detected agent changed, so the caller can
    /// request a redraw. Designed to be called on a periodic timer (see `Topic::AgentDetect`).
    pub fn detect_agents(&mut self) -> bool {
        let now_millis = UNIX_EPOCH.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
        let mut changed = false;
        for tab in self.tab_manager.tabs_mut() {
            for pane in tab.root.iter_leaves_mut() {
                #[cfg(not(windows))]
                let detected = crate::daemon::foreground_process_name(pane.master_fd, pane.shell_pid)
                    .ok()
                    .and_then(|name| crate::agent::detect(&name));
                #[cfg(windows)]
                let detected = None;

                // Derive Working/Idle from recent PTY activity (#15). Only meaningful
                // when an agent is actually running.
                let status = detected.map(|_| {
                    let last = pane.last_output.load(Ordering::Relaxed);
                    if now_millis.saturating_sub(last) < Self::IDLE_THRESHOLD_MILLIS {
                        crate::agent::AgentStatus::Working
                    } else {
                        crate::agent::AgentStatus::Idle
                    }
                });

                if pane.agent != detected {
                    pane.agent = detected;
                    changed = true;
                }
                if pane.agent_status != status.unwrap_or_default() {
                    pane.agent_status = status.unwrap_or_default();
                    changed = true;
                }
            }
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    pub fn collect_session(&self) -> Option<crate::session::SessionState> {
        let tabs = self.tab_manager.tabs();

        // Key the session by the first pane's CWD so it's naturally project-scoped.
        let first_pane = tabs.first()?.active_pane();
        let root = self.pane_cwd(first_pane).or_else(home::home_dir)?;
        let save_of = |pane: &tab::Pane| crate::session::PaneSaveInfo {
            cwd: self.pane_cwd(pane),
            id: pane.id,
            // Capture the running agent's command line so session restore can re-launch it
            // with its resume flag (#17).
            agent: pane.agent.map(|kind| {
                let cmdline = crate::daemon::foreground_process_cmdline(pane.master_fd, pane.shell_pid)
                    .unwrap_or_default();
                crate::session::AgentLeaf { kind: kind.label().to_string(), cmdline }
            }),
        };

        Some(crate::session::collect(root, tabs, save_of))
    }

    /// Replay a saved session into this window.
    ///
    /// The first pane of the first tab already exists, so its CWD is set via `cd`. Remaining
    /// panes are created by splits with explicit CWDs; additional tabs are spawned normally.
    pub fn restore_session(
        &mut self,
        session: &crate::session::SessionState,
        proxy: &EventLoopProxy<Event>,
        startup: bool,
    ) {
        const MAX_PANES: usize = 20;

        let mut tabs = session.tabs.iter().peekable();
        let Some(first_tab) = tabs.next() else {
            return;
        };

        let leaves = first_tab_leaves(&first_tab.root);

        // At startup the first pane spawns with the saved CWD (injected before window creation),
        // so no cd is needed. When restoring from the palette mid-session, the current pane is
        // already running in a different directory, so we cd it.
        if !startup {
            if let Some((first_cwd, _, _)) = leaves.first() {
                if let Some(cmd) = make_cd_command(first_cwd) {
                    let _ = self.notifier.0.send(Msg::Input(cmd.into()));
                }
            }
        }

        // Reassign the saved id to the already-created first pane, reserving it (#28).
        if let Some((_, first_id, _)) = leaves.first() {
            if *first_id != 0 {
                crate::pane_address::id::ensure_pane_at_least(*first_id);
                self.tab_manager.active_tab_mut().root.active_pane_mut().id = *first_id;
            }
        }

        let mut created = 1usize;
        for (cwd, id, agent) in leaves.iter().skip(1) {
            if created >= MAX_PANES {
                break;
            }
            let direction = if created % 2 == 0 {
                tab::SplitDirection::Horizontal
            } else {
                tab::SplitDirection::Vertical
            };
            self.split_active_pane_with_cwd(direction, cwd.clone(), *id, proxy);
            // After the split the new pane is active; re-launch its saved agent (#17).
            self.maybe_resume_active(agent);
            created += 1;
        }

        // Apply the first tab's saved name.
        self.tab_manager.active_tab_mut().name = first_tab.name.clone();

        // Re-launch the first pane's agent (it already exists; nothing is created here).
        if let Some((_, _, agent)) = leaves.first() {
            self.maybe_resume_active(agent);
        }

        for tab_state in tabs {
            let tab_leaves = first_tab_leaves(&tab_state.root);
            let first_leaf = tab_leaves.first();
            let first_cwd = first_leaf.map(|(cwd, _, _)| cwd.clone());
            let first_id = first_leaf.map(|(_, id, _)| *id).unwrap_or(0);
            self.create_new_tab_with_cwd(proxy, sanitize_cwd(first_cwd.as_deref()), first_id);
            self.tab_manager.active_tab_mut().name = tab_state.name.clone();

            for (cwd, id, agent) in tab_leaves.iter().skip(1) {
                if created >= MAX_PANES {
                    break;
                }
                let direction = if created % 2 == 0 {
                    tab::SplitDirection::Horizontal
                } else {
                    tab::SplitDirection::Vertical
                };
                self.split_active_pane_with_cwd(direction, cwd.clone(), *id, proxy);
                self.maybe_resume_active(agent);
                created += 1;
            }

            // Resume an agent in the first (already-created) pane of this tab.
            if let Some((_, _, agent)) = tab_leaves.first() {
                self.maybe_resume_active(agent);
            }
        }

        self.activate_tab(0, proxy);
        self.dirty = true;
    }

    /// Send a resume command to the active pane if its saved agent supports resume (#17).
    fn maybe_resume_active(&self, agent: &Option<crate::session::AgentLeaf>) {
        if let Some(command) = resume_command(agent) {
            let _ = self.notifier.0.send(Msg::Input(command.into_bytes().into()));
        }
    }

    fn split_active_pane_with_cwd(
        &mut self,
        direction: tab::SplitDirection,
        cwd: PathBuf,
        requested_id: u64,
        proxy: &EventLoopProxy<Event>,
    ) {
        let new_pane = match self.create_pane(proxy, Some(cwd), requested_id) {
            Some(pane) => pane,
            None => return,
        };
        // Compute the viewport before borrowing the tab mutably.
        let viewport = self.full_pane_viewport();
        self.tab_manager.active_tab_mut().root.split(direction, new_pane, viewport);
        self.activate_current_pane(proxy);
    }

    fn create_pane(
        &self,
        proxy: &EventLoopProxy<Event>,
        cwd: Option<PathBuf>,
        requested_id: u64,
    ) -> Option<tab::Pane> {
        let mut pty_config = self.config.pty_config();
        if let Some(cwd) = cwd {
            pty_config.working_directory = Some(cwd);
        }
        let event_proxy = EventProxy::new(proxy.clone(), self.display.window.id());

        let terminal =
            Term::new(self.config.term_options(), &self.display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        let pty =
            tty::new(&pty_config, self.display.size_info.into(), self.display.window.id().into())
                .ok()?;

        #[cfg(not(windows))]
        let master_fd = pty.file().as_raw_fd();
        #[cfg(not(windows))]
        let shell_pid = pty.child().id();

        let last_output = Arc::new(AtomicU64::new(0));
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy,
            pty,
            pty_config.drain_on_exit,
            self.config.debug.ref_test,
            Some(Arc::clone(&last_output)),
        )
        .ok()?;

        let loop_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        Some(tab::Pane {
            terminal,
            notifier: Notifier(loop_tx),
            active: true,
            id: resolve_pane_id(requested_id),
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
            agent: None,
            agent_status: Default::default(),
            last_output,
        })
    }

    /// Handle a pending tab/pane action from the input processor.
    fn handle_tab_action(&mut self, action: TabAction, proxy: &EventLoopProxy<Event>) {
        match action {
            TabAction::CreateNewTab => self.create_new_tab(proxy),
            TabAction::CloseTab => self.close_active_tab(proxy),
            TabAction::NextTab => {
                if self.tab_manager.tab_count() > 1 {
                    self.activate_tab(
                        (self.tab_manager.active_tab_index() + 1) % self.tab_manager.tab_count(),
                        proxy,
                    );
                }
            },
            TabAction::PreviousTab => {
                if self.tab_manager.tab_count() > 1 {
                    let len = self.tab_manager.tab_count();
                    let new_index = (self.tab_manager.active_tab_index() + len - 1) % len;
                    self.activate_tab(new_index, proxy);
                }
            },
            TabAction::SplitPaneHorizontal => {
                self.split_active_pane(tab::SplitDirection::Vertical, proxy);
            },
            TabAction::SplitPaneVertical => {
                self.split_active_pane(tab::SplitDirection::Horizontal, proxy);
            },
            TabAction::ClosePane => {
                self.close_active_pane(proxy);
            },
            TabAction::SwitchPaneLeft => {
                let full_viewport = self.full_pane_viewport();
                let tab = self.tab_manager.active_tab_mut();
                if tab.focus_adjacent_pane(tab::SplitDirection::Horizontal, true, full_viewport) {
                    self.activate_current_pane(proxy);
                }
            },
            TabAction::SwitchPaneRight => {
                let full_viewport = self.full_pane_viewport();
                let tab = self.tab_manager.active_tab_mut();
                if tab.focus_adjacent_pane(tab::SplitDirection::Horizontal, false, full_viewport) {
                    self.activate_current_pane(proxy);
                }
            },
            TabAction::SwitchPaneUp => {
                let full_viewport = self.full_pane_viewport();
                let tab = self.tab_manager.active_tab_mut();
                if tab.focus_adjacent_pane(tab::SplitDirection::Vertical, true, full_viewport) {
                    self.activate_current_pane(proxy);
                }
            },
            TabAction::SwitchPaneDown => {
                let full_viewport = self.full_pane_viewport();
                let tab = self.tab_manager.active_tab_mut();
                if tab.focus_adjacent_pane(tab::SplitDirection::Vertical, false, full_viewport) {
                    self.activate_current_pane(proxy);
                }
            },
            TabAction::ResizePaneLeft
            | TabAction::ResizePaneRight
            | TabAction::ResizePaneUp
            | TabAction::ResizePaneDown => {
                // Axis mapping (mirrors the split inversion): left/right resize a
                // Horizontal split (panes side by side); up/down a Vertical one
                // (panes stacked). Sign: growing `first`'s share moves the border
                // right/down; shrinking it moves the border left/up.
                let (axis, grow_first): (tab::SplitDirection, bool) = match action {
                    TabAction::ResizePaneLeft => (tab::SplitDirection::Horizontal, false),
                    TabAction::ResizePaneRight => (tab::SplitDirection::Horizontal, true),
                    TabAction::ResizePaneUp => (tab::SplitDirection::Vertical, false),
                    TabAction::ResizePaneDown => (tab::SplitDirection::Vertical, true),
                    // Unreachable: the guard above covers all four variants.
                    _ => return,
                };
                let delta = if grow_first { 0.05 } else { -0.05 };

                // Compute the viewport before borrowing the tab mutably.
                let viewport = self.full_pane_viewport();
                let tab = self.tab_manager.active_tab_mut();
                if !tab.zoomed && tab.root.resize_active(axis, delta, viewport) {
                    self.display.pending_update.dirty = true;
                    self.dirty = true;
                }
            },
            TabAction::TogglePaneZoom => {
                self.tab_manager.active_tab_mut().toggle_zoom();
                self.display.pending_update.dirty = true;
                self.dirty = true;
            },
            TabAction::CycleTabFilter => {
                // Pure view-state change — just cycle the tab-bar filter and redraw.
                self.tab_manager.cycle_filter();
                self.display.pending_update.dirty = true;
                self.dirty = true;
            },
        }
    }

    /// Write the ref test results to the disk.
    pub fn write_ref_test_results(&self) {
        // Dump grid state.
        let mut grid = self.terminal.lock().grid().clone();
        grid.initialize_all();
        grid.truncate();

        let serialized_grid = json::to_string(&grid).expect("serialize grid");

        let size_info = &self.display.size_info;
        let size = TermSize::new(size_info.columns(), size_info.screen_lines());
        let serialized_size = json::to_string(&size).expect("serialize size");

        let serialized_config = format!("{{\"history_size\":{}}}", grid.history_size());

        File::create("./grid.json")
            .and_then(|mut f| f.write_all(serialized_grid.as_bytes()))
            .expect("write grid.json");

        File::create("./size.json")
            .and_then(|mut f| f.write_all(serialized_size.as_bytes()))
            .expect("write size.json");

        File::create("./config.json")
            .and_then(|mut f| f.write_all(serialized_config.as_bytes()))
            .expect("write config.json");
    }

    /// Submit the pending changes to the `Display`.
    fn submit_display_update(
        terminal: &mut Term<EventProxy>,
        display: &mut Display,
        notifier: &mut Notifier,
        message_buffer: &MessageBuffer,
        search_state: &mut SearchState,
        old_is_searching: bool,
        config: &UiConfig,
        tab_count: usize,
    ) {
        // Compute cursor positions before resize.
        let num_lines = terminal.screen_lines();
        let cursor_at_bottom = terminal.grid().cursor.point.line + 1 == num_lines;
        let origin_at_bottom = if terminal.mode().contains(TermMode::VI) {
            terminal.vi_mode_cursor.point.line == num_lines - 1
        } else {
            search_state.direction == Direction::Left
        };

        display.handle_update(terminal, notifier, message_buffer, search_state, config, tab_count);

        let new_is_searching = search_state.history_index.is_some();
        if !old_is_searching && new_is_searching {
            // Scroll on search start to make sure origin is visible with minimal viewport motion.
            let display_offset = terminal.grid().display_offset();
            if display_offset == 0 && cursor_at_bottom && !origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(1));
            } else if display_offset != 0 && origin_at_bottom {
                terminal.scroll_display(Scroll::Delta(-1));
            }
        }
    }

    /// Address of the currently focused pane, e.g. `w1:p3` (#28).
    // Consumed by the JSON socket (#33) and CLI pane control (#34).
    #[allow(dead_code)]
    pub fn active_pane_address(&self) -> crate::pane_address::PaneAddress {
        let pane = self.tab_manager.active_tab().root.active_pane();
        crate::pane_address::PaneAddress::new(self.window_number, pane.id)
    }

    /// All panes in this window with their stable addresses and metadata (#33).
    pub fn pane_infos(&self) -> Vec<PaneInfo> {
        self.tab_manager
            .tabs()
            .iter()
            .flat_map(|tab| tab.root.iter_leaves())
            .map(|pane| self.pane_info(pane))
            .collect()
    }

    /// Metadata for a single pane in this window (#33).
    pub fn pane_info(&self, pane: &Pane) -> PaneInfo {
        // Fetch cwd before locking: pane_cwd takes the same non-reentrant FairMutex.
        let cwd = self.pane_cwd(pane);

        let terminal = pane.terminal.lock();
        PaneInfo {
            address: crate::pane_address::PaneAddress::new(self.window_number, pane.id),
            cwd,
            agent: pane.agent.map(|agent| agent.label().to_string()),
            agent_status: pane.agent.map(|_| format!("{:?}", pane.agent_status)),
            title: terminal.title.clone(),
            columns: terminal.columns(),
            lines: terminal.screen_lines(),
            scrollback: terminal.grid().history_size(),
        }
    }

    /// Extract the last `max_lines` lines of output from a pane (#33).
    pub fn pane_output(&self, pane: &Pane, max_lines: usize) -> String {
        pane.terminal.lock().scrollback_text(max_lines)
    }

    /// Find a pane by its durable id in any tab of this window.
    // Consumed by the JSON socket (#33) and CLI pane control (#34).
    #[allow(dead_code)]
    pub fn pane_for_address(&self, address: &crate::pane_address::PaneAddress) -> Option<&Pane> {
        if address.window != 0 && address.window != self.window_number {
            return None;
        }
        self.tab_manager.tabs().iter().find_map(|tab| {
            tab.root.iter_leaves().into_iter().find(|pane| pane.id == address.pane)
        })
    }

    /// Focus the pane with the given durable id, activating its tab if needed.
    ///
    /// Returns `false` when no pane in this window has that id.
    // Consumed by the JSON socket (#33) and CLI pane control (#34).
    #[allow(dead_code)]
    pub fn focus_pane_by_address(&mut self, address: &crate::pane_address::PaneAddress) -> bool {
        if address.window != 0 && address.window != self.window_number {
            return false;
        }
        for (index, tab) in self.tab_manager.tabs_mut().iter_mut().enumerate() {
            if tab.root.focus_pane_by_id(address.pane) {
                self.tab_manager.select_tab(index);
                self.dirty = true;
                return true;
            }
        }
        false
    }
}

/// One pane as reported by the IPC pane listing (#33).
#[derive(serde::Serialize)]
pub struct PaneInfo {
    #[serde(serialize_with = "crate::pane_address::serialize_address")]
    pub address: crate::pane_address::PaneAddress,
    pub cwd: Option<PathBuf>,
    pub agent: Option<String>,
    pub agent_status: Option<String>,
    pub title: Option<String>,
    pub columns: usize,
    pub lines: usize,
    pub scrollback: usize,
}

impl std::fmt::Display for PaneInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.address)?;
        if let Some(cwd) = &self.cwd {
            write!(f, "\t{}", cwd.display())?;
        }
        if let (Some(agent), Some(status)) = (&self.agent, &self.agent_status) {
            write!(f, "\t{agent}\t{status}")?;
        }
        Ok(())
    }
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        // Shutdown all tabs' PTYs.
        for tab in self.tab_manager.tabs() {
            for pane in tab.root.iter_leaves() {
                let _ = pane.notifier.0.send(Msg::Shutdown);
            }
        }
    }
}

// Session-restore helpers.

/// Each restored leaf: its CWD, durable pane id, and any agent captured at save time.
type LeafInfo = (PathBuf, u64, Option<crate::session::AgentLeaf>);

fn first_tab_leaves(node: &crate::session::PaneNodeState) -> Vec<LeafInfo> {
    let mut out = Vec::new();
    collect_session_leaves(node, &mut out);
    out
}

fn collect_session_leaves(node: &crate::session::PaneNodeState, out: &mut Vec<LeafInfo>) {
    match node {
        crate::session::PaneNodeState::Leaf { cwd, id, agent } => {
            out.push((cwd.clone(), *id, agent.clone()))
        },
        crate::session::PaneNodeState::Split { first, second, .. } => {
            collect_session_leaves(first, out);
            collect_session_leaves(second, out);
        }
    }
}

/// Resolve the id for a newly-created pane: reuse a restored `requested_id` (reserving it so the
/// counter never reissues it), or allocate a fresh never-reused id.
fn resolve_pane_id(requested: u64) -> u64 {
    if requested == 0 {
        crate::pane_address::id::next_pane_id()
    } else {
        crate::pane_address::id::ensure_pane_at_least(requested);
        requested
    }
}

/// Build the shell input that re-launches a captured agent with its resume flag, if supported.
fn resume_command(agent: &Option<crate::session::AgentLeaf>) -> Option<String> {
    let leaf = agent.as_ref()?;
    let kind = match crate::agent::detect(&leaf.kind) {
        // If we can't map the stored kind string back to a known agent, treat it as Unknown
        // (never resumed).
        Some(kind) => kind,
        None => crate::agent::AgentKind::Unknown,
    };
    let argv = crate::agent::resume_args(kind, &leaf.cmdline)?;
    // Append a newline so the command is submitted (Enter), not just typed into the pane.
    Some(format!("{}\n", argv.join(" ")))
}

fn sanitize_cwd(path: Option<&Path>) -> Option<PathBuf> {
    let path = path?;
    if path.is_dir() {
        Some(path.to_path_buf())
    } else {
        None
    }
}

fn make_cd_command(path: &Path) -> Option<Vec<u8>> {
    let s = path.to_str()?;
    if s.is_empty() {
        return None;
    }
    let quoted = s.replace('\'', "'\\''");
    Some(format!("cd '{quoted}'\n").into_bytes())
}
