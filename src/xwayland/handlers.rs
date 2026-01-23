use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};
use tracing::info;

use crate::catacomb::Catacomb;

impl XWaylandShellHandler for Catacomb {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland.as_mut().expect("XWayland not initialized").shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, surface: X11Surface) {
        if let Some(ext) = self.xwayland.as_mut() {
            let id = surface.window_id();
            if let Some(rect) = ext.pending_configs.remove(&id) {
                let _ = surface.configure(rect);
            }
        }
        self.windows.add_x11(surface);
    }
}

impl XwmHandler for Catacomb {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwayland
            .as_mut()
            .expect("XWayland not initialized")
            .wm
            .as_mut()
            .expect("X11 WM not initialized")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_mapped(true);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(ext) = self.xwayland.as_mut() {
            ext.pending_configs.remove(&window.window_id());
        }
        info!("x11 unmapped window id={}", window.window_id());
        self.windows.mark_dead_x11(window.window_id());
        self.unstall();
    }
    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(ext) = self.xwayland.as_mut() {
            ext.pending_configs.remove(&window.window_id());
        }
        info!("x11 destroyed window id={}", window.window_id());
        self.windows.mark_dead_x11(window.window_id());
        self.unstall();
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<smithay::xwayland::xwm::Reorder>,
    ) {
        if window.is_override_redirect() {
            return;
        }
        let mut rect = window.geometry();
        if let Some(x) = x {
            rect.loc.x = x;
        }
        if let Some(y) = y {
            rect.loc.y = y;
        }
        if let Some(w) = w {
            rect.size.w = w as i32;
        }
        if let Some(h) = h {
            rect.size.h = h as i32;
        }
        if rect.size.w < 1 {
            rect.size.w = 1;
        }
        if rect.size.h < 1 {
            rect.size.h = 1;
        }
        if window.is_mapped() {
            let _ = window.configure(rect);
        } else if let Some(ext) = self.xwayland.as_mut() {
            ext.pending_configs.insert(window.window_id(), rect);
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        mut geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if window.is_override_redirect() {
            return;
        }
        if geometry.size.w < 1 {
            geometry.size.w = 1;
        }
        if geometry.size.h < 1 {
            geometry.size.h = 1;
        }
        if let Some(ext) = self.xwayland.as_mut() {
            ext.pending_configs.insert(window.window_id(), geometry);
        }
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {}
    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}
