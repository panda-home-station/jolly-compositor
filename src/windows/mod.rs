//! Window management.

use std::borrow::Cow;
use std::cell::{RefCell, RefMut};
use std::mem;
use std::cmp;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tracing::info;

use _decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use catacomb_ipc::{AppIdMatcher, ClientInfo, WindowScale};
use smithay::backend::drm::DrmEventMetadata;
use smithay::backend::renderer::element::{Element, RenderElementStates};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_protocols::xdg::decoration as _decoration;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::compositor;
use smithay::wayland::session_lock::LockSurface;
use smithay::wayland::shell::wlr_layer::{Layer, LayerSurface};
use smithay::wayland::shell::xdg::{PopupSurface, ToplevelSurface};
use crate::xwayland::X11Surface;

use crate::catacomb::Catacomb;
use crate::drawing::{CatacombElement, Graphics};
use crate::input::{HandleGesture, TouchState};
use crate::layer::Layers;
use crate::orientation::Orientation;
use crate::output::{Canvas, GESTURE_HANDLE_HEIGHT, Output};
use crate::overview::{DragActionType, DragAndDrop, Overview};
use crate::windows::layout::{LayoutPosition, Layouts};
use crate::windows::surface::{CatacombLayerSurface, InputSurface, InputSurfaceKind, Surface, ShellSurface};
use crate::windows::window::Window;
use smithay::output::Mode;
use std::collections::HashMap;
use std::time::Instant as StdInstant;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use dirs;
use libc;

pub mod layout;
pub mod surface;
pub mod window;

/// Transaction timeout for locked sessions.
const MAX_LOCKED_TRANSACTION_MILLIS: u64 = 100;

/// Maximum time before a transaction is cancelled.
const MAX_TRANSACTION_MILLIS: u64 = 1000;

/// Global transaction timer in milliseconds.
static TRANSACTION_START: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct LaunchCtx {
    #[allow(dead_code)]
    command: String,
    command_key: Option<String>,
    card_id: Option<String>,
    #[allow(dead_code)]
    time: StdInstant,
    pid: Option<i32>,
    pgid: Option<i32>,
}

/// Start a new transaction.
///
/// This will reset the transaction start to the current system time if there's
/// no transaction pending, setting up the timeout for the transaction.
pub fn start_transaction() {
    // Skip when transaction is already active.
    if TRANSACTION_START.load(Ordering::Relaxed) != 0 {
        return;
    }
    let now = UNIX_EPOCH.elapsed().unwrap().as_millis() as u64;
    TRANSACTION_START.store(now, Ordering::Relaxed);
}

/// Perform operation for every known window.
macro_rules! with_all_windows {
    ($windows:expr, | $window:tt | $fn:expr) => {{
        match $windows.pending_view() {
            View::Lock(Some($window)) => $fn,
            _ => (),
        }

        for $window in $windows.layouts.windows() {
            $fn
        }
        for $window in $windows.layers.iter() {
            $fn
        }
    }};
}

/// Perform mutable operation for every known window.
macro_rules! with_all_windows_mut {
    ($windows:expr, | $window:tt | $fn:expr) => {{
        match $windows.pending_view_mut() {
            View::Lock(Some($window)) => $fn,
            _ => (),
        }

        for mut $window in $windows.layouts.windows_mut() {
            $fn
        }
        for $window in $windows.layers.iter_mut() {
            $fn
        }
    }};
}

/// Container tracking all known clients.
#[derive(Debug)]
pub struct Windows {
    pub window_scales: Vec<(AppIdMatcher, WindowScale)>,

    orphan_popups: Vec<Window<PopupSurface>>,
    layouts: Layouts,
    layers: Layers,
    view: View,

    event_loop: LoopHandle<'static, Catacomb>,
    activated: Option<ShellSurface>,
    transaction: Option<Transaction>,
    textures: Vec<CatacombElement>,
    start_time: Instant,
    pub output: Output,
    system_roles: HashMap<String, AppIdMatcher>,
    last_launch: Option<(String, Instant)>,
    /// Pending launch context for mapping command/card to observed app_id.
    pending_launch: Option<LaunchCtx>,
    /// Pending resume for process group (PGID, ResumeTime, AppID).
    pending_resume: Option<(i32, Instant, Option<String>)>,
    /// Pending suspend for process group (PGID, SuspendTime).
    pending_suspend: Option<(i32, Instant)>,
    /// Role of the currently activated window (persisted even if window is destroyed).
    active_role: Option<String>,
       /// Mapping from normalized command key to strict app_id.
    command_map: HashMap<String, String>,
    /// Mapping from card_id to strict app_id (reserved for future use).
    card_map: HashMap<String, String>,

    /// Cached output state for rendering.
    ///
    /// This is used to tie the transactions to their respective output size and
    /// should be passed to anyone who doesn't communicate with clients about
    /// future updates, but instead tries to calculate things for the next
    /// rendered frame.
    canvas: Canvas,

    /// Orientation independent from [`Windows::orientation_locked`] state.
    unlocked_orientation: Orientation,
    orientation_locked: bool,

    /// IME force enable/disable state.
    ime_override: Option<bool>,

    /// Client-independent damage.
    dirty: bool,
    trace_scene_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Mappings {
    command_map: HashMap<String, String>,
    card_map: HashMap<String, String>,
}

impl Windows {
    pub fn new(display: &DisplayHandle, event_loop: LoopHandle<'static, Catacomb>) -> Self {
        let output = Output::new_dummy(display);
        let canvas = *output.canvas();

        let mut s = Self {
            event_loop,
            output,
            canvas,
            start_time: Instant::now(),
            orientation_locked: true,
            dirty: true,
            unlocked_orientation: Default::default(),
            orphan_popups: Default::default(),
            window_scales: Default::default(),
            ime_override: Default::default(),
            transaction: Default::default(),
            activated: Default::default(),
            textures: Default::default(),
            layouts: Default::default(),
            layers: Default::default(),
            view: Default::default(),
            system_roles: Default::default(),
            last_launch: Default::default(),
            pending_launch: Default::default(),
            pending_resume: None,
            pending_suspend: None,
            active_role: None,
            command_map: Default::default(),
            card_map: Default::default(),
            trace_scene_enabled: false,
        };
        s.load_mappings();
        s
    }

    pub fn set_trace_scene_enabled(&mut self, enabled: bool) {
        self.trace_scene_enabled = enabled;
        if self.trace_scene_enabled {
            self.log_scene_stack();
        }
    }

    pub fn update_mode(&mut self, mode: Mode) {
        info!("Windows: Updating mode to {}x{}@{}mHz", mode.size.w, mode.size.h, mode.refresh);
        self.output.set_mode(mode);
        self.canvas = *self.output.canvas();
        
        let new_size = self.canvas.size();
        let scale = self.output.scale();
        info!("Windows: Re-configuring windows to new size {:?}", new_size);
        
        for layout in self.layouts.layouts() {
            if let Some(primary) = layout.primary() {
                primary.borrow_mut().set_dimensions(scale, Rectangle::from_size(new_size));
            }
            if let Some(secondary) = layout.secondary() {
                secondary.borrow_mut().set_dimensions(scale, Rectangle::from_size(new_size));
            }
        }
        self.dirty = true;
    }

    fn log_scene_stack(&self) {
        let v = self.pending_view();
        let name = match v {
            View::Workspace => "Workspace",
            View::Overview(_) => "Overview",
            View::DragAndDrop(_) => "DragAndDrop",
            View::Fullscreen(_) => "Fullscreen",
            View::Lock(_) => "Lock",
        };
        let active_id = self.layouts.active().id.0;
        info!("Scene: view={} active_layout_id={}", name, active_id);
        let mut i = 0usize;
        for layout in self.layouts.layouts() {
            let marker = if layout.id == self.layouts.active().id { "*" } else { " " };
            let p = layout.primary().map(|w| w.borrow().title().unwrap_or_default()).unwrap_or_default();
            let s = layout.secondary().map(|w| w.borrow().title().unwrap_or_default()).unwrap_or_default();
            info!("{} [{}] primary='{}' secondary='{}'", marker, i, p, s);
            i += 1;
        }
        let mut j = 0usize;
        for layer in self.layers.iter() {
            let t = layer.title().unwrap_or_default();
            let a = layer.xdg_app_id().or(layer.app_id.clone()).unwrap_or_default();
            info!("[L{}] title='{}' app_id='{}'", j, t, a);
            j += 1;
        }
        if let Some((title, app_id)) = self.active_window_info() {
            info!("Focus: title='{}' app_id='{}'", title, app_id);
        } else {
            info!("Focus: None");
        }
    }

    pub fn log_focus_graph(&self) {
        info!("===== Focus Graph =====");
        self.log_scene_stack();
        info!("=======================");
    }

    pub fn system_role(&self, role: &str) -> Option<AppIdMatcher> {
        self.system_roles.get(role).cloned()
    }

    pub fn focus_mapped_command(&mut self, key: &str) -> bool {
        if let Some(app_id_str) = self.command_map.get(key).cloned() {
            if let Ok(app) = AppIdMatcher::try_from(app_id_str) {
                return self.focus_app(app);
            }
        }
        false
    }

    pub fn focus_mapped_card(&mut self, card_id: &str) -> bool {
        if let Some(app_id_str) = self.card_map.get(card_id).cloned() {
            if let Ok(app) = AppIdMatcher::try_from(app_id_str) {
                return self.focus_app(app);
            }
        }
        false
    }

    fn mappings_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("catacomb").join("mappings.json"))
    }

    fn load_mappings(&mut self) {
        if let Some(path) = Self::mappings_path() {
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(m) = serde_json::from_str::<Mappings>(&data) {
                    self.command_map = m.command_map;
                    self.card_map = m.card_map;
                }
            }
        }
    }

    fn save_mappings(&self) {
        if let Some(path) = Self::mappings_path() {
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let m = Mappings {
                command_map: self.command_map.clone(),
                card_map: self.card_map.clone(),
            };
            if let Ok(json) = serde_json::to_string_pretty(&m) {
                let _ = fs::write(path, json);
            }
        }
    }

    pub fn log_window_tree(&self) {
        info!("===== Window Tree Dump =====");
        let active_id = self.layouts.active().id;
        info!("View: {:?}", match &self.view {
            View::Overview(_) => "Overview",
            View::DragAndDrop(_) => "DragAndDrop",
            View::Fullscreen(_) => "Fullscreen",
            View::Lock(_) => "Lock",
            View::Workspace => "Workspace",
        });
        info!("Active layout id: {}", active_id.0);
        info!("Parent layouts: {:?}", self.layouts.parent_layouts.iter().map(|id| id.0).collect::<Vec<_>>());
        for (i, layout) in self.layouts.layouts().iter().enumerate() {
            let marker = if layout.id == active_id { "*" } else { " " };
            info!("{} Layout[{}] id={}", marker, i, layout.id.0);
            if let Some(primary) = layout.primary() {
                let w = primary.borrow();
                let title = w.title().unwrap_or_default();
                let xdg_id = w.xdg_app_id().unwrap_or_default();
                let app_id = w.app_id.clone().unwrap_or_default();
                let alive = w.alive();
                let ty = match &w.surface {
                    ShellSurface::Xdg(_) => "XDG",
                    ShellSurface::X11(_) => "X11",
                };
                info!("    Primary: alive={} type={} title='{}' xdg_id='{}' app_id='{}'", alive, ty, title, xdg_id, app_id);
            } else {
                info!("    Primary: None");
            }
            if let Some(secondary) = layout.secondary() {
                let w = secondary.borrow();
                let title = w.title().unwrap_or_default();
                let xdg_id = w.xdg_app_id().unwrap_or_default();
                let app_id = w.app_id.clone().unwrap_or_default();
                let alive = w.alive();
                let ty = match &w.surface {
                    ShellSurface::Xdg(_) => "XDG",
                    ShellSurface::X11(_) => "X11",
                };
                info!("    Secondary: alive={} type={} title='{}' xdg_id='{}' app_id='{}'", alive, ty, title, xdg_id, app_id);
            } else {
                info!("    Secondary: None");
            }
        }
        let mut i = 0;
        for layer in self.layers.iter() {
            let title = layer.title().unwrap_or_default();
            let app_id = layer.xdg_app_id().or(layer.app_id.clone()).unwrap_or_default();
            let alive = layer.alive();
            info!("Layer[{}]: alive={} title='{}' app_id='{}'", i, alive, title, app_id);
            i += 1;
        }
        if let Some((title, app_id)) = self.active_window_info() {
            info!("Focus: title='{}' app_id='{}'", title, app_id);
        } else {
            info!("Focus: None");
        }
        info!("============================");
    }
    /// Focus the first window matching the App ID.
    pub fn focus_app(&mut self, app_id: AppIdMatcher) -> bool {
        info!("focus_app: Request to focus {:?}", app_id);
        
        // Find window position for the requested App ID.
        let position = self.layouts.layouts().iter().enumerate().find_map(|(i, layout)| {
            // Check primary window.
            if let Some(primary) = layout.primary() {
                let window = primary.borrow();
                let xdg_id = window.xdg_app_id();
                let id = xdg_id.as_ref().or(window.app_id.as_ref());
                let title = window.title();

                if app_id.matches(id) {
                    return Some(LayoutPosition::new(i, false));
                }
                
                // Fallback: Check title
                if let Some(t) = &title {
                    if app_id.matches(Some(t)) {
                        return Some(LayoutPosition::new(i, false));
                    }
                }
            }

            // Check secondary window.
            if let Some(secondary) = layout.secondary() {
                let window = secondary.borrow();
                let xdg_id = window.xdg_app_id();
                let id = xdg_id.as_ref().or(window.app_id.as_ref());
                let title = window.title();

                if app_id.matches(id) {
                    return Some(LayoutPosition::new(i, true));
                }
                
                // Fallback: Check title
                if let Some(t) = &title {
                    if app_id.matches(Some(t)) {
                        return Some(LayoutPosition::new(i, true));
                    }
                }
            }

            None
        });

        // Switch to the found window.
        if let Some(position) = position {
            // Special handling for Overlay to preserve parent history
            let is_overlay = {
                if let Some(layout) = self.layouts.get(position.index) {
                     layout.primary().map(|w| w.borrow().title().unwrap_or_default() == "JollyPad-Overlay").unwrap_or(false)
                } else {
                    false
                }
            };

            if is_overlay {
                info!("focus_app: Switching to Overlay (position {:?}). Pushing active to parent.", position);
                self.layouts.push_active_to_parent();
                self.layouts.set_active(&self.output, Some(position), false);
                
                // Ensure view is Workspace or Fullscreen(overlay) so it actually renders
                if matches!(self.view, View::Fullscreen(_)) {
                    // If we were in fullscreen app, we need to switch view to overlay if we want it on top
                    // Or if we want overlay ON TOP of fullscreen, we need to handle that.
                    // For now, let's try switching to Workspace view which handles layers + layouts
                    self.set_view(View::Workspace);
                }
                
                // FORCE RESIZE to ensure it renders on top
                self.resize_visible();
            } else {
                info!("focus_app: Switching to normal app (position {:?}). Clearing parents.", position);
                self.layouts.set_active(&self.output, Some(position), true);
            }
            // Clear layer focus to ensure toplevel activation logic works
            self.layers.focus = None;
            true
        } else {
            info!("focus_app: No matching window found.");
            false
        }
    }

    /// Toggle application visibility: focus if hidden/inactive, switch to previous if active.
    pub fn toggle_app(&mut self, app_id: AppIdMatcher) -> bool {
        // Check if the app is currently active
        // We check both title and app_id because sometimes the matcher is against title
        let (title, current_id) = self.active_window_info().unwrap_or(("None".to_string(), "None".to_string()));
        
        let is_active = app_id.matches(Some(&current_id)) || app_id.matches(Some(&title));
        
        info!("toggle_app: Request={:?}, ActiveTitle='{}', ActiveID='{}', IsActive={}", app_id, title, current_id, is_active);

        if is_active {
            // If active, we want to "hide" it. 
            info!("toggle_app: Hiding app. Layout count: {}", self.layouts.len());
            for (i, l) in self.layouts.layouts().iter().enumerate() {
                 let p = l.primary().map(|w| w.borrow().title().unwrap_or_default()).unwrap_or("None".into());
                 let s = l.secondary().map(|w| w.borrow().title().unwrap_or_default()).unwrap_or("None".into());
                 info!("  Layout {}: Primary='{}', Secondary='{}'", i, p, s);
            }
            
            // If we are in a fullscreen view of this app, leave fullscreen.
            if let View::Fullscreen(window) = &self.view {
                let window = window.borrow();
                let xdg_id = window.xdg_app_id();
                let id = xdg_id.as_ref().or(window.app_id.as_ref());
                let title = window.title();
                
                // Match against ID or Title
                if app_id.matches(id) || (title.is_some() && app_id.matches(title.as_ref())) {
                    drop(window);
                    self.set_view(View::Workspace);
                    self.resize_visible();
                }
            }

            // If we are in workspace view, switch to the previous layout.
            // Prefer system role 'home' if registered.
            let role_home = self.system_roles.get("home");
            let desktop_index = self.layouts.layouts().iter().position(|l| {
                if let Some(p) = l.primary() {
                    let w = p.borrow();
                    let t = w.title().unwrap_or_default();
                    let xdg_id = w.xdg_app_id().unwrap_or_default();
                    let app_id = w.app_id.clone().unwrap_or_default();
                    if let Some(home_matcher) = role_home {
                        home_matcher.matches(Some(&app_id)) || home_matcher.matches(Some(&t)) || home_matcher.matches(Some(&xdg_id))
                    } else {
                        false
                    }
                } else {
                    false
                }
            });

            // If we have a parent history, use it first!
            let parent_layout_id = self.layouts.parent_layouts.last().copied();
            
            if let Some(parent_id) = parent_layout_id {
                 info!("toggle_app: Found parent layout ID {:?}, switching back to it.", parent_id);
                 if let Some(index) = self.layouts.layouts().iter().position(|l| l.id == parent_id) {
                     self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
                     return true;
                 } else {
                     info!("toggle_app: Parent layout ID {:?} not found in current layouts, falling back.", parent_id);
                 }
            }

            if let Some(index) = desktop_index {
                 info!("toggle_app: Switching explicitly to Desktop at index {}", index);
                 self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
            } else if self.layouts.len() <= 1 {
                self.layouts.set_active(&self.output, None, true);
            } else {
                // Fallback: If we can't find desktop, and we have multiple layouts, 
                // defaulting to Layout 0 is safer than cycling blindly if cycling fails.
                info!("toggle_app: Desktop not found, forcing switch to Layout 0");
                self.layouts.set_active(&self.output, Some(LayoutPosition::new(0, false)), true);
            }
             true

        } else {
            // If not active, focus it.
            self.focus_app(app_id)
        }
    }

    pub fn set_system_role(&mut self, role: String, app_id: AppIdMatcher) {
        self.system_roles.insert(role, app_id);
    }

    /// Mark X11 window as dead by window id.
    pub fn mark_dead_x11(&mut self, window_id: u32) {
        start_transaction();
        let mut killed_fullscreen = false;
        if let View::Fullscreen(window) = &self.view {
            if let ShellSurface::X11(s) = &window.borrow().surface {
                if s.window_id() == window_id {
                    killed_fullscreen = true;
                }
            }
        }
        for mut window in self.layouts.windows_mut() {
            if let ShellSurface::X11(s) = &window.surface {
                if s.window_id() == window_id {
                    window.kill();
                }
            }
        }
        if killed_fullscreen {
            self.start_transaction().view = Some(View::Workspace);
            let role_home = self.system_roles.get("home");
            let desktop_index = self.layouts.layouts().iter().position(|l| {
                if let Some(p) = l.primary() {
                    let w = p.borrow();
                    let t = w.title().unwrap_or_default();
                    let xdg_id = w.xdg_app_id().unwrap_or_default();
                    let app_id = w.app_id.clone().unwrap_or_default();
                    if let Some(home_matcher) = role_home {
                        home_matcher.matches(Some(&app_id)) || home_matcher.matches(Some(&t)) || home_matcher.matches(Some(&xdg_id))
                    } else {
                        t == "JollyPad-Desktop" || xdg_id == "jolly-home" || app_id == "jolly-home"
                    }
                } else {
                    false
                }
            });
            if let Some(index) = desktop_index {
                self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
            } else if !self.layouts.is_empty() {
                self.layouts.set_active(&self.output, Some(LayoutPosition::new(0, false)), true);
            } else {
                self.layouts.set_active(&self.output, None, true);
            }
        }
    }

    /// Get active window info (title, app_id).
    pub fn active_window_info(&self) -> Option<(String, String)> {
        if let Some(weak) = self.layouts.focus.as_ref() {
            if let Some(window_rc) = weak.upgrade() {
                let window = window_rc.borrow();
                // Prefer xdg_app_id if available, fallback to window.app_id
                let xdg_id = window.xdg_app_id();
                let win_id = window.app_id.clone();
                let app_id = xdg_id.clone().or(win_id.clone()).unwrap_or_default();
                let title = window.title().unwrap_or_default();
                return Some((title, app_id));
            }
        }
        None
    }

    /// Get list of clients.
    pub fn clients_info(&self) -> Vec<ClientInfo> {
        self.layouts
            .windows()
            .map(|window| ClientInfo {
                title: window.title().unwrap_or_default(),
                app_id: window.xdg_app_id().or(window.app_id.clone()).unwrap_or_default(),
                pid: window.process_group,
            })
            .collect()
    }

    /// Close an application by App ID.
    pub fn close_app(&mut self, app_id: AppIdMatcher) -> bool {
        let mut closed = false;
        for mut window in self.layouts.windows_mut() {
            let t = window.title().unwrap_or_default();
            let xdg_id = window.xdg_app_id().unwrap_or_default();
            let w_app_id = window.app_id.clone().unwrap_or_default();
            
            if app_id.matches(Some(&w_app_id)) || app_id.matches(Some(&t)) || app_id.matches(Some(&xdg_id)) {
                window.kill();
                closed = true;
            }
        }
        closed
    }

    pub fn note_launch_context(
        &mut self,
        command: String,
        command_key: Option<String>,
        card_id: Option<String>,
        pgid: Option<i32>,
    ) {
        info!("LAUNCH: exec_spawned command='{}' key='{:?}' card_id='{:?}' pgid='{:?}'", command, command_key, card_id, pgid);
        self.last_launch = Some((command.clone(), Instant::now()));
        self.pending_launch = Some(LaunchCtx {
            command,
            command_key,
            card_id,
            time: StdInstant::now(),
            pid: None,
            pgid,
        });
    }
    pub fn note_launch(&mut self, command: String) {
        info!("LAUNCH: exec_spawned command='{}'", command);
        self.last_launch = Some((command.clone(), Instant::now()));
        self.pending_launch = Some(LaunchCtx {
            command,
            command_key: None,
            card_id: None,
            time: StdInstant::now(),
            pid: None,
            pgid: None,
        });
    }
    pub fn update_pending_pgid(&mut self, pid: i32) {
        if let Some(ctx) = self.pending_launch.as_mut() {
            ctx.pid = Some(pid);
            ctx.pgid = Some(pid);
        }
    }
    /// Add a new window.
    pub fn add(&mut self, surface: ToplevelSurface) {
        // Set general window states.
        surface.with_pending_state(|state| {
            // Force Fullscreen for handheld experience
            state.states.set(State::Fullscreen);
            state.states.set(State::Activated);

            // Always use server-side decoration.
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });

        let surface = ShellSurface::Xdg(surface);

        let transform = self.output.orientation().surface_transform();
        let scale = self.output.scale();

        let pgid = self.pending_launch.as_ref().and_then(|ctx| ctx.pgid);
        let window = Rc::new(RefCell::new(Window::new(surface, scale, transform, None, pgid)));
        self.layouts.create(&self.output, window.clone());

        // Auto-focus the new window
        self.layouts.focus = Some(Rc::downgrade(&window));

        // Only switch to Fullscreen if it's NOT the overlay
        let is_overlay = window.borrow().title().unwrap_or_default() == "JollyPad-Overlay";
        if !is_overlay {
            // Switch view to Fullscreen immediately for normal apps
            self.view = View::Fullscreen(window.clone());
        } else {
            // For overlay, ensure we stay in Workspace mode so we can see underlay
            self.view = View::Workspace;
        }

        if let Some((cmd, t)) = self.last_launch.take() {
            let title = window.borrow().title().unwrap_or_default();
            let xdg_id = window.borrow().xdg_app_id();
            let win_id = window.borrow().app_id.clone();
            let app_id = xdg_id.clone().or(win_id.clone()).unwrap_or_default();
            let elapsed = t.elapsed().as_millis();
            info!("LAUNCH: window_created title='{}' xdg_app_id={:?} app_id={:?} final='{}' elapsed={}ms command='{}'", title, xdg_id, win_id, app_id, elapsed, cmd);
        }
        // Bind pending launch mappings to observed app_id.
        if let Some(ctx) = self.pending_launch.take() {
            if let Some(key) = ctx.command_key {
                self.command_map.insert(key, window.borrow().xdg_app_id().or(window.borrow().app_id.clone()).unwrap_or_default());
            }
            if let Some(card) = ctx.card_id {
                self.card_map.insert(card, window.borrow().xdg_app_id().or(window.borrow().app_id.clone()).unwrap_or_default());
            }
            self.save_mappings();
        }
    }

    /// Add a new X11 window.
    pub fn add_x11(&mut self, surface: X11Surface) {
        let surface = ShellSurface::X11(surface);

        let transform = self.output.orientation().surface_transform();
        let scale = self.output.scale();

        let pgid = self.pending_launch.as_ref().and_then(|ctx| ctx.pgid);
        let window = Rc::new(RefCell::new(Window::new(surface, scale, transform, None, pgid)));
        self.layouts.create(&self.output, window.clone());

        // Auto-focus the new window
        self.layouts.focus = Some(Rc::downgrade(&window));

        // Switch view to Fullscreen immediately
        self.view = View::Fullscreen(window.clone());

        if let Some((cmd, t)) = self.last_launch.take() {
            let title = window.borrow().title().unwrap_or_default();
            let app_id = window.borrow().xdg_app_id().or(window.borrow().app_id.clone()).unwrap_or_default();
            let elapsed = t.elapsed().as_millis();
            info!("LAUNCH: x11_window_created title='{}' app_id='{}' elapsed={}ms command='{}'", title, app_id, elapsed, cmd);
        }
        if let Some(ctx) = self.pending_launch.take() {
            if let Some(key) = ctx.command_key {
                self.command_map.insert(key, window.borrow().xdg_app_id().or(window.borrow().app_id.clone()).unwrap_or_default());
            }
            if let Some(card) = ctx.card_id {
                self.card_map.insert(card, window.borrow().xdg_app_id().or(window.borrow().app_id.clone()).unwrap_or_default());
            }
            self.save_mappings();
        }
    }

    /// Add a new layer shell window.
    pub fn add_layer(&mut self, layer: Layer, surface: LayerSurface, namespace: String) {
        let transform = self.output.orientation().surface_transform();
        let scale = self.output.scale();

        let surface = CatacombLayerSurface::new(layer, surface);
        let mut window = Window::new(surface, scale, transform, Some(namespace), None);

        window.enter(&self.output);
        self.layers.add(window);
    }

    /// Add a new popup window.
    pub fn add_popup(&mut self, popup: PopupSurface) {
        let transform = self.output.orientation().surface_transform();
        let scale = self.output.scale();

        self.orphan_popups.push(Window::new(popup, scale, transform, None, None));
    }

    /// Move popup location.
    pub fn reposition_popup(&mut self, popup: &PopupSurface, token: u32) {
        for mut window in self.layouts.windows_mut() {
            let scale = self.output.scale();
            window.reposition_popup(scale, popup, token);
        }
    }

    /// Update the session lock surface.
    pub fn set_lock_surface(&mut self, surface: LockSurface) {
        let surface_transform = self.output.orientation().surface_transform();
        let output_scale = self.output.scale();
        let output_size = self.output.size();

        let lock_window = match self.pending_view_mut() {
            View::Lock(lock_window) => lock_window,
            _ => return,
        };

        // Set lock surface size.
        let mut window = Window::new(surface, output_scale, surface_transform, None, None);
        window.set_dimensions(output_scale, Rectangle::from_size(output_size));

        // Update lockscreen.
        *lock_window = Some(Box::new(window));
    }

    /// Lock the session.
    pub fn lock(&mut self) {
        self.set_view(View::Lock(None));
    }

    /// Unlock the session.
    pub fn unlock(&mut self) {
        self.start_transaction().view = Some(View::Workspace);
    }

    /// Find the XDG shell window responsible for a specific surface.
    pub fn find_xdg(&mut self, wl_surface: &WlSurface) -> Option<RefMut<'_, Window>> {
        // Get root surface.
        let mut wl_surface = Cow::Borrowed(wl_surface);
        while let Some(surface) = compositor::get_parent(&wl_surface) {
            wl_surface = Cow::Owned(surface);
        }

        self.layouts
            .windows_mut()
            .find(|window| window.surface.maybe_surface().map_or(false, |root| root == *wl_surface.as_ref()))
    }

    /// Handle a surface commit for any window.
    pub fn surface_commit(&mut self, surface: &WlSurface) {
        // Get the topmost surface for window comparison.
        let mut root_surface = Cow::Borrowed(surface);
        while let Some(parent) = compositor::get_parent(&root_surface) {
            root_surface = Cow::Owned(parent);
        }

        // Find a window matching the root surface.
        macro_rules! find_window {
            ($windows:expr) => {{
                $windows.find(|window| {
                    window
                        .surface
                        .maybe_surface()
                        .map_or(false, |root| root == *root_surface.as_ref())
                })
            }};
        }

        // Handle session lock surface commits.
        let scale = self.output.scale();
        if let View::Lock(Some(window)) = self.pending_view_mut() {
            if window.surface() == *root_surface.as_ref() {
                window.surface_commit_common(scale, &[], surface);
                return;
            }
        }

        // Handle XDG surface commits.
        if let Some(mut window) = find_window!(self.layouts.windows_mut()) {
            window.surface_commit_common(scale, &self.window_scales, surface);
            return;
        }

        // Handle popup orphan adoption.
        self.orphan_surface_commit(&root_surface);

        // Apply popup surface commits.
        for mut window in self.layouts.windows_mut() {
            if window.popup_surface_commit(scale, &root_surface, surface) {
                // Abort as soon as we found the parent.
                return;
            }
        }
        for window in self.layers.iter_mut() {
            if window.popup_surface_commit(scale, &root_surface, surface) {
                // Abort as soon as we found the parent.
                return;
            }
        }

        // Abort if we can't find any window for this surface.
        let window = match find_window!(self.layers.iter_mut()) {
            Some(window) => window,
            None => return,
        };

        // Handle layer shell surface commits.
        let old_exclusive = *self.output.exclusive();
        let fullscreen_active = matches!(self.view, View::Fullscreen(_));
        window.surface_commit(&self.window_scales, &mut self.output, fullscreen_active, surface);

        // Resize windows after exclusive zone changes.
        if self.output.exclusive() != &old_exclusive {
            self.resize_visible();
        }
    }

    /// Handle orphan popup surface commits.
    ///
    /// After the first surface commit, every popup should have a parent set.
    /// This function puts it at the correct location in the window tree
    /// below its parent.
    ///
    /// Popups will be dismissed if a surface commit is made for them without
    /// any parent set. They will also be dismissed if the parent is not
    /// Handle orphan popup surface commits.
    pub fn orphan_surface_commit(&mut self, root_surface: &WlSurface) -> Option<()> {
        let mut orphans = self.orphan_popups.iter();
        let index = orphans.position(|popup| popup.surface() == *root_surface)?;
        let mut popup = self.orphan_popups.swap_remove(index);
        let parent = popup.parent()?;

        // Try and add it to the primary window.
        let active_layout = self.layouts.active();
        if let Some(primary) = active_layout.primary().as_ref() {
            popup = primary.borrow_mut().add_popup(popup, &parent)?;
        }

        // Try and add it to the secondary window.
        if let Some(secondary) = active_layout.secondary().as_ref() {
            popup = secondary.borrow_mut().add_popup(popup, &parent)?;
        }

        // Try and add it to any layer shell windows.
        for window in self.layers.iter_mut() {
            popup = window.add_popup(popup, &parent)?;
        }

        // Dismiss popup if it wasn't added to either of the visible windows.
        popup.surface.send_popup_done();

        Some(())
    }

    /// Import pending buffers for all windows.
    #[cfg_attr(feature = "profiling", profiling::function)]
    pub fn import_buffers(&mut self, renderer: &mut GlesRenderer) {
        // Import XDG windows/popups.
        for mut window in self.layouts.windows_mut() {
            window.import_buffers(renderer);
        }

        // Import layer shell windows.
        for window in self.layers.iter_mut() {
            window.import_buffers(renderer);
        }

        // Import session lock surface.
        if let View::Lock(Some(window)) = &mut self.view {
            window.import_buffers(renderer);
        }
    }

    /// Get all textures for rendering.
    #[cfg_attr(feature = "profiling", profiling::function)]
    pub fn textures(
        &mut self,
        renderer: &mut GlesRenderer,
        graphics: &mut Graphics,
        cursor_position: Option<Point<f64, Logical>>,
    ) -> &[CatacombElement] {
        // Clear global damage.
        self.dirty = false;

        let scale = self.output.scale();
        self.textures.clear();

        // Draw gesture handle when not in fullscreen/lock view.
        if !matches!(self.view, View::Fullscreen(_) | View::Lock(_)) {
            /*
            // Get texture for gesture handle.
            let gesture_handle = graphics.gesture_handle(renderer, &self.canvas, self.ime_override);

            // Calculate gesture handle bounds.
            let mut bounds = gesture_handle.geometry(scale.into());

            // Calculate position for gesture handle.
            let output_height = self.canvas.physical_size().h;
            let handle_location = (0, output_height - bounds.size.h);
            bounds.loc = handle_location.into();

            CatacombElement::add_element(
                &mut self.textures,
                gesture_handle,
                handle_location,
                bounds,
                None,
                scale,
            );
            */
        }

        // Render touch location cursor.
        if let Some(cursor_position) = cursor_position {
            let cursor = graphics.cursor(renderer, &self.canvas);

            // Center texture around touch position.
            let mut cursor_position = cursor_position.to_physical(scale).to_i32_round();
            let mut bounds = cursor.geometry(scale.into());
            cursor_position.x -= bounds.size.w / 2;
            cursor_position.y -= bounds.size.h / 2;
            bounds.loc = cursor_position;

            CatacombElement::add_element(
                &mut self.textures,
                cursor,
                cursor_position,
                bounds,
                None,
                scale,
            );
        }

        match &mut self.view {
            View::Workspace => {
                for layer in self.layers.foreground() {
                    layer.textures(&mut self.textures, scale, None, None);
                }

                self.layouts.textures(&mut self.textures, scale);

                // If active layout is overlay, render underlay behind it
                let is_overlay = self.layouts.active().primary().map(|w| w.borrow().title().unwrap_or_default() == "JollyPad-Overlay").unwrap_or(false);
                if is_overlay {
                    self.layouts.textures_underlay(&mut self.textures, scale);
                }

                for layer in self.layers.background() {
                    layer.textures(&mut self.textures, scale, None, None);
                }
            },
            View::DragAndDrop(dnd) => {
                dnd.textures(&mut self.textures, &self.canvas, graphics);

                self.layouts.textures(&mut self.textures, scale);

                for layer in self.layers.background() {
                    layer.textures(&mut self.textures, scale, None, None);
                }
            },
            View::Overview(overview) => {
                overview.textures(
                    &mut self.textures,
                    &self.output,
                    &self.canvas,
                    &self.layouts,
                    graphics,
                );

                for layer in self.layers.background() {
                    layer.textures(&mut self.textures, scale, None, None);
                }
            },
            View::Fullscreen(window) => {
                for layer in self.layers.overlay() {
                    layer.textures(&mut self.textures, scale, None, None);
                }

                window.borrow().textures(&mut self.textures, scale, None, None);
                
                // If fullscreen window is overlay, render underlay behind it
                let is_overlay = window.borrow().title().unwrap_or_default() == "JollyPad-Overlay";
                if is_overlay {
                    self.layouts.textures_underlay(&mut self.textures, scale);
                }
            },
            View::Lock(Some(window)) => window.textures(&mut self.textures, scale, None, None),
            View::Lock(None) => (),
        }

        self.textures.as_slice()
    }

    /// Request new frames for all visible windows.
    pub fn request_frames(&mut self) {
        let runtime = self.runtime();

        match &self.view {
            View::Fullscreen(window) => {
                for overlay in self.layers.overlay() {
                    overlay.request_frame(runtime);
                }
                window.borrow().request_frame(runtime);
            },
            View::Overview(_) | View::DragAndDrop(_) => {
                for window in self.layers.background() {
                    window.request_frame(runtime);
                }
            },
            View::Workspace => {
                self.layers.request_frames(runtime);
                self.layouts.with_visible(|window| window.request_frame(runtime));
            },
            View::Lock(Some(window)) => window.request_frame(runtime),
            View::Lock(None) => (),
        }
    }

    /// Update pending state (suspend/resume).
    pub fn update_pending_state(&mut self) {
        let now = Instant::now();
        
        // Handle pending suspend
        if let Some((pgid, time)) = self.pending_suspend {
            if now >= time {
                info!("Executing delayed suspend for pgid {}", pgid);
                unsafe { libc::kill(-pgid, libc::SIGSTOP) };
                self.pending_suspend = None;
            }
        }
        
        // Handle pending resume
        if let Some((pgid, time, app_id)) = self.pending_resume.clone() {
             if now >= time {
                 info!("Executing delayed resume for pgid {}", pgid);
                 unsafe { libc::kill(-pgid, libc::SIGCONT) };
                 self.cork_app_audio(app_id.as_deref(), false);
                 self.control_media(app_id.as_deref(), false);
                 self.pending_resume = None;
             }
        }
    }

    fn control_media(&self, app_id: Option<&str>, pause: bool) {
        let app_id = match app_id {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return,
        };
        
        let action = if pause { "pause" } else { "play" };
        
        // Spawn a background task to handle media control to avoid blocking compositor
        std::thread::spawn(move || {
            // Specific handling for known apps
            if app_id.contains("jellyfin") {
                 let _ = Command::new("playerctl")
                    .arg("-p")
                    .arg("jellyfinmediaplayer")
                    .arg(action)
                    .status();
            }
            
            // Generic fallback: match player name
            if let Ok(output) = Command::new("playerctl").arg("-l").output() {
                 let stdout = String::from_utf8_lossy(&output.stdout);
                 for player in stdout.lines() {
                     let p_norm = player.to_lowercase();
                     let a_norm = app_id.to_lowercase();
                     if a_norm.contains(&p_norm) || p_norm.contains(&a_norm) {
                         let _ = Command::new("playerctl")
                            .arg("-p")
                            .arg(player)
                            .arg(action)
                            .status();
                     }
                 }
            }
        });
    }

    fn cork_app_audio(&self, app_id: Option<&str>, cork: bool) {
        let app_id = match app_id {
            Some(id) if !id.is_empty() => id,
            _ => return,
        };

        let output = Command::new("pactl")
            .arg("list")
            .arg("sink-inputs")
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut current_id: Option<String> = None;
        let mut targets: Vec<String> = Vec::new();

        for line in stdout.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("Sink Input #") {
                let num = rest.trim().split_whitespace().next().unwrap_or("");
                if num.is_empty() {
                    current_id = None;
                } else {
                    current_id = Some(num.to_string());
                }
                continue;
            }

            if trimmed.starts_with("pipewire.access.portal.app_id =") && trimmed.contains(app_id) {
                if let Some(id) = current_id.clone() {
                    targets.push(id);
                }
            }
        }

        if targets.is_empty() {
            return;
        }

        let mute_value = if cork { "1" } else { "0" };

        for id in targets {
            let _ = Command::new("pactl")
                .arg("set-sink-input-mute")
                .arg(&id)
                .arg(mute_value)
                .output();
        }
    }

    /// Mark all rendered clients as presented for `wp_presentation`.
    pub fn mark_presented(
        &mut self,
        states: &RenderElementStates,
        metadata: &Option<DrmEventMetadata>,
    ) {
        // Update XDG client presentation time.
        for mut window in self.layouts.windows_mut() {
            window.mark_presented(states, metadata, &self.output, &self.start_time);
        }

        // Update layer-shell client presentation time.
        for layer in self.layers.iter_mut() {
            layer.mark_presented(states, metadata, &self.output, &self.start_time);
        }
    }

    /// Stage dead XDG shell window for reaping.
    pub fn reap_xdg(&mut self, surface: &ToplevelSurface) {
        // Remove fullscreen if this client was fullscreened.
        self.unfullscreen(surface);

        // Reap layout windows.
        self.layouts.reap(&self.output, &ShellSurface::Xdg(surface.clone()));
    }

    /// Stage dead layer shell window for reaping.
    pub fn reap_layer(&mut self, surface: &LayerSurface) {
        // Start transaction to ensure window is reaped even without any resize.
        start_transaction();

        // Clear focus if this layer was focused
        if let Some((focus_wl, _)) = &self.layers.focus {
            if focus_wl == surface.wl_surface() {
                self.layers.focus = None;
            }
        }

        // Handle layer shell death.
        let old_exclusive = *self.output.exclusive();
        if let Some(window) = self.layers.iter().find(|layer| layer.surface.eq(surface)) {
            self.output.exclusive().reset(window.surface.anchor, window.surface.exclusive_zone);
        }

        // Resize windows if reserved layer space changed.
        if self.output.exclusive() != &old_exclusive {
            self.resize_visible();
        }
    }

    /// Reap dead XDG popup windows.
    pub fn refresh_popups(&mut self) {
        for mut window in self.layouts.windows_mut() {
            window.refresh_popups();
        }
    }

    /// Start Overview window Drag & Drop.
    pub fn start_dnd(&mut self, layout_position: LayoutPosition) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            _ => return,
        };

        // Convert layout position to window.
        let window = match self.layouts.window_at(layout_position) {
            Some(window) => window.clone(),
            None => return,
        };

        let dnd = DragAndDrop::new(&self.output, overview, layout_position, window);
        self.set_view(View::DragAndDrop(dnd));
    }

    /// Fullscreen the supplied XDG surface.
    pub fn fullscreen(&mut self, surface: &ToplevelSurface) {
        if let Some(window) = self.layouts.find_window(&surface.surface()) {
            // Update window's XDG state.
            window.borrow_mut().surface.set_fullscreen(true);

            // Resize windows and change view.
            self.set_view(View::Fullscreen(window.clone()));
            self.resize_visible();
        }
    }

    /// Switch from fullscreen back to workspace view.
    ///
    /// If a surface is supplied, the view will not be changed unless the
    /// supplied surface matches the current fullscreen surface.
    pub fn unfullscreen<'a>(&mut self, surface: impl Into<Option<&'a ToplevelSurface>>) {
        let surface = surface.into();

        let window = match &self.view {
            View::Fullscreen(window) => window.borrow_mut(),
            _ => return,
        };

        // We can't easily check equality if surface is ToplevelSurface and window.surface is ShellSurface.
        // We need to extract ToplevelSurface from ShellSurface or compare WlSurface.
        let match_found = match surface {
            Some(s) => window.surface.maybe_surface().is_some_and(|wl| wl == s.surface()),
            None => true,
        };

        if match_found {
            // Update window's XDG state.
            window.surface.set_fullscreen(false);
            drop(window);

            // Resize windows and change view.
            self.set_view(View::Workspace);
            self.resize_visible();
        }
    }

    /// Current window focus.
    pub fn focus(&mut self) -> Option<(WlSurface, Option<String>)> {
        // Always focus session lock surfaces.
        if let View::Lock(window) = &self.view {
            return window.as_ref().map(|window| (window.surface().clone(), window.app_id.clone()));
        }

        // If a layer is focused, the toplevel stack is effectively unfocused for activation purposes
        let layer_focused = self.layers.focus.is_some();

        let focused = if layer_focused {
            None
        } else {
            match self.layouts.focus.as_ref().map(Weak::upgrade) {
                // Use focused surface if the window is still alive.
                Some(Some(window)) => {
                    // Clear urgency.
                    let mut window = window.borrow_mut();
                    window.urgent = false;

                    Some((window.surface.clone(), window.app_id.clone(), window.title()))
                },
                // Fallback to primary if secondary perished.
                Some(None) => {
                    let active_layout = self.layouts.pending_active();
                    let primary = active_layout.primary();
                    let focused = primary.map(|window| {
                        // Clear urgency.
                        let mut window = window.borrow_mut();
                        window.urgent = false;

                        (window.surface.clone(), window.app_id.clone(), window.title())
                    });
                    self.layouts.focus = primary.map(Rc::downgrade);
                    focused
                },
                // Do not upgrade if toplevel is explicitly unfocused.
                None => None,
            }
        };

        // Update window activation state.
        let new_surface = focused.as_ref().map(|(surface, _, _)| surface);
        if self.activated.as_ref() != new_surface {
            // Clear old activated flag.
            let old_activated = self.activated.take();
            if let Some(activated) = old_activated.as_ref() {
                activated.set_state(|_state| {
                    activated.set_activated(false);
                });

                // Suspend old process group
                if let Some(window) = self.layouts.windows().find(|w| w.surface == *activated) {
                    if let Some(pgid) = window.process_group {
                        if pgid > 0 {
                            let app_id = window.app_id.as_deref();
                            
                            // 1. Mute Audio
                            self.cork_app_audio(app_id, true);
                            
                            // 2. Pause Media
                            self.control_media(app_id, true);
                            
                            // 3. Schedule SIGSTOP (Delay 100ms)
                            self.pending_suspend = Some((pgid, Instant::now() + Duration::from_millis(100)));
                            info!("Scheduled suspend for pgid {} (100ms delay)", pgid);
                        }
                    }
                }
            }

            // Capture old role before updating
            let old_role = self.active_role.clone();
            // Reset active role, will be updated below if applicable
            self.active_role = None;

            // Set new activated flag.
            if let Some((surface, app_id_opt, title_opt)) = &focused {
                // Determine new role
                for (role, matcher) in &self.system_roles {
                     let app_id_match = app_id_opt.as_ref().map_or(false, |id| matcher.matches(Some(id)));
                     let title_match = title_opt.as_ref().map_or(false, |t| matcher.matches(Some(t)));
                     
                     if app_id_match || title_match {
                        self.active_role = Some(role.clone());
                        break;
                     }
                }

                surface.set_state(|_state| {
                    surface.set_activated(true);
                });

                // Always cancel any pending resume when switching to a new window
                self.pending_resume = None;

                // Resume new process group
                if let Some(window_rc) = self.layouts.focus.as_ref().and_then(|w| w.upgrade()) {
                    let window = window_rc.borrow();
                    if let Some(pgid) = window.process_group {
                        if pgid > 0 {
                            // Check if we should delay resume (e.g. coming from Nav/Home)
                            let mut delay = false;
                            
                            // Check cached old role (handles destroyed surfaces)
                            if let Some(role) = &old_role {
                                if role == "nav" || role == "home" {
                                    // If we are switching TO a system role (like Home), DO NOT DELAY.
                                    if let Some(new_role) = &self.active_role {
                                        if new_role == "home" {
                                            delay = false;
                                        } else {
                                            delay = true;
                                        }
                                    } else {
                                        delay = true;
                                    }
                                }
                            }
                            
                            // Fallback: check if previous surface was a system role layer (if still alive)
                            if !delay {
                                if let Some(prev) = old_activated.as_ref() {
                                    if let Some(prev_wl) = prev.maybe_surface() {
                                        if self.layers.iter().any(|l| {
                                            l.surface.maybe_surface().as_ref() == Some(&prev_wl) &&
                                            l.app_id.as_ref().map_or(false, |id| {
                                                self.system_roles.get("nav").map_or(false, |m| m.matches(Some(id))) ||
                                                self.system_roles.get("home").map_or(false, |m| m.matches(Some(id)))
                                            })
                                        }) {
                                            delay = true;
                                        }
                                    }
                                }
                            }

                            info!(
                                "Focus switch: OldRole={:?} NewRole={:?} Delay={}", 
                                old_role, self.active_role, delay
                            );

                            if delay {
                                let app_id = window.app_id.clone();
                                self.pending_resume = Some((pgid, Instant::now() + Duration::from_millis(100), app_id));
                                info!("Delayed resume for pgid {} (transient system role)", pgid);
                            } else {
                                // Cancel pending suspend
                                if let Some((p, _)) = self.pending_suspend {
                                    if p == pgid {
                                        self.pending_suspend = None;
                                        info!("Cancelled pending suspend for pgid {}", pgid);
                                    }
                                }
                                
                                unsafe { libc::kill(-pgid, libc::SIGCONT) };
                                info!("Resumed process group {}", pgid);

                                let app_id = window.app_id.as_deref();
                                self.cork_app_audio(app_id, false);
                                self.control_media(app_id, false);
                            }
                        }
                    }
                }
            } else if let Some((focus_wl, _)) = &self.layers.focus {
                // Toplevel focus lost, but Layer is focused. Capture its role.
                if let Some(layer) = self.layers.iter().find(|l| l.surface.surface() == *focus_wl) {
                      let title = layer.title();
                      let app_id = layer.xdg_app_id().or(layer.app_id.clone());
                      
                      for (role, matcher) in &self.system_roles {
                           if matcher.matches(app_id.as_ref()) || (title.is_some() && matcher.matches(title.as_ref())) {
                                self.active_role = Some(role.clone());
                                info!("Layer focus active_role set to {}", role);
                                break;
                           }
                      }
                 }
            }
            self.activated = new_surface.cloned();
        }

        focused.and_then(|(surface, app_id, _)| surface.maybe_surface().map(|wl| (wl, app_id)))
            // Check for layer-shell window focus.
            .or_else(|| self.layers.focus.clone())
    }

    /// Update the focused window.
    pub fn set_focus(
        &mut self,
        layout: Option<Weak<RefCell<Window>>>,
        layer: Option<WlSurface>,
        app_id: Option<String>,
    ) {
        self.layouts.focus = layout;
        self.layers.focus = layer.map(|l| (l, app_id));
    }

    /// Start a new transaction.
    fn start_transaction(&mut self) -> &mut Transaction {
        start_transaction();
        self.transaction.get_or_insert(Transaction::new())
    }

    /// Attempt to execute pending transactions.
    ///
    /// This will return the duration until the transaction should be timed out
    /// when there is an active transaction but it cannot be completed yet.
    pub fn update_transaction(&mut self) -> Option<Duration> {
        if let Some((pgid, time, app_id)) = self.pending_resume.as_ref() {
            let now = Instant::now();
            if now >= *time {
                unsafe { libc::kill(-pgid, libc::SIGCONT) };
                info!("Executed delayed resume for pgid {}", pgid);
                let app_id_ref = app_id.as_deref();
                self.cork_app_audio(app_id_ref, false);
                self.pending_resume = None;
            }
        }

        let start = TRANSACTION_START.load(Ordering::Relaxed);
        if start == 0 {
            // Even without an active transaction, apply liveliness changes to reap dead windows.
            let old_layout_count = self.layouts.active().window_count();
            let old_layer_count = self.layers.len();

            self.dirty |= self.layouts.apply_transaction(&self.output);
            self.layers.apply_transaction();

            if let View::Lock(Some(window)) = &mut self.view {
                window.apply_transaction();
            }

            let old_canvas = mem::replace(&mut self.canvas, *self.output.canvas());
            self.dirty |= old_canvas.orientation() != self.canvas.orientation()
                || old_canvas.scale() != self.canvas.scale();

            if self.layouts.is_empty()
                && matches!(self.view, View::Overview(_) | View::DragAndDrop(_) | View::Fullscreen(_))
            {
                self.view = View::Workspace;
            }

            self.fix_view_after_reap();

            self.dirty |= old_layout_count != self.layouts.active().window_count()
                || old_layer_count != self.layers.len();

            if self.dirty { }

            if let Some((_, time, _)) = self.pending_resume {
                let now = Instant::now();
                if time > now {
                    return Some(time - now);
                }
            }

            return None;
        }

        // Check if the transaction requires updating.
        let elapsed = UNIX_EPOCH.elapsed().unwrap().as_millis() as u64 - start;
        let timeout = self.transaction_timeout();
        if elapsed <= timeout {
            let lock_ready = match self.pending_view() {
                // Check if lock surface transaction is done.
                View::Lock(Some(window)) => window.transaction_done(),
                // Wait for timeout to allow surface creation.
                View::Lock(None) => false,
                _ => true,
            };

            // Check if all participants are ready.
            let finished = lock_ready
                && self.layouts.windows().all(|window| window.transaction_done())
                && self.layers.iter().all(Window::transaction_done);

            // Abort if the transaction is still pending.
            if !finished {
                let delta = timeout - elapsed;
                let t_rem = Duration::from_millis(delta);
                if let Some((_, time, _)) = self.pending_resume {
                    let now = Instant::now();
                    if time > now {
                        return Some(cmp::min(t_rem, time - now));
                    }
                }
                return Some(t_rem);
            }
        } else {
            // Blacklist windows which caused transaction failure.
            for mut window in self.layouts.windows_mut().filter(|w| !w.transaction_done()) {
                window.set_ignore_transactions();
            }
            for window in self.layers.iter_mut().filter(|w| !w.transaction_done()) {
                window.set_ignore_transactions();
            }
        }

        let old_view_name = match &self.view {
            View::Workspace => "Workspace",
            View::Overview(_) => "Overview",
            View::DragAndDrop(_) => "DragAndDrop",
            View::Fullscreen(_) => "Fullscreen",
            View::Lock(_) => "Lock",
        };
        // Clear transaction timer.
        TRANSACTION_START.store(0, Ordering::Relaxed);

        // Store old visible window count to see if we need to redraw.
        let old_layout_count = self.layouts.active().window_count();
        let old_layer_count = self.layers.len();

        // Switch active view.
        if let Some(view) = self.transaction.take().and_then(|transaction| transaction.view) {
            self.dirty = true;
            self.view = view;
            if self.trace_scene_enabled {
                let new_view_name = match &self.view {
                    View::Workspace => "Workspace",
                    View::Overview(_) => "Overview",
                    View::DragAndDrop(_) => "DragAndDrop",
                    View::Fullscreen(_) => "Fullscreen",
                    View::Lock(_) => "Lock",
                };
                let now_ms = UNIX_EPOCH.elapsed().unwrap().as_millis() as u64;
                let delta_ms = now_ms.saturating_sub(start);
                info!("SceneSwitch: {} -> {} elapsed={}ms", old_view_name, new_view_name, delta_ms);
                self.log_scene_stack();
            }
        }

        // Apply layout/liveliness changes.
        self.dirty |= self.layouts.apply_transaction(&self.output);

        // Update layer shell windows.
        self.layers.apply_transaction();

        // Update session lock window.
        if let View::Lock(Some(window)) = &mut self.view {
            window.apply_transaction();
        }

        // Ensure view is consistent after reaping (e.g. fullscreen app exited).
        self.fix_view_after_reap();

        // Update canvas and force redraw on orientation change.
        let old_canvas = mem::replace(&mut self.canvas, *self.output.canvas());
        self.dirty |= old_canvas.orientation() != self.canvas.orientation()
            || old_canvas.scale() != self.canvas.scale();

        // Close overview if all layouts died.
        if self.layouts.is_empty()
            && matches!(self.view, View::Overview(_) | View::DragAndDrop(_) | View::Fullscreen(_))
        {
            self.view = View::Workspace;
        }

        // Redraw if a visible window has died.
        self.dirty |= old_layout_count != self.layouts.active().window_count()
            || old_layer_count != self.layers.len();

        if self.dirty { }

        None
    }

    fn fix_view_after_reap(&mut self) {
        if let View::Fullscreen(window) = &self.view {
            let (alive, surface_opt) = {
                let window_ref = window.borrow();
                (window_ref.alive(), window_ref.surface.maybe_surface())
            };

            if !alive {
                self.view = View::Workspace;
                let parent = self.layouts.parent_layouts.last().copied();
                if let Some(parent_id) = parent {
                    if let Some(index) = self.layouts.layouts().iter().position(|l| l.id == parent_id) {
                        self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
                    } else {
                        let role_home = self.system_roles.get("home");
                        let desktop_index = self.layouts.layouts().iter().position(|l| {
                            if let Some(p) = l.primary() {
                                let w = p.borrow();
                                let t = w.title().unwrap_or_default();
                                let xdg_id = w.xdg_app_id().unwrap_or_default();
                                let app_id = w.app_id.clone().unwrap_or_default();
                                if let Some(home_matcher) = role_home {
                                    home_matcher.matches(Some(&app_id)) || home_matcher.matches(Some(&t)) || home_matcher.matches(Some(&xdg_id))
                                } else {
                                    t == "JollyPad-Desktop" || xdg_id == "jolly-home" || app_id == "jolly-home"
                                }
                            } else {
                                false
                            }
                        });
                        if let Some(index) = desktop_index {
                            self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
                        } else if !self.layouts.is_empty() {
                            self.layouts.set_active(&self.output, Some(LayoutPosition::new(0, false)), true);
                        } else {
                            self.layouts.set_active(&self.output, None, true);
                        }
                    }
                } else {
                    let role_home = self.system_roles.get("home");
                    let desktop_index = self.layouts.layouts().iter().position(|l| {
                        if let Some(p) = l.primary() {
                            let w = p.borrow();
                            let t = w.title().unwrap_or_default();
                            let xdg_id = w.xdg_app_id().unwrap_or_default();
                            let app_id = w.app_id.clone().unwrap_or_default();
                            if let Some(home_matcher) = role_home {
                                home_matcher.matches(Some(&app_id)) || home_matcher.matches(Some(&t)) || home_matcher.matches(Some(&xdg_id))
                            } else {
                                t == "JollyPad-Desktop" || xdg_id == "jolly-home" || app_id == "jolly-home"
                            }
                        } else {
                            false
                        }
                    });
                    if let Some(index) = desktop_index {
                        self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
                    } else if !self.layouts.is_empty() {
                        self.layouts.set_active(&self.output, Some(LayoutPosition::new(0, false)), true);
                    } else {
                        self.layouts.set_active(&self.output, None, true);
                    }
                }
                return;
            }

            let still_present = surface_opt.map(|surf| {
                self.layouts
                    .windows()
                    .any(|w| w.surface.maybe_surface() == Some(surf.clone()))
            }).unwrap_or(false);

            if !still_present {
                self.view = View::Workspace;
                let parent = self.layouts.parent_layouts.last().copied();
                if let Some(parent_id) = parent {
                    if let Some(index) = self.layouts.layouts().iter().position(|l| l.id == parent_id) {
                        self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
                    } else {
                        let role_home = self.system_roles.get("home");
                        let desktop_index = self.layouts.layouts().iter().position(|l| {
                            if let Some(p) = l.primary() {
                                let w = p.borrow();
                                let t = w.title().unwrap_or_default();
                                let xdg_id = w.xdg_app_id().unwrap_or_default();
                                let app_id = w.app_id.clone().unwrap_or_default();
                                if let Some(home_matcher) = role_home {
                                    home_matcher.matches(Some(&app_id)) || home_matcher.matches(Some(&t)) || home_matcher.matches(Some(&xdg_id))
                                } else {
                                    t == "JollyPad-Desktop" || xdg_id == "jolly-home" || app_id == "jolly-home"
                                }
                            } else {
                                false
                            }
                        });
                        if let Some(index) = desktop_index {
                            self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
                        } else if !self.layouts.is_empty() {
                            self.layouts.set_active(&self.output, Some(LayoutPosition::new(0, false)), true);
                        } else {
                            self.layouts.set_active(&self.output, None, true);
                        }
                    }
                } else {
                    let role_home = self.system_roles.get("home");
                    let desktop_index = self.layouts.layouts().iter().position(|l| {
                        if let Some(p) = l.primary() {
                            let w = p.borrow();
                            let t = w.title().unwrap_or_default();
                            let xdg_id = w.xdg_app_id().unwrap_or_default();
                            let app_id = w.app_id.clone().unwrap_or_default();
                            if let Some(home_matcher) = role_home {
                                home_matcher.matches(Some(&app_id)) || home_matcher.matches(Some(&t)) || home_matcher.matches(Some(&xdg_id))
                            } else {
                                t == "JollyPad-Desktop" || xdg_id == "jolly-home" || app_id == "jolly-home"
                            }
                        } else {
                            false
                        }
                    });
                    if let Some(index) = desktop_index {
                        self.layouts.set_active(&self.output, Some(LayoutPosition::new(index, false)), true);
                    } else if !self.layouts.is_empty() {
                        self.layouts.set_active(&self.output, Some(LayoutPosition::new(0, false)), true);
                    } else {
                        self.layouts.set_active(&self.output, None, true);
                    }
                }
            }
        }
    }

    /// Get timeout before transactions will be forcefully applied.
    fn transaction_timeout(&self) -> u64 {
        match self.pending_view() {
            // Enforce a tighter transaction timing for session locking.
            View::Lock(_) => MAX_LOCKED_TRANSACTION_MILLIS,
            _ => MAX_TRANSACTION_MILLIS,
        }
    }

    /// Resize all windows to their expected size.
    pub fn resize_all(&mut self) {
        let available_fullscreen = self.output.available_fullscreen();
        let output_size = self.output.size();
        let scale = self.output.scale();

        // Handle fullscreen/lock surfaces.
        let fullscreen_window = match self.pending_view_mut() {
            View::Fullscreen(window) => {
                let mut window = window.borrow_mut();
                window.set_dimensions(scale, available_fullscreen);

                Some(window.surface().clone())
            },
            View::Lock(Some(window)) => {
                let rect = Rectangle::from_size(output_size);
                window.set_dimensions(scale, rect);

                None
            },
            _ => None,
        };
        let fullscreen_window = fullscreen_window.as_ref();

        // Resize XDG clients.
        for layout in self.layouts.layouts() {
            // Skip resizing fullscreened layout.
            if layout.windows().any(|window| Some(window.borrow().surface()) == fullscreen_window.cloned()) {
                continue;
            }

            layout.resize(&self.output);
        }

        // Resize layer shell clients.
        for window in self.layers.iter_mut() {
            let fullscreened =
                window.surface.layer() == Layer::Overlay && fullscreen_window.is_some();
            window.update_dimensions(&mut self.output, fullscreened);
        }
    }

    /// Resize visible windows to their expected size.
    pub fn resize_visible(&mut self) {
        let available_fullscreen = self.output.available_fullscreen();
        let output_scale = self.output.scale();
        let output_size = self.output.size();

        match self.pending_view_mut() {
            // Resize fullscreen and overlay surfaces in fullscreen view.
            View::Fullscreen(window) => {
                // Resize fullscreen XDG client.
                window.borrow_mut().set_dimensions(output_scale, available_fullscreen);

                // Resize overlay layer clients.
                for window in self.layers.overlay_mut() {
                    window.update_dimensions(&mut self.output, true);
                }
            },
            View::Lock(Some(window)) => {
                let rect = Rectangle::from_size(output_size);
                window.set_dimensions(output_scale, rect);
            },
            // Resize active XDG layout and layer shell in any other view.
            _ => {
                // Resize XDG windows.
                self.layouts.pending_active().resize(&self.output);

                // Resize layer shell windows.
                for window in self.layers.iter_mut() {
                    window.update_dimensions(&mut self.output, false);
                }
            },
        }
    }

    /// Set output orientation, completely bypassing locking logic.
    fn set_orientation(&mut self, orientation: Orientation) {
        // Start transaction to ensure output transaction will be applied.
        start_transaction();

        // Update output orientation.
        self.output.set_orientation(orientation);

        // Update window transform.
        let transform = orientation.surface_transform();
        with_all_windows!(self, |window| window.update_transform(transform));

        // Resize all windows to new output size.
        self.resize_all();
    }

    /// Update output orientation.
    pub fn update_orientation(&mut self, orientation: Orientation) {
        self.unlocked_orientation = orientation;

        // Ignore orientation changes during orientation lock.
        if self.orientation_locked {
            return;
        }

        self.set_orientation(orientation);
    }

    /// Lock the output's orientation.
    pub fn lock_orientation(&mut self, orientation: Option<Orientation>) {
        // Change to the new locked orientation.
        if let Some(orientation) = orientation {
            self.set_orientation(orientation);
        }

        self.orientation_locked = true;
    }

    /// Unlock the output's orientation.
    pub fn unlock_orientation(&mut self) {
        self.orientation_locked = false;
        self.update_orientation(self.unlocked_orientation);
    }

    /// Get orientation lock state.
    pub fn orientation_locked(&self) -> bool {
        self.orientation_locked
    }

    /// Check if any window was damaged since the last redraw.
    pub fn damaged(&mut self) -> bool {
        if self.dirty {
            return true;
        }

        match &self.view {
            View::Overview(overview) if overview.dirty() => true,
            View::Workspace | View::Overview(_) => {
                self.layouts.windows().any(|window| window.dirty())
                    || self.layers.iter().any(Window::dirty)
            },
            View::Fullscreen(window) => {
                window.borrow().dirty() || self.layers.overlay().any(Window::dirty)
            },
            View::Lock(window) => window.as_ref().is_some_and(|window| window.dirty()),
            View::DragAndDrop(_) => false,
        }
    }

    /// Handle start of touch input.
    pub fn on_touch_start(&mut self, point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            _ => return,
        };

        // Hold on overview window stages it for D&D.
        if let Some(position) = overview.layout_position(&self.output, &self.layouts, point) {
            overview.start_hold(&self.event_loop, position);
        }

        overview.drag_action = Default::default();
        overview.last_drag_point = point;
        overview.y_offset = 0.;
    }

    /// Hand quick touch input.
    pub fn on_tap(&mut self, point: Point<f64, Logical>, toggle_ime: &mut bool) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            // Handle IME override toggle on gesture handle tap.
            View::Workspace => {
                *toggle_ime = point.y >= (self.canvas.size().h - GESTURE_HANDLE_HEIGHT) as f64
                    && self.focus().is_none();
                return;
            },
            View::DragAndDrop(_) | View::Fullscreen(_) | View::Lock(_) => return,
        };

        overview.cancel_hold(&self.event_loop);

        // Click inside window opens it as new primary.
        if let Some(position) = overview.layout_position(&self.output, &self.layouts, point) {
            self.layouts.set_active(&self.output, Some(position), true);
        }

        // Return to workspace view.
        //
        // If the click was outside of the focused window, we just close out of the
        // Overview and return to the previous primary/secondary windows.
        self.set_view(View::Workspace);
    }

    /// Hand quick double-touch input.
    pub fn on_double_tap(&mut self, point: Point<f64, Logical>) {
        // Ensure we're in workspace view.
        if !matches!(self.view, View::Workspace) {
            return;
        }

        // Ignore tap outside of gesture handle.
        let canvas_size = self.canvas.size().to_f64();
        if point.y < (canvas_size.h - GESTURE_HANDLE_HEIGHT as f64) {
            return;
        }

        if point.x >= canvas_size.w / 1.5 {
            self.layouts.cycle_active(&self.output, 1);
        } else if point.x < canvas_size.w / 3. {
            self.layouts.cycle_active(&self.output, -1);
        }
    }

    /// Handle a touch drag.
    pub fn on_drag(&mut self, touch_state: &mut TouchState, mut point: Point<f64, Logical>) {
        let overview = match &mut self.view {
            View::Overview(overview) => overview,
            View::DragAndDrop(dnd) => {
                // Cancel velocity and clamp if touch position is outside the screen.
                let output_size = self.output.wm_size().to_f64();
                if point.x < 0.
                    || point.x > output_size.w
                    || point.y < 0.
                    || point.y > output_size.h
                {
                    point.x = point.x.clamp(0., output_size.w - 1.);
                    point.y = point.y.clamp(0., output_size.h - 1.);
                    touch_state.cancel_velocity();
                }

                let delta = point - mem::replace(&mut dnd.touch_position, point);
                dnd.window_position += delta;

                // Redraw when the D&D window is moved.
                self.dirty = true;

                return;
            },
            View::Fullscreen(_) | View::Lock(_) | View::Workspace => return,
        };

        let delta = point - mem::replace(&mut overview.last_drag_point, point);

        // Lock current drag direction if it hasn't been determined yet.
        if matches!(overview.drag_action.action_type, DragActionType::None) {
            if delta.x.abs() < delta.y.abs() {
                overview.drag_action = overview
                    .layout_position(&self.output, &self.layouts, point)
                    .and_then(|position| self.layouts.window_at(position))
                    .map(|window| DragActionType::Close(Rc::downgrade(window)).into())
                    .unwrap_or_default();
            } else {
                overview.drag_action = DragActionType::Cycle.into();
            }
        }

        // Update drag action.
        match overview.drag_action.action_type {
            DragActionType::Cycle => {
                let sensitivity = self.output.physical_size().w as f64 * 0.4;
                overview.x_offset += delta.x / sensitivity;
            },
            DragActionType::Close(_) => overview.y_offset += delta.y,
            DragActionType::None => (),
        }

        // Cancel velocity once drag actions are completed.
        if overview.cycle_edge_reached(self.layouts.len()) {
            touch_state.cancel_velocity();
        }

        overview.cancel_hold(&self.event_loop);

        // Redraw when cycling through the overview.
        self.dirty = true;
    }

    /// Handle touch drag release.
    pub fn on_drag_release(&mut self) {
        match &mut self.view {
            View::Overview(overview) => overview.drag_action.done = true,
            View::DragAndDrop(dnd) => {
                let (primary_bounds, secondary_bounds) = dnd.drop_bounds(&self.output);
                if primary_bounds.to_f64().contains(dnd.touch_position) {
                    let surface = dnd.window.borrow().surface().clone();
                    if let Some(position) = self.layouts.position(&surface) {
                        self.layouts.set_primary(&self.output, position);
                        self.set_view(View::Workspace);
                    }
                } else if secondary_bounds.to_f64().contains(dnd.touch_position) {
                    let surface = dnd.window.borrow().surface().clone();
                    if let Some(position) = self.layouts.position(&surface) {
                        self.layouts.set_secondary(&self.output, position);
                        self.set_view(View::Workspace);
                    }
                } else {
                    let overview = Overview::new(dnd.overview_x_offset, None);
                    self.set_view(View::Overview(overview));
                }
            },
            View::Fullscreen(_) | View::Lock(_) | View::Workspace => (),
        }
    }

    /// Process in-progress handle gestures.
    pub fn on_gesture(&mut self, touch_state: &mut TouchState, gesture: HandleGesture) {
        match (gesture, &mut self.view) {
            (HandleGesture::Vertical(position), View::Overview(overview)) => {
                overview.set_open_percentage(&self.output, position);

                // Ensure we don't keep processing velocity after completion.
                if overview.gesture_threshold_passed() && !touch_state.touching() {
                    touch_state.cancel_velocity();
                    self.on_gesture_done(gesture);
                }
            },
            (HandleGesture::Vertical(position), View::Workspace) if !self.layouts.is_empty() => {
                // Ignore overview gesture until changes are required.
                let available = self.canvas.available_overview().to_f64();
                if position < available.loc.y || position >= available.loc.y + available.size.h {
                    return;
                }

                // Change view and resize windows.
                let active_empty = self.layouts.active().is_empty();
                let primary_percentage = Some(if active_empty { 0. } else { 1. });
                let overview = Overview::new(self.layouts.active_offset(), primary_percentage);
                self.set_view(View::Overview(overview));
            },
            (HandleGesture::Vertical(_) | HandleGesture::Horizontal, _) => (),
        }
    }

    /// Process completion of handle gestures.
    pub fn on_gesture_done(&mut self, gesture: HandleGesture) {
        match (gesture, &mut self.view) {
            (HandleGesture::Vertical(position), View::Overview(overview)) if overview.opened => {
                overview.set_open_percentage(&self.output, position);

                if overview.gesture_threshold_passed() {
                    // Switch back to workspace view.
                    self.layouts.set_active(&self.output, None, true);
                    self.set_view(View::Workspace);
                } else {
                    // Stay in overview.
                    let overview = Overview::new(overview.x_offset, None);
                    self.set_view(View::Overview(overview));
                }
            },
            (HandleGesture::Vertical(position), View::Overview(overview)) if !overview.opened => {
                overview.set_open_percentage(&self.output, position);

                if overview.gesture_threshold_passed() {
                    overview.opened = true;
                } else {
                    self.set_view(View::Workspace);
                }
            },
            // Leave fullscreen on "drag up".
            (HandleGesture::Vertical(position), View::Fullscreen(window)) => {
                // Require drag end to be above the gesture handle.
                let available = self.canvas.available_overview().to_f64();
                if position < available.loc.y || position >= available.loc.y + available.size.h {
                    return;
                }

                // Unset XDG fullscreen state.
                window.borrow().surface.set_state(|_state| {
                    window.borrow().surface.set_fullscreen(false);
                });

                // Change back to workspace view.
                self.set_view(View::Workspace);

                // Resize back to workspace size.
                self.resize_visible();
            },
            (HandleGesture::Vertical(_) | HandleGesture::Horizontal, _) => (),
        }
    }

    /// Check which surface is at a specific touch point.
    ///
    /// This filters out non-interactive surfaces.
    pub fn surface_at(&mut self, position: Point<f64, Logical>) -> Option<InputSurface> {
        let scale = self.canvas.scale();

        /// Focus a layer shell surface and return it.
        macro_rules! focus_layer_surface {
            ($window:expr, $surface:expr) => {{
                // Only set new focus target if focus is accepted.
                if !$window.deny_focus {
                    let wl_surface = $window.surface().clone();
                    let app_id = $window.app_id.clone();
                    $surface.toplevel = Some(InputSurfaceKind::Layer((wl_surface, app_id)));
                }
            }};
        }

        // Prevent window interaction in Overview/DnD.
        match &self.view {
            View::Workspace => (),
            View::Fullscreen(window) => {
                if let Some((window, mut surface)) = self.layers.overlay_surface_at(scale, position)
                {
                    focus_layer_surface!(window, surface);
                    return Some(surface);
                }

                // Get surface of the fullscreened window.
                let window_ref = window.borrow();
                let mut surface = window_ref.surface_at(scale, position)?;

                // Set toplevel to update focus.
                let app_id = window_ref.app_id.clone();
                let window = Rc::downgrade(window);
                surface.toplevel = Some(InputSurfaceKind::Layout((window, app_id)));

                return Some(surface);
            },
            View::Lock(Some(window)) => return window.surface_at(scale, position),
            _ => return None,
        };

        // Search for topmost clicked surface.

        if let Some((window, mut surface)) = self.layers.foreground_surface_at(scale, position) {
            focus_layer_surface!(window, surface);
            return Some(surface);
        }

        if let Some(surface) = self.layouts.touch_surface_at(scale, position) {
            return Some(surface);
        }

        if let Some((window, mut surface)) = self.layers.background_surface_at(scale, position) {
            focus_layer_surface!(window, surface);
            return Some(surface);
        }

        None
    }

    /// Add a per-window scale override.
    pub fn add_window_scale(&mut self, app_id: AppIdMatcher, scale: WindowScale) {
        // Remove previous scale assignments for this EXACT regex.
        self.window_scales.retain(|(matcher, _)| matcher.base() != app_id.base());

        // Add scale if it is transformative to the output scale.
        match scale {
            WindowScale::Additive(scale) | WindowScale::Subtractive(scale) if scale == 0. => (),
            WindowScale::Multiplicative(scale) | WindowScale::Divisive(scale) if scale == 1. => (),
            _ => self.window_scales.push((app_id, scale)),
        }

        // Update existing window scales.
        let window_scales = mem::take(&mut self.window_scales);
        let output_scale = self.output.scale();
        with_all_windows_mut!(self, |window| window.set_window_scale(&window_scales, output_scale));
        self.window_scales = window_scales;
    }

    /// Check if a surface is currently visible.
    pub fn surface_visible(&self, surface: &WlSurface) -> bool {
        match self.pending_view() {
            View::Lock(Some(window)) if window.owns_surface(surface) => return true,
            _ => (),
        }

        let mut visible_xdg = false;
        self.layouts.with_visible(|window| visible_xdg |= window.owns_surface(surface));

        visible_xdg || self.layers.iter().any(|layer| layer.owns_surface(surface))
    }

    /// Application runtime.
    pub fn runtime(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// Get transaction view, or current view if no transaction is active.
    fn pending_view(&self) -> &View {
        self.transaction.as_ref().and_then(|t| t.view.as_ref()).unwrap_or(&self.view)
    }

    /// Get transaction view, or current view if no transaction is active.
    fn pending_view_mut(&mut self) -> &mut View {
        self.transaction.as_mut().and_then(|t| t.view.as_mut()).unwrap_or(&mut self.view)
    }

    /// Change the active view.
    fn set_view(&mut self, view: View) {
        // SAFETY: Prevent accidental unlock. Use [`Self::unlock`] instead.
        if let View::Lock(_) = self.pending_view() {
            return;
        }

        match view {
            // Skip transaction when switching to overview.
            View::Overview(_) => match &mut self.transaction {
                // Clear pending view to go to overview.
                Some(transaction) => {
                    transaction.view = None;
                    self.view = view;
                },
                // Directly apply overview without pending transaction.
                None => {
                    self.dirty = true;
                    self.view = view;
                },
            },
            _ => self.start_transaction().view = Some(view),
        }
    }

    /// Update the window manager's current output.
    pub fn set_output(&mut self, output: Output) {
        self.canvas = *output.canvas();
        self.output = output;
    }

    /// Get access to the current canvas.
    ///
    /// This is different from [`Self::output`] by returning a cached output
    /// while transactions are processed.
    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    /// Update the output's scale.
    pub fn set_scale(&mut self, scale: f64) {
        self.start_transaction();
        self.output.set_scale(scale);

        // Update surfaces' preferred fractional and buffer scale.
        with_all_windows!(self, |window| window.update_scale(scale));

        self.resize_all();
    }

    /// Mark the entire screen as dirty.
    pub fn set_dirty(&mut self) {
        self.dirty = true;
    }

    /// Raise a surface's window to the foreground.
    pub fn raise(&mut self, surface: &WlSurface) {
        if let Some(layout_position) = self.layouts.position(surface) {
            self.layouts.set_active(&self.output, Some(layout_position), false);
        }
    }

    /// Mark a surface's window as urgent.
    pub fn set_urgent(&mut self, surface: &WlSurface, urgent: bool) {
        if let Some(window) = self.layouts.find_window(surface) {
            window.borrow_mut().urgent = urgent;
        }
    }

    /// Get parent geometry of the window owning a surface.
    pub fn parent_geometry(&self, surface: &WlSurface) -> Rectangle<i32, Logical> {
        with_all_windows!(self, |window| {
            if window.owns_surface(surface) {
                return window.bounds(self.output.scale());
            }
        });

        // Default to full output size.
        Rectangle::from_size(self.output.size())
    }

    /// IME force enable/disable status.
    ///
    /// This is only responsible for determining the gesture handle color, must
    /// only be used through [`Catacomb::toggle_ime_override`].
    pub fn set_ime_override(&mut self, ime_override: Option<bool>) {
        self.ime_override = ime_override;
        self.dirty = true;
    }
}

/// Atomic changes to [`Windows`].
#[derive(Debug)]
struct Transaction {
    view: Option<View>,
}

impl Transaction {
    fn new() -> Self {
        Self { view: None }
    }
}

/// Compositor window arrangements.
#[derive(Default, Debug)]
enum View {
    /// List of all open windows.
    Overview(Overview),
    /// Drag and drop for tiling windows.
    DragAndDrop(DragAndDrop),
    /// Fullscreened XDG-shell window.
    Fullscreen(Rc<RefCell<Window>>),
    /// Session lock.
    Lock(Option<Box<Window<LockSurface>>>),
    /// Currently active windows.
    #[default]
    Workspace,
}
