//! Terminal window context.

use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::mem;
#[cfg(not(windows))]
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

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
use crate::tab::{self, TabManager};
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
}

impl WindowContext {
    const BORDERLESS_RESIZE_HANDLE_SIZE: f32 = 8.0;

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
        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy.clone(),
            pty,
            pty_config.drain_on_exit,
            config.debug.ref_test,
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
            search_state: SearchState::default(),
            active: true,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
        };
        let initial_tab =
            tab::Tab { root: tab::PaneNode::Leaf(initial_pane), title: tab::Tab::auto_title(0) };
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

        // Collect tab bar info for rendering.
        let tab_bar_info = if self.tab_manager.tab_count() > 1 {
            let titles: Vec<String> = self
                .tab_manager
                .tabs()
                .iter()
                .enumerate()
                .map(|(index, _tab)| tab::Tab::auto_title(index))
                .collect();
            Some((titles, self.tab_manager.active_tab_index()))
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
                tab_bar_info.as_ref().map(|(t, i)| (t.as_slice(), *i)),
                self.close_button_hovered,
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
                tab_bar_info.as_ref().map(|(t, i)| (t.as_slice(), *i)),
                self.close_button_hovered,
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

        // Set cursor to pointer when hovering over the close button (after hint processing
        // which may reset the cursor).
        if let Some(direction) = self.borderless_resize_direction() {
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
        let pty_config = self.config.pty_config();

        let event_proxy = EventProxy::new(proxy.clone(), self.display.window.id());

        let terminal =
            Term::new(self.config.term_options(), &self.display.size_info, event_proxy.clone());
        let terminal = Arc::new(FairMutex::new(terminal));

        let pty = match tty::new(
            &pty_config,
            self.display.size_info.into(),
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

        let event_loop = match PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy,
            pty,
            pty_config.drain_on_exit,
            self.config.debug.ref_test,
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
            search_state: SearchState::default(),
            active: true,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
        };

        let new_tab = tab::Tab {
            root: tab::PaneNode::Leaf(pane),
            title: tab::Tab::auto_title(self.tab_manager.tab_count()),
        };

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
        let new_pane = match self.create_pane(proxy) {
            Some(pane) => pane,
            None => return,
        };

        self.tab_manager.active_tab_mut().root.split_active(direction, new_pane);

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

    /// Create a new pane (terminal + PTY) with the current display configuration.
    fn create_pane(&self, proxy: &EventLoopProxy<Event>) -> Option<tab::Pane> {
        let pty_config = self.config.pty_config();
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

        let event_loop = PtyEventLoop::new(
            Arc::clone(&terminal),
            event_proxy,
            pty,
            pty_config.drain_on_exit,
            self.config.debug.ref_test,
        )
        .ok()?;

        let loop_tx = event_loop.channel();
        let _io_thread = event_loop.spawn();

        Some(tab::Pane {
            terminal,
            notifier: Notifier(loop_tx),
            search_state: SearchState::default(),
            active: true,
            #[cfg(not(windows))]
            master_fd,
            #[cfg(not(windows))]
            shell_pid,
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
            TabAction::SelectTab(index) => {
                if index < self.tab_manager.tab_count() {
                    self.activate_tab(index, proxy);
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
