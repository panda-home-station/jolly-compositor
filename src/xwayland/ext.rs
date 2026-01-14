use calloop::LoopHandle;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::{X11Wm, XWayland, XWaylandEvent};

use crate::catacomb::Catacomb;

pub struct XWaylandExt {
    pub shell_state: XWaylandShellState,
    pub wm: Option<X11Wm>,
    pub display_number: Option<u32>,
}

pub fn setup(event_loop: LoopHandle<'static, Catacomb>, display_handle: &DisplayHandle) -> XWaylandExt {
    let (xwayland, xwayland_client) = XWayland::spawn(
        display_handle,
        None,
        std::iter::empty::<(&'static str, &'static str)>(),
        true,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| (),
    )
    .expect("failed to start XWayland");

    let xwayland_client_for_wm = xwayland_client.clone();
    event_loop
        .insert_source(xwayland, move |event, _, catacomb| match event {
            XWaylandEvent::Ready { x11_socket, display_number } => {
                let wm = X11Wm::start_wm(catacomb.event_loop.clone(), x11_socket, xwayland_client_for_wm.clone())
                    .expect("Failed to attach X11 Window Manager");
                if let Some(ext) = catacomb.xwayland.as_mut() {
                    ext.display_number = Some(display_number);
                    ext.wm = Some(wm);
                }
            },
            XWaylandEvent::Error => {},
        })
        .expect("failed to insert xwayland source");

    let shell_state = XWaylandShellState::new::<Catacomb>(display_handle);

    XWaylandExt { shell_state, wm: None, display_number: None }
}
