//! Window surfaces.

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Weak;
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::drawing::CatacombSurfaceData;
use smithay::wayland::compositor;
use smithay::wayland::session_lock::{LockSurface, LockSurfaceState};
use smithay::wayland::shell::wlr_layer::{
    Anchor, ExclusiveZone, Layer, LayerSurface, LayerSurfaceAttributes, LayerSurfaceState,
};
use smithay::wayland::shell::xdg::{
    PopupState, PopupSurface, SurfaceCachedState, ToplevelState, ToplevelSurface,
    XdgPopupSurfaceData, XdgToplevelSurfaceData, XdgToplevelSurfaceRoleAttributes,
};
use crate::xwayland::X11Surface;

use crate::windows::Window;

/// Common surface functionality.
pub trait Surface {
    /// Surface state type.
    type State;

    /// Get underlying Wayland surface.
    fn surface(&self) -> WlSurface;

    /// Get underlying Wayland surface if available.
    ///
    /// For X11 surfaces, the associated wl_surface may not be available immediately.
    /// This method allows callers to gracefully skip operations until it exists.
    fn maybe_surface(&self) -> Option<WlSurface> {
        Some(self.surface())
    }

    /// Check if the window has been closed.
    fn alive(&self) -> bool;

    /// Send the initial configure.
    fn initial_configure(&self);

    /// Check whether the initial configure was already sent.
    fn initial_configure_sent(&self) -> bool;

    /// Update surface state.
    fn set_state<F: FnMut(&mut Self::State)>(&self, f: F);

    /// Update surface's dimensions.
    fn resize(&self, size: Size<i32, Logical>);

    /// Window's acknowledged size.
    fn acked_size(&self) -> Size<i32, Logical>;

    /// Geometry of the window's visible bounds.
    fn geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let Some(surface) = self.maybe_surface() else { return None };
        compositor::with_states(&surface, |states| {
            states.cached_state.get::<SurfaceCachedState>().current().geometry
        })
    }

    /// Set fullscreen state.
    fn set_fullscreen(&self, _fullscreen: bool) {}

    /// Set activated state.
    fn set_activated(&self, _activated: bool) {}

    /// Send close request.
    fn send_close(&self) {}

    /// Get surface title.
    fn title(&self) -> Option<String> { None }

    /// Get surface App ID.
    fn app_id(&self) -> Option<String> { None }

    /// Get preferred fractional scale.
    #[allow(dead_code)]
    fn preferred_fractional_scale(&self) -> Option<f64> {
        let surface = self.maybe_surface()?;
        compositor::with_states(&surface, |states| {
            states
                .data_map
                .get::<RefCell<CatacombSurfaceData>>()
                .and_then(|s| Some(s.borrow().preferred_fractional_scale))
        })
    }

    /// Get buffer scale.
    #[allow(dead_code)]
    fn buffer_scale(&self) -> i32 {
        match self.maybe_surface() {
            Some(surface) => compositor::with_states(&surface, |states| {
                let surface_data = states.data_map.get::<RefCell<CatacombSurfaceData>>();
                if let Some(surface_data) = surface_data {
                    surface_data.borrow().buffer_size.w
                } else {
                    1
                }
            }),
            None => 1,
        }
    }
}

/// A wrapper for different shell surfaces.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellSurface {
    Xdg(ToplevelSurface),
    X11(X11Surface),
}

impl Surface for ShellSurface {
    type State = ();

    fn surface(&self) -> WlSurface {
        match self {
            ShellSurface::Xdg(s) => s.surface(),
            ShellSurface::X11(s) => s.wl_surface().unwrap(),
        }
    }

    fn maybe_surface(&self) -> Option<WlSurface> {
        match self {
            ShellSurface::Xdg(s) => Some(s.surface()),
            ShellSurface::X11(s) => s.wl_surface(),
        }
    }

    fn alive(&self) -> bool {
        match self {
            ShellSurface::Xdg(s) => s.alive(),
            ShellSurface::X11(s) => s.alive(),
        }
    }

    fn initial_configure(&self) {
        match self {
            ShellSurface::Xdg(s) => s.initial_configure(),
            ShellSurface::X11(_) => {},
        }
    }

    fn initial_configure_sent(&self) -> bool {
        match self {
            ShellSurface::Xdg(s) => s.initial_configure_sent(),
            ShellSurface::X11(_) => true,
        }
    }

    fn set_state<F: FnMut(&mut Self::State)>(&self, _f: F) {
        // No-op for ShellSurface wrapper, use specific methods
    }

    fn resize(&self, size: Size<i32, Logical>) {
        match self {
            ShellSurface::Xdg(s) => s.resize(size),
            ShellSurface::X11(s) => {
                let rect = Rectangle::from_size(size);
                s.configure(rect).ok();
            },
        }
    }

    fn acked_size(&self) -> Size<i32, Logical> {
        match self {
            ShellSurface::Xdg(s) => s.acked_size(),
            ShellSurface::X11(s) => {
                 s.geometry().size
            },
        }
    }
    
    fn geometry(&self) -> Option<Rectangle<i32, Logical>> {
         match self {
            ShellSurface::Xdg(s) => s.geometry(),
            ShellSurface::X11(s) => Some(s.geometry()),
        }
    }

    fn set_fullscreen(&self, fullscreen: bool) {
        match self {
            ShellSurface::Xdg(s) => s.set_fullscreen(fullscreen),
            ShellSurface::X11(s) => { s.set_fullscreen(fullscreen).ok(); },
        }
    }

    fn set_activated(&self, activated: bool) {
        match self {
            ShellSurface::Xdg(s) => s.set_activated(activated),
            ShellSurface::X11(s) => { s.set_activated(activated).ok(); },
        }
    }

    fn send_close(&self) {
        match self {
            ShellSurface::Xdg(s) => s.send_close(),
            ShellSurface::X11(s) => { s.close().ok(); },
        }
    }

    fn title(&self) -> Option<String> {
        match self {
            ShellSurface::Xdg(s) => s.title(),
            ShellSurface::X11(s) => Some(s.title()),
        }
    }

    fn app_id(&self) -> Option<String> {
        match self {
            ShellSurface::Xdg(s) => s.app_id(),
            ShellSurface::X11(s) => Some(s.class()),
        }
    }
}

impl Surface for ToplevelSurface {
    type State = ToplevelState;

    fn surface(&self) -> WlSurface {
        self.wl_surface().clone()
    }

    fn alive(&self) -> bool {
        self.alive()
    }

    fn initial_configure(&self) {
        if !self.initial_configure_sent() {
            self.send_configure();
        }
    }

    fn initial_configure_sent(&self) -> bool {
        compositor::with_states(&self.surface(), |states| {
            let surface_data = states.data_map.get::<XdgToplevelSurfaceData>().unwrap();
            surface_data.lock().unwrap().initial_configure_sent
        })
    }

    fn set_state<F: FnMut(&mut Self::State)>(&self, f: F) {
        self.with_pending_state(f);

        // Ignore configures before the initial one.
        if self.initial_configure_sent() {
            self.send_configure();
        }
    }

    fn resize(&self, size: Size<i32, Logical>) {
        self.set_state(|state| {
            state.size = Some(size);
        });
    }

    fn acked_size(&self) -> Size<i32, Logical> {
        compositor::with_states(&self.surface(), |states| {
            let attributes = states
                .data_map
                .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                .and_then(|attributes| attributes.lock().ok());

            attributes.and_then(|attributes| attributes.current.size)
        })
        .unwrap_or_default()
    }

    fn set_fullscreen(&self, fullscreen: bool) {
        self.with_pending_state(|state| {
            if fullscreen {
                state.states.set(State::Fullscreen);
            } else {
                state.states.unset(State::Fullscreen);
            }
        });
        if self.initial_configure_sent() {
             self.send_configure();
        }
    }

    fn set_activated(&self, activated: bool) {
        self.with_pending_state(|state| {
            if activated {
                state.states.set(State::Activated);
            } else {
                state.states.unset(State::Activated);
            }
        });
        if self.initial_configure_sent() {
             self.send_configure();
        }
    }

    fn title(&self) -> Option<String> {
        compositor::with_states(&self.surface(), |states| {
            states
                .data_map
                .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                .and_then(|attributes| attributes.lock().ok())
                .map(|attrs| attrs.title.clone())
        })
        .flatten()
    }

    fn app_id(&self) -> Option<String> {
        compositor::with_states(&self.surface(), |states| {
            states
                .data_map
                .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                .and_then(|attributes| attributes.lock().ok())
                .map(|attrs| attrs.app_id.clone())
        })
        .flatten()
    }
}

impl Surface for PopupSurface {
    type State = PopupState;

    fn surface(&self) -> WlSurface {
        self.wl_surface().clone()
    }

    fn alive(&self) -> bool {
        self.alive()
    }

    fn initial_configure(&self) {
        if self.initial_configure_sent() {
            return;
        }

        let _ = self.send_configure();
    }

    fn initial_configure_sent(&self) -> bool {
        compositor::with_states(&self.surface(), |states| {
            let surface_data = states.data_map.get::<XdgPopupSurfaceData>().unwrap();
            surface_data.lock().unwrap().initial_configure_sent
        })
    }

    fn set_state<F: FnMut(&mut Self::State)>(&self, f: F) {
        self.with_pending_state(f);

        // Ignore configures before the initial one.
        if self.initial_configure_sent() {
            let _ = self.send_configure();
        }
    }

    fn resize(&self, _size: Size<i32, Logical>) {}

    fn acked_size(&self) -> Size<i32, Logical> {
        self.with_pending_state(|state| state.positioner.rect_size)
    }
}

#[derive(Debug)]
pub struct CatacombLayerSurface {
    pub exclusive_zone: ExclusiveZone,
    pub anchor: Anchor,
    surface: LayerSurface,
    layer: Layer,
}

impl CatacombLayerSurface {
    pub fn new(layer: Layer, surface: LayerSurface) -> Self {
        Self { surface, layer, exclusive_zone: Default::default(), anchor: Default::default() }
    }

    pub fn layer(&self) -> Layer {
        self.layer
    }
}

impl Surface for CatacombLayerSurface {
    type State = LayerSurfaceState;

    fn surface(&self) -> WlSurface {
        self.surface.wl_surface().clone()
    }

    fn alive(&self) -> bool {
        self.surface.alive()
    }

    fn initial_configure(&self) {
        if self.initial_configure_sent() {
            return;
        }

        self.send_configure();
    }

    fn initial_configure_sent(&self) -> bool {
        compositor::with_states(&self.surface(), |states| {
            let surface_data = states.data_map.get::<Mutex<LayerSurfaceAttributes>>().unwrap();
            surface_data.lock().unwrap().initial_configure_sent
        })
    }

    fn set_state<F: FnMut(&mut Self::State)>(&self, f: F) {
        self.surface.with_pending_state(f);

        // Ignore configures before the initial one.
        if self.initial_configure_sent() {
            self.surface.send_configure();
        }
    }

    fn resize(&self, size: Size<i32, Logical>) {
        self.set_state(|state| {
            state.size = Some(size);
        });
    }

    fn acked_size(&self) -> Size<i32, Logical> {
        compositor::with_states(&self.surface(), |states| {
            let attributes = states
                .data_map
                .get::<Mutex<LayerSurfaceAttributes>>()
                .and_then(|attributes| attributes.lock().ok());

            attributes.and_then(|attributes| attributes.current.size)
        })
        .unwrap_or_default()
    }

    fn geometry(&self) -> Option<Rectangle<i32, Logical>> {
        None
    }
}

impl Deref for CatacombLayerSurface {
    type Target = LayerSurface;

    fn deref(&self) -> &Self::Target {
        &self.surface
    }
}

impl Surface for LockSurface {
    type State = LockSurfaceState;

    fn surface(&self) -> WlSurface {
        self.wl_surface().clone()
    }

    fn alive(&self) -> bool {
        self.alive()
    }

    fn initial_configure(&self) {
        // Initial configure is sent immediately after
        // ext_session_lock_surface_v1 is bound.
    }

    fn initial_configure_sent(&self) -> bool {
        // See `initial_configure`.
        true
    }

    fn set_state<F: FnMut(&mut Self::State)>(&self, f: F) {
        self.with_pending_state(f);
        self.send_configure();
    }

    fn resize(&self, size: Size<i32, Logical>) {
        self.set_state(|state| {
            let size = (size.w as u32, size.h as u32);
            state.size = Some(size.into());
        });
    }

    fn acked_size(&self) -> Size<i32, Logical> {
        let current_state = self.current_state();
        let size = current_state.size.unwrap_or_default();
        (size.w as i32, size.h as i32).into()
    }

    fn geometry(&self) -> Option<Rectangle<i32, Logical>> {
        None
    }
}

/// Surface for touch input handling.
pub struct InputSurface {
    pub toplevel: Option<InputSurfaceKind>,
    pub surface_offset: Point<f64, Logical>,
    pub surface_scale: f64,
    pub surface: WlSurface,
}

impl InputSurface {
    pub fn new(
        surface: WlSurface,
        surface_offset: Point<f64, Logical>,
        surface_scale: f64,
    ) -> Self {
        Self { surface, surface_scale, surface_offset, toplevel: None }
    }
}

/// Types of input surfaces.
pub enum InputSurfaceKind {
    Layout((Weak<RefCell<Window>>, Option<String>)),
    Layer((WlSurface, Option<String>)),
}

impl InputSurfaceKind {
    /// Extract the App ID from this surface.
    pub fn take_app_id(&mut self) -> Option<String> {
        let (Self::Layout((_, app_id)) | Self::Layer((_, app_id))) = self;
        app_id.take()
    }
}
