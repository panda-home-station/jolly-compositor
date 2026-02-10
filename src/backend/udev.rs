//! Udev backend.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, process};

use libc::dev_t as DeviceId;
#[cfg(feature = "profiling")]
use profiling::puffin::GlobalProfiler;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{AsErrno, Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{
    EventLoop, LoopHandle, RegistrationToken,
};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{DevPath, DeviceFd, Logical, Point};
use smithay::wayland::dmabuf::DmabufFeedback;
use tracing::error;

use crate::state::Catacomb;
use crate::protocols::screencopy::frame::Screencopy;
use crate::trace_error;
use crate::shell::windows::Windows;

use crate::backend::drm::{OutputDevice, RenderedFrame};

/// Retry delay after a WouldBlock when trying to add a DRM device.
const DRM_RETRY_DELAY: Duration = Duration::from_millis(250);

pub fn run() {
    tracing::info!("🚀 Catacomb UDEV starting");
    // Disable ARM framebuffer compression formats.
    //
    // This is necessary due to a driver bug which causes random artifacts to show
    // up on the primary plane when rendering buffers with the AFBC modifier:
    //
    // https://gitlab.freedesktop.org/mesa/mesa/-/issues/7968#note_1799187
    unsafe { env::set_var("PAN_MESA_DEBUG", "noafbc") };

    let mut event_loop = EventLoop::try_new().expect("event loop");
    let udev = Udev::new(event_loop.handle());
    let mut catacomb = Catacomb::new(event_loop.handle(), udev);

    // Create backend and add presently connected devices.
    let backend = UdevBackend::new(&catacomb.seat_name).expect("init udev");
    for (_, path) in backend.device_list() {
        add_device(&mut catacomb, path.into());
    }

    // Setup hardware acceleration.
    match catacomb.backend.default_dmabuf_feedback(&backend) {
        Ok(dmabuf_feedback) => {
             catacomb.dmabuf_state.create_global_with_default_feedback::<Catacomb>(
                &catacomb.display_handle,
                &dmabuf_feedback,
            );
        },
        Err(e) => {
            tracing::warn!("⚠️ Failed to initialize default dmabuf feedback (no output device?): {}", e);
            tracing::warn!("⚠️ Service will continue running to wait for hotplug events...");
        }
    }

    // Handle device events.
    event_loop
        .handle()
        .insert_source(backend, move |event, _, catacomb| match event {
            UdevEvent::Added { path, .. } => add_device(catacomb, path),
            UdevEvent::Changed { device_id } => {
                trace_error!(catacomb.backend.change_device(
                    &catacomb.display_handle,
                    &mut catacomb.windows,
                    device_id,
                ));
            },
            UdevEvent::Removed { device_id } => catacomb.backend.remove_device(device_id),
        })
        .expect("insert udev source");

    // Continously dispatch event loop.
    while !catacomb.terminated {
        if let Err(error) = event_loop.dispatch(None, &mut catacomb) {
            error!("Event loop error: {error}");
            break;
        }
        catacomb.display_handle.flush_clients().expect("flushing clients");
    }
}

/// Add udev device, automatically kicking off rendering for it.
fn add_device(catacomb: &mut Catacomb, path: PathBuf) {
    // Try to create the device.
    let result =
        catacomb.backend.add_device(&catacomb.display_handle, &mut catacomb.windows, &path, true);

    // Kick-off rendering if the device creation was successful.
    match result {
        Ok(()) => {
            tracing::info!("✅ Device added successfully, starting frame creation.");
            catacomb.create_frame()
        },
        Err(err) => {
            tracing::error!("❌ Failed to add device: {}", err);
            tracing::error!("This is likely why the system panics later with 'missing output device'.");
        },
    }
}

/// Udev backend shared state.
pub struct Udev {
    scheduled_redraws: Vec<RegistrationToken>,
    event_loop: LoopHandle<'static, Catacomb>,
    pub output_device: Option<OutputDevice>,
    session: LibSeatSession,
}

impl Udev {
    fn new(event_loop: LoopHandle<'static, Catacomb>) -> Self {
        // Initialize the VT session.
        let (session, notifier) = match LibSeatSession::new() {
            Ok(session) => session,
            Err(_) => {
                error!(
                    "[error] Unable to start libseat session: Ensure logind/seatd service is \
                     running with no active session"
                );
                process::exit(666);
            },
        };

        // Setup input handling.

        let mut context =
            Libinput::new_with_udev::<LibinputSessionInterface<_>>(session.clone().into());
        context.udev_assign_seat(&session.seat()).expect("assign seat");

        let input_backend = LibinputInputBackend::new(context.clone());
        event_loop
            .insert_source(input_backend, |event, _, catacomb| catacomb.handle_input(event))
            .expect("insert input source");

        // Register notifier for handling session events.
        event_loop
            .insert_source(notifier, move |event, _, catacomb| match event {
                SessionEvent::PauseSession => {
                    context.suspend();

                    if let Some(output_device) = &mut catacomb.backend.output_device {
                        output_device.drm.pause();
                    }
                },
                SessionEvent::ActivateSession => {
                    if let Err(err) = context.resume() {
                        error!("Failed to resume libinput: {err:?}");
                    }

                    // Reset DRM state.
                    if let Some(output_device) = &mut catacomb.backend.output_device {
                        // NOTE: Ideally we'd just reset the DRM+Compositor here, but this is
                        // currently not possible due to a bug in Smithay or the driver.
                        let device_id = output_device.id;
                        let result = catacomb.backend.change_device(
                            &catacomb.display_handle,
                            &mut catacomb.windows,
                            device_id,
                        );
                        if let Err(err) = result {
                            error!("Failed reconnecting DRM device: {err:?}");
                        }
                    }

                    catacomb.force_redraw(true);
                },
            })
            .expect("insert notifier source");

        Self {
            event_loop,
            session,
            scheduled_redraws: Default::default(),
            output_device: Default::default(),
        }
    }

    /// Get Wayland seat name.
    pub fn seat_name(&self) -> String {
        self.session.seat()
    }

    /// Change Unix TTY.
    pub fn change_vt(&mut self, vt: i32) {
        trace_error!(self.session.change_vt(vt));
    }

    /// Set the output mode.
    pub fn set_mode(&mut self, mode: smithay::output::Mode) -> Result<(), Box<dyn Error>> {
        if let Some(output_device) = &mut self.output_device {
            output_device.set_mode(mode)?;
        }
        Ok(())
    }

    /// Set output power saving state.
    pub fn set_display_status(&mut self, on: bool) {
        let output_device = match &mut self.output_device {
            Some(output_device) => output_device,
            None => return,
        };

        output_device.set_enabled(on);

        // Request immediate redraw, so vblanks start coming in again.
        if on {
            self.schedule_redraw(Duration::ZERO);
        }
    }

    /// Render a frame.
    ///
    /// Will return `true` if something was rendered.
    pub fn render(
        &mut self,
        windows: &mut Windows,
        cursor_position: Option<Point<f64, Logical>>,
    ) -> RenderedFrame {
        let output_device = match &mut self.output_device {
            Some(output_device) => output_device,
            None => return RenderedFrame { rendered: false, gpu_fence: None },
        };

        match output_device.render(&self.event_loop, windows, cursor_position) {
            Ok(frame) => frame,
            Err(err) => {
                error!("{err}");
                RenderedFrame { rendered: false, gpu_fence: None }
            },
        }
    }

    /// Get the current output's renderer.
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        self.output_device.as_mut().map(|output_device| &mut output_device.gles)
    }

    /// Reset the DRM compostor's buffer ages.
    pub fn reset_buffer_ages(&mut self) {
        if let Some(output_device) = &mut self.output_device {
            output_device.drm_compositor.reset_buffer_ages();
        }
    }

    /// Request a redraw once `duration` has passed.
    pub fn schedule_redraw(&mut self, duration: Duration) {
        let token = self
            .event_loop
            .insert_source(Timer::from_duration(duration), move |_, _, catacomb| {
                catacomb.create_frame();
                TimeoutAction::Drop
            })
            .expect("insert render timer");
        self.scheduled_redraws.push(token);
    }

    /// Cancel all pending redraws.
    pub fn cancel_scheduled_redraws(&mut self) {
        for scheduled_redraw in self.scheduled_redraws.drain(..) {
            self.event_loop.remove(scheduled_redraw);
        }
    }

    /// Stage a screencopy request for the next frame.
    pub fn request_screencopy(&mut self, screencopy: Screencopy) {
        let output_device = match &mut self.output_device {
            Some(output_device) => output_device,
            None => return,
        };

        // Stage new screencopy.
        output_device.screencopy = Some(screencopy);
    }

    /// Default dma surface feedback.
    fn default_dmabuf_feedback(
        &self,
        backend: &UdevBackend,
    ) -> Result<DmabufFeedback, Box<dyn Error>> {
        match &self.output_device {
            Some(output_device) => output_device.default_dmabuf_feedback(backend),
            None => Err("missing output device".into()),
        }
    }

    fn add_device(
        &mut self,
        display_handle: &DisplayHandle,
        windows: &mut Windows,
        path: &Path,
        retry_on_error: bool,
    ) -> Result<(), Box<dyn Error>> {
        let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let device_fd = match self.session.open(path, open_flags) {
            Ok(fd) => DrmDeviceFd::new(DeviceFd::from(fd)),
            Err(err) => {
                // Retry device on WouldBlock error.
                if retry_on_error && err.as_errno() == Some(11) {
                    let timer = Timer::from_duration(DRM_RETRY_DELAY);
                    let retry_path = path.to_path_buf();
                    self.event_loop
                        .insert_source(timer, move |_, _, catacomb| {
                            trace_error!(catacomb.backend.add_device(
                                &catacomb.display_handle,
                                &mut catacomb.windows,
                                &retry_path,
                                false,
                            ));

                            TimeoutAction::Drop
                        })
                        .expect("drm retry");
                }

                return Err(err.into());
            },
        };

        let output_device = OutputDevice::new(
            display_handle,
            device_fd,
            windows,
            self.event_loop.clone(),
        )?;

        self.output_device = Some(output_device);

        Ok(())
    }

    fn remove_device(&mut self, device_id: DeviceId) {
        let output_device = self.output_device.take();
        if let Some(output_device) = output_device.filter(|device| device.id == device_id) {
            self.event_loop.remove(output_device.token);
        }
    }

    fn change_device(
        &mut self,
        display_handle: &DisplayHandle,
        windows: &mut Windows,
        device_id: DeviceId,
    ) -> Result<(), Box<dyn Error>> {
        let device = self.output_device.as_ref().filter(|dev| dev.id == device_id);
        let path = device.and_then(|device| device.gbm.dev_path());
        if let Some(path) = path {
            self.remove_device(device_id);
            self.add_device(display_handle, windows, &path, true)?;
        }

        Ok(())
    }

}
