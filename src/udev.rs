//! Udev backend.

use std::collections::HashMap;
use std::error::Error;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, io, mem, process, ptr};

use _linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags;
use indexmap::IndexSet;
use libc::dev_t as DeviceId;
#[cfg(feature = "profiling")]
use profiling::puffin::GlobalProfiler;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{
    DrmCompositor as SmithayDrmCompositor, FrameFlags, PrimaryPlaneElement, RenderFrameResult,
};
use smithay::backend::drm::exporter::gbm::{GbmFramebufferExporter, NodeFilter};
use smithay::backend::drm::gbm::GbmFramebuffer;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, DrmSurface};
use smithay::backend::egl::context::EGLContext;
use smithay::backend::egl::display::EGLDisplay;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer, ffi};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    self, Bind, BufferType, Frame, ImportDma, ImportEgl, Offscreen, Renderer, utils,
};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{AsErrno, Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::output::{Mode, OutputModeSource, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{
    Dispatcher, EventLoop, Interest, LoopHandle, Mode as TriggerMode, PostAction, RegistrationToken,
};
use smithay::reexports::drm::control::connector::{Info as ConnectorInfo, State as ConnectorState};
use smithay::reexports::drm::control::property::{
    Handle as PropertyHandle, Value as PropertyValue,
};
use smithay::reexports::drm::control::{Device, Mode as DrmMode, ModeTypeFlags, ResourceHandles};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_protocols::wp::linux_dmabuf as _linux_dmabuf;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{DevPath, DeviceFd, Logical, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder};
use smithay::wayland::{dmabuf, shm};
use tracing::{debug, error};

use crate::catacomb::Catacomb;
use crate::drawing::{CatacombElement, Graphics};
use crate::output::Output;
use crate::protocols::screencopy::frame::Screencopy;
use crate::trace_error;
use crate::windows::Windows;

/// Default background color.
const CLEAR_COLOR: [f32; 4] = [0., 0., 0., 1.];

/// Retry delay after a WouldBlock when trying to add a DRM device.
const DRM_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Supported DRM color formats.
///
/// These are formats supported by most devices which have at least 8 bits per
/// channel, to ensure we're not falling back to reduced color palettes.
const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
    Fourcc::Xrgb8888,
    Fourcc::Xbgr8888,
];

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
    output_device: Option<OutputDevice>,
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

        let (mut drm, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
        let gbm = GbmDevice::new(device_fd)?;

        let display = unsafe { EGLDisplay::new(gbm.clone())? };
        let context = EGLContext::new(&display)?;

        let mut gles = unsafe { GlesRenderer::new(context).expect("create renderer") };

        // Initialize GPU for EGL rendering (ignore missing EGL_WL_bind_wayland_display).
        let _ = gles.bind_wl_display(display_handle);

        // Create the DRM compositor.
        let (drm_compositor, connector) = self
            .create_drm_compositor(display_handle, windows, &gles, &mut drm, &gbm)
            .ok_or("drm compositor")?;

        // Listen for VBlanks.
        let device_id = drm.device_id();
        let dispatcher =
            Dispatcher::new(drm_notifier, move |event, metadata, catacomb: &mut Catacomb| {
                match event {
                    DrmEvent::VBlank(_crtc) => {
                        let output_device = match &mut catacomb.backend.output_device {
                            Some(output_device) => output_device,
                            None => return,
                        };

                        // Mark the last frame as submitted.
                        trace_error!(output_device.drm_compositor.frame_submitted());

                        // Signal new frame to profiler.
                        #[cfg(feature = "profiling")]
                        GlobalProfiler::lock().new_frame();

                        // Send presentation time feedback.
                        catacomb
                            .windows
                            .mark_presented(&output_device.last_render_states, metadata);

                        // Request redraw before the next VBlank.
                        let frame_interval = catacomb.windows.canvas().frame_interval();
                        let prediction = catacomb.frame_pacer.predict();
                        match prediction.filter(|prediction| prediction < &frame_interval) {
                            Some(prediction) => {
                                catacomb.backend.schedule_redraw(frame_interval - prediction);
                            },
                            None => catacomb.create_frame(),
                        }
                    },
                    DrmEvent::Error(error) => error!("DRM error: {error}"),
                };
            });
        let token = self.event_loop.register_dispatcher(dispatcher.clone())?;

        // Create OpenGL textures.
        let graphics = Graphics::new();

        // Initialize last render state as empty.
        let last_render_states = RenderElementStates { states: HashMap::new() };

        self.output_device = Some(OutputDevice {
            last_render_states,
            drm_compositor,
            graphics,
            gles,
            token,
            gbm,
            drm,
            id: device_id,
            connector,
            screencopy: Default::default(),
        });

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

    /// Create the DRM compositor.
    fn create_drm_compositor(
        &mut self,
        display: &DisplayHandle,
        windows: &mut Windows,
        gles: &GlesRenderer,
        drm: &mut DrmDevice,
        gbm: &GbmDevice<DrmDeviceFd>,
    ) -> Option<(DrmCompositor, smithay::reexports::drm::control::connector::Handle)> {
        let formats_group = match Bind::<Dmabuf>::supported_formats(gles) {
            Some(f) => f,
            None => {
                tracing::error!("Failed to get supported formats: returns None");
                return None;
            }
        };

        // FILTER: Force Linear modifier only to avoid AFBC issues causing Invalid Argument errors
        // Convert to Vec to allow filtering and passing to DrmCompositor
        let mut formats: Vec<_> = formats_group.into_iter().collect();
        
        let linear_formats: Vec<_> = formats.iter().cloned().filter(|f| {
            let mod_val = u64::from(f.modifier);
            // 0 = Linear
            // 72057594037927935 (0x00ffffffffffffff) = Invalid (Implicit/Legacy)
            // STRICT FILTER: Only allow explicit LINEAR (0). 'Invalid' (Implicit) can fail on NVIDIA with Atomic.
            mod_val == 0 
        }).collect();
        
        tracing::info!("DEBUG: Total formats: {}, Linear formats: {}", formats.len(), linear_formats.len());
        if formats.len() > 0 {
             tracing::info!("DEBUG: First format modifier: {:?}", formats[0].modifier);
        }

        if !linear_formats.is_empty() {
            formats = linear_formats;
        } else {
            tracing::warn!("⚠️ No LINEAR/INVALID formats found! Falling back to using ALL available formats (AFBC might be enabled).");
        }


        if formats.is_empty() {
             tracing::error!("❌ No formats supported at all! This is fatal.");
             return None;
        }

        let resources = match drm.resource_handles() {
            Ok(r) => r,
            Err(e) => {
                 tracing::error!("Failed to get DRM resource handles: {:?}", e);
                 return None;
            }
        };

        tracing::info!("Found {} connectors", resources.connectors().len());

        // Find the first connected output port.
        // RETRY LOOP: Wait for valid modes (HDMI handshake can be slow on cold boot)
        let mut connector = None;
        let retry_count = 60; // Increased to 60 (30 seconds) for slow TV cold boots
        
        for attempt in 1..=retry_count {
            // Refresh resources on each attempt to get latest connector state
            let current_resources = match drm.resource_handles() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Failed to refresh resources: {:?}", e);
                    resources.clone() // Fallback to initial resources
                }
            };

            tracing::info!("🔄 Attempt {}/{} Checking connectors...", attempt, retry_count);
            
            let found = current_resources.connectors().iter().find_map(|conn| {
                match drm.get_connector(*conn, true) {
                    Ok(c) => {
                        let state = c.state();
                        let interface = format!("{:?}", c.interface());
                        let modes = c.modes();
                        let max_width = modes.iter().map(|m| m.size().0).max().unwrap_or(0);
                        
                        // Enhanced Logging requested by user
                        tracing::info!("   🔌 Connector {:?} ({}) State: {:?}", conn, interface, state);
                        
                        if state == ConnectorState::Connected {
                            tracing::info!("      Found {} modes. Max Width: {}", modes.len(), max_width);
                            if modes.is_empty() {
                                tracing::warn!("      ⚠️ Connected but NO modes found!");
                            } else {
                                for (idx, m) in modes.iter().take(5).enumerate() {
                                    tracing::info!("      Mode[{}]: {}x{} @ {:.2}Hz", idx, m.size().0, m.size().1, m.vrefresh() as f32 / 1000.0);
                                }
                            }

                            // Try to log EDID presence
                            if let Ok(props) = drm.get_properties(c.handle()) {
                                let (handles, _values) = props.as_props_and_values();
                                for handle in handles {
                                    if let Ok(info) = drm.get_property(*handle) {
                                        if info.name().to_str().unwrap_or("") == "EDID" {
                                            tracing::info!("      ✅ EDID Property detected on this connector.");
                                        }
                                    }
                                }
                            }
                        }

                        if state == ConnectorState::Connected {
                            // Check if modes are plausible (e.g. not just 640x480 safe mode)
                            if max_width >= 1920 {
                                Some(c)
                            } else {
                                tracing::warn!("      ⚠️ Connector {:?} connected but max width {} < 1920. Waiting...", conn, max_width);
                                None
                            }
                        } else {
                            None
                        }
                    },
                    Err(e) => {
                        tracing::error!("Failed to get connector info: {:?}", e);
                        None
                    }
                }
            });

            if found.is_some() {
                connector = found;
                tracing::info!("✅ Found valid connector with high-res modes!");
                break;
            }
            
            if attempt < retry_count {
                tracing::info!("⏳ Waiting for HDMI handshake / High-res modes... (Attempt {}/{})", attempt, retry_count);
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        
        // Fallback: If retry failed, try to get ANY connected connector (even low res)
        if connector.is_none() {
             tracing::warn!("⚠️ High-res retry loop failed. Falling back to ANY connected connector.");
             
             // Refresh resources one last time
             let current_resources = match drm.resource_handles() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Failed to refresh resources for fallback: {:?}", e);
                    resources.clone()
                }
             };

             connector = current_resources.connectors().iter().find_map(|conn| {
                match drm.get_connector(*conn, true) {
                    Ok(c) => if c.state() == ConnectorState::Connected { Some(c) } else { None },
                    Err(_) => None
                }
            });
        }

        // Fallback: If no connected port, try to find ANY HDMI/DP port
        if connector.is_none() {
             tracing::warn!("⚠️ No CONNECTED port found. Falling back to first available HDMI/DP port...");
             connector = resources.connectors().iter().find_map(|conn| {
                 match drm.get_connector(*conn, true) {
                    Ok(c) => {
                         let name = format!("{:?}", c.interface());
                         if name.contains("HDMI") || name.contains("DisplayPort") {
                             Some(c)
                         } else {
                             None
                         }
                    }
                    Err(_) => None
                 }
             });
        }

        if connector.is_none() {
             tracing::error!("❌ No connected connector found and fallback failed!");
             return None;
        }
        let connector = connector.unwrap();

        // HACK: Force "max bpc" to 8 to enable 4K@60Hz YUV420
        // This is required because some drivers/cables fail to negotiate high bandwidth RGB,
        // and forcing 8bpc allows the driver to fall back to YUV420 which fits in HDMI 2.0 bandwidth.
        
        let connector_handle = connector.handle();
        if let Ok(props) = drm.get_properties(connector_handle) {
            let (handles, _values) = props.as_props_and_values();
            for (_i, handle) in handles.iter().enumerate() {
                if let Ok(info) = drm.get_property(*handle) {
                    let name = info.name().to_str().unwrap_or("unknown");
                    
                    if name == "Colorspace" {
                         // Force BT2020_YCC (10) to support 4K@60Hz YUV420 on HDMI 2.0
                         // This is often required alongside the YCbCr420_ONLY mode flag.
                         let target_value = 10u64;
                         
                         if let Err(e) = drm.set_property(connector_handle, *handle, target_value.into()) {
                             tracing::warn!("Failed to set 'Colorspace' to 10: {:?}", e);
                         } else {
                             tracing::info!("✅ Set 'Colorspace' to 10 (BT2020_YCC)");
                         }
                    }

                    if name == "max bpc" {
                        if let Err(e) = drm.set_property(connector_handle, *handle, 8u64.into()) {
                             tracing::warn!("Failed to set 'max bpc': {:?}", e);
                        }
                    }
                }
            }
        }

        let modes = connector.modes();

        // Try to preserve existing mode if the output matches.
        let output_name = format!("{:?}", connector.interface());
        let current_mode = windows.output.canvas().physical_resolution();
        let existing_name = windows.output.smithay_output().name();

        let preserved_mode = if existing_name == output_name {
            modes.iter().find(|m| {
                let (w, h) = m.size();
                // Match resolution and refresh rate (fuzzy match refresh if needed, but strict is safer for now)
                w as i32 == current_mode.w && h as i32 == current_mode.h &&
                (m.vrefresh() as i32 * 1000) == windows.output.canvas().frame_interval().as_millis() as i32 * 1000 // Approximate check?
                // Actually windows.output.canvas().mode has the refresh rate directly.
            })
        } else {
            None
        };
        
        // If strict match fails, try resolution only match
        let preserved_mode = preserved_mode.or_else(|| {
             if existing_name == output_name {
                modes.iter().find(|m| {
                    let (w, h) = m.size();
                    w as i32 == current_mode.w && h as i32 == current_mode.h
                })
             } else {
                 None
             }
        });

        let _best_existing_mode = preserved_mode
            .or_else(|| {
                // Find the preferred mode (usually native resolution)
                let preferred = modes.iter().find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED));
                
                if let Some(preferred) = preferred {
                    // Try to find a mode with the same resolution but higher refresh rate
                    let (w, h) = preferred.size();
                    modes.iter()
                         .filter(|m| m.size() == (w, h))
                         .max_by_key(|m| m.vrefresh())
                         .or(Some(preferred)) // Fallback to preferred if something goes wrong
                 } else {
                     modes.first()
                 }
            });

        // === MODE SELECTION ===
        let mut candidates: Vec<DrmMode> = modes.to_vec();
        
        // Sort: Preferred first, then by refresh rate descending
        candidates.sort_by(|a, b| {
            let a_pref = a.mode_type().contains(ModeTypeFlags::PREFERRED);
            let b_pref = b.mode_type().contains(ModeTypeFlags::PREFERRED);
            match (a_pref, b_pref) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.vrefresh().cmp(&a.vrefresh()),
            }
        });

        // Simple dedup
        let mut unique_candidates = Vec::new();
        for m in candidates {
            if !unique_candidates.contains(&m) {
                unique_candidates.push(m);
            }
        }
        
        tracing::info!("Candidate Modes for Attempt:");
        for (i, m) in unique_candidates.iter().enumerate() {
            tracing::info!("  [#{}] {}x{} @ {:.2}Hz (Flags: 0x{:x})", i, m.size().0, m.size().1, m.vrefresh() as f32 / 1000.0, m.flags().bits());
        }

        for (attempt, attempted_mode) in unique_candidates.iter().enumerate() {
            tracing::info!("🚀 Attempting Mode #{}: {:?}", attempt, attempted_mode);

            // 1. Prepare Mode (Hack for 4K Bandwidth)
            let mut mode_to_use = *attempted_mode;
            
            // FIX: Force YCbCr420_ONLY (0x8000) for 4K modes to fit HDMI 2.0 bandwidth
            // This prevents "Invalid Argument" errors on NVIDIA/Atomic when using 4K@60Hz.
            if mode_to_use.size().0 == 3840 && mode_to_use.size().1 == 2160 {
                 // 0x8000 is DRM_MODE_FLAG_YCbCr420_ONLY
                 const DRM_MODE_FLAG_YCBCR420_ONLY: u32 = 0x8000;
                 
                 // Hack to modify private fields of DrmMode (which wraps drm_mode_modeinfo)
                 // Structure layout of drm_mode_modeinfo:
                 // u32 clock;
                 // u16 hdisplay, hsync_start, hsync_end, htotal, hskew; (5 * 2 = 10 bytes)
                 // u16 vdisplay, vsync_start, vsync_end, vtotal, vscan; (5 * 2 = 10 bytes)
                 // u32 vrefresh;
                 // u32 flags;  <-- Offset = 4 + 10 + 10 + 4 = 28 bytes.
                 
                 unsafe {
                      let ptr = &mut mode_to_use as *mut DrmMode as *mut u8;
                      let flags_ptr = ptr.add(28) as *mut u32;
                      let current_flags = *flags_ptr;
                      
                      tracing::info!("DEBUG: 4K Mode Flags Check. Current: 0x{:x}, Target: 0x{:x}", current_flags, current_flags | DRM_MODE_FLAG_YCBCR420_ONLY);
                      
                      // Only add if not present
                      if (current_flags & DRM_MODE_FLAG_YCBCR420_ONLY) == 0 {
                         *flags_ptr = current_flags | DRM_MODE_FLAG_YCBCR420_ONLY;
                         tracing::info!("🔧 Forced YCbCr420_ONLY (0x8000) for 4K mode. Old flags: 0x{:x}, New flags: 0x{:x}", current_flags, *flags_ptr);
                      }
                  }
            }

            // 1. Create Surface
            let surface_opt = Self::create_surface(drm, resources.clone(), &connector, mode_to_use);
            
            if surface_opt.is_none() {
                tracing::warn!("❌ Failed to create surface for mode #{}. Skipping.", attempt);
                continue;
            }
            let surface = surface_opt.unwrap();

            // 2. Create Allocator
            let gbm_flags = GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT;
            let allocator = GbmAllocator::new(gbm.clone(), gbm_flags);

            // 3. Prepare Output Mode for Compositor
            let (width, height) = attempted_mode.size();
            let mode = Mode {
                size: (width as i32, height as i32).into(),
                refresh: attempted_mode.vrefresh() as i32 * 1000,
            };

            // Update Windows Output (temporary, confirmed if success)
            let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
            let output_name = format!("{:?}", connector.interface());
            
            // Note: We modify `windows` here, but if it fails we overwrite it next loop.
            // Ideally we should only update windows on success, but `DrmCompositor::new` needs `output_mode_source` from windows?
            // `output_mode_source` is `windows.canvas().into()`.
            // So we MUST update windows before creating compositor.
            
            windows.set_output(Output::new(display, output_name, mode, PhysicalProperties {
                size: (physical_width as i32, physical_height as i32).into(),
                subpixel: Subpixel::Unknown,
                serial_number: "Unknown".into(),
                model: "Generic DRM".into(),
                make: "Catacomb".into(),
            }));
            
            // Add modes to output
             for drm_mode in modes.iter() {
                let (w, h) = drm_mode.size();
                let m = Mode {
                    size: (w as i32, h as i32).into(),
                    refresh: drm_mode.vrefresh() as i32 * 1000,
                };
                windows.output.add_mode(m);
            }

            // 4. Create Compositor (This triggers Atomic Commit!)
            let output_mode_source: OutputModeSource = windows.canvas().into();
            
            // Re-create formats iterator for each attempt (it was consumed? No, `formats` is Vec, we can iterate)
            // But `DrmCompositor::new` takes `impl IntoIterator`. `formats.clone()` works.
            
            let res = DrmCompositor::new(
                output_mode_source,
                surface,
                None,
                allocator,
                GbmFramebufferExporter::new(gbm.clone(), NodeFilter::All),
                SUPPORTED_COLOR_FORMATS.iter().copied(),
                formats.clone(), 
                Size::default(),
                None,
            );

            match res {
                Ok(compositor) => {
                    tracing::info!("✅ Successfully created DrmCompositor with mode #{}: {:?}", attempt, attempted_mode);
                    return Some((compositor, connector.handle()));
                },
                Err(e) => {
                    tracing::error!("❌ Mode-setting failed for mode #{}: {:?}", attempt, e);
                    tracing::warn!("Retrying with next candidate...");
                }
            }
        }
        
        tracing::error!("❌ All candidate modes failed! Cannot initialize DRM compositor.");
        None
    }

    /// Create DRM surface on the ideal CRTC.
    fn create_surface(
        drm: &mut DrmDevice,
        resources: ResourceHandles,
        connector: &ConnectorInfo,
        mode: DrmMode,
    ) -> Option<DrmSurface> {
        for encoder in connector.encoders() {
            let encoder = match drm.get_encoder(*encoder) {
                Ok(encoder) => encoder,
                Err(_) => continue,
            };

            // Get all CRTCs compatible with the encoder.
            let mut crtcs = resources.filter_crtcs(encoder.possible_crtcs());

            // Sort CRTCs by maximum number of overlay planes.
            crtcs.sort_by_cached_key(|crtc| {
                drm.planes(crtc).map_or(0, |planes| -(planes.overlay.len() as isize))
            });

            // Get first CRTC allowing for successful surface creation.
            for crtc in crtcs {
                if let Ok(drm_surface) = drm.create_surface(crtc, mode, &[connector.handle()]) {
                    return Some(drm_surface);
                }
            }
        }

        None
    }
}

/// Target device for rendering.
pub struct OutputDevice {
    last_render_states: RenderElementStates,
    screencopy: Option<Screencopy>,
    drm_compositor: DrmCompositor,
    gbm: GbmDevice<DrmDeviceFd>,
    gles: GlesRenderer,
    graphics: Graphics,
    drm: DrmDevice,
    id: DeviceId,
    connector: smithay::reexports::drm::control::connector::Handle,

    token: RegistrationToken,
}

impl OutputDevice {
    /// Set the output mode.
    pub fn set_mode(&mut self, mode: smithay::output::Mode) -> Result<(), Box<dyn Error>> {
        tracing::info!("Setting hardware mode to {}x{}@{}mHz", mode.size.w, mode.size.h, mode.refresh);
        let connector_info = self.drm.get_connector(self.connector, false)?;
        
        let found_mode = connector_info
            .modes()
            .iter()
            .find(|m| {
                let (w, h) = m.size();
                w as i32 == mode.size.w
                    && h as i32 == mode.size.h
                    && (m.vrefresh() as i32 * 1000) == mode.refresh
            })
            .copied();

        let drm_mode = if let Some(m) = found_mode {
            m
        } else {
             return Err("Mode not found".into());
        };

        tracing::info!("Using DRM mode: {:?}", drm_mode);
        self.drm_compositor.surface().use_mode(drm_mode)?;
        
        // Reset buffer ages to force full redraw and ensure new buffers are allocated for new size
        self.drm_compositor.reset_buffer_ages();
        Ok(())
    }

    /// Get DRM property handle.
    fn get_drm_property(&self, name: &str) -> Result<PropertyHandle, io::Error> {
        let crtc = self.drm_compositor.crtc();

        // Get all available properties.
        let properties = self.drm.get_properties(crtc)?;
        let (property_handles, _) = properties.as_props_and_values();

        // Find property matching the requested name.
        for handle in property_handles {
            let property_info = self.drm.get_property(*handle)?;
            let property_name = property_info
                .name()
                .to_str()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;

            if property_name == name {
                return Ok(*handle);
            }
        }

        Err(io::Error::new(io::ErrorKind::NotFound, "missing drm property"))
    }

    /// Set output DPMS state.
    fn set_enabled(&mut self, enabled: bool) {
        let property = match self.get_drm_property("ACTIVE") {
            Ok(property) => property,
            Err(err) => {
                error!("Could not get DRM property `ACTIVE`: {err}");
                return;
            },
        };

        let crtc = self.drm_compositor.crtc();

        let value = PropertyValue::Boolean(enabled);
        trace_error!(self.drm.set_property(crtc, property, value.into()));
    }

    /// Render a frame.
    ///
    /// Will return `true` if something was rendered.
    #[cfg_attr(feature = "profiling", profiling::function)]
    fn render(
        &mut self,
        event_loop: &LoopHandle<'static, Catacomb>,
        windows: &mut Windows,
        cursor_position: Option<Point<f64, Logical>>,
    ) -> Result<RenderedFrame, Box<dyn Error>> {
        let scale = windows.canvas().scale();

        // Update output mode since we're using static for transforms.
        self.drm_compositor.set_output_mode_source(windows.canvas().into());

        let textures = windows.textures(&mut self.gles, &mut self.graphics, cursor_position);
        let mut frame_result = self.drm_compositor.render_frame(
            &mut self.gles,
            textures,
            CLEAR_COLOR,
            FrameFlags::DEFAULT | FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY,
        )?;
        let rendered = !frame_result.is_empty;

        // Get fence for measuring frame completion.
        let gpu_fence = match &frame_result.primary_element {
            PrimaryPlaneElement::Swapchain(element) => element.sync.export(),
            _ => None,
        };

        // Update last render states.
        self.last_render_states = mem::take(&mut frame_result.states);

        // Copy framebuffer for screencopy.
        if let Some(mut screencopy) = self.screencopy.take() {
            // Mark entire buffer as damaged.
            let region = screencopy.region();
            let damage = [Rectangle::from_size(region.size)];
            screencopy.damage(&damage);

            let buffer = screencopy.buffer();
            let sync_point = if let Ok(dmabuf) = dmabuf::get_dmabuf(buffer) {
                Self::copy_framebuffer_dma(
                    &mut self.gles,
                    scale,
                    &frame_result,
                    region,
                    &mut dmabuf.clone(),
                )?
            } else {
                // Ignore unknown buffer types.
                let buffer_type = renderer::buffer_type(buffer);
                if !matches!(buffer_type, Some(BufferType::Shm)) {
                    return Err(format!("unsupported buffer format: {buffer_type:?}").into());
                }

                self.copy_framebuffer_shm(windows, cursor_position, region, buffer)?
            };

            // Wait for OpenGL sync to submit screencopy, frame.
            match sync_point.export() {
                Some(sync_fd) => {
                    // Wait for fence to be done.
                    let mut screencopy = Some(screencopy);
                    let source = Generic::new(sync_fd, Interest::READ, TriggerMode::OneShot);
                    let _ = event_loop.insert_source(source, move |_, _, _| {
                        screencopy.take().unwrap().submit();
                        Ok(PostAction::Remove)
                    });
                },
                None => screencopy.submit(),
            }
        }

        // Skip frame submission if everything used direct scanout.
        if rendered {
            self.drm_compositor.queue_frame(())?;
        }

        Ok(RenderedFrame { rendered, gpu_fence })
    }

    /// Copy a region of the framebuffer to a DMA buffer.
    #[cfg_attr(feature = "profiling", profiling::function)]
    fn copy_framebuffer_dma(
        gles: &mut GlesRenderer,
        scale: f64,
        frame_result: &RenderFrameResult<GbmBuffer, GbmFramebuffer, CatacombElement>,
        region: Rectangle<i32, Physical>,
        buffer: &mut Dmabuf,
    ) -> Result<SyncPoint, Box<dyn Error>> {
        // Bind the screencopy buffer as render target.
        let mut framebuffer = gles.bind(buffer)?;

        // Blit the framebuffer into the target buffer.
        let damage = [Rectangle::from_size(region.size)];
        let sync_point = frame_result.blit_frame_result(
            region.size,
            Transform::Normal,
            scale,
            gles,
            &mut framebuffer,
            damage,
            [],
        )?;

        Ok(sync_point)
    }

    /// Copy a region of the framebuffer to an SHM buffer.
    #[cfg_attr(feature = "profiling", profiling::function)]
    fn copy_framebuffer_shm(
        &mut self,
        windows: &mut Windows,
        cursor_position: Option<Point<f64, Logical>>,
        region: Rectangle<i32, Physical>,
        buffer: &WlBuffer,
    ) -> Result<SyncPoint, Box<dyn Error>> {
        // Create and bind an offscreen render buffer.
        let buffer_dimensions = renderer::buffer_dimensions(buffer).unwrap();
        let mut offscreen_buffer: GlesRenderbuffer =
            self.gles.create_buffer(Fourcc::Abgr8888, buffer_dimensions)?;
        let mut framebuffer = self.gles.bind(&mut offscreen_buffer)?;

        let canvas = windows.canvas();
        let scale = canvas.scale();
        let output_size = canvas.physical_resolution();
        let transform = canvas.orientation().output_transform();

        // Calculate drawing area after output transform.
        let damage = transform.transform_rect_in(region, &output_size);

        // Collect textures for rendering.
        let textures = windows.textures(&mut self.gles, &mut self.graphics, cursor_position);

        // Initialize the buffer to our clear color.
        let mut frame = self.gles.render(&mut framebuffer, output_size, transform)?;
        frame.clear(CLEAR_COLOR.into(), &[damage])?;

        // Render everything to the offscreen buffer.
        utils::draw_render_elements(&mut frame, scale, textures, &[damage])?;

        // Ensure rendering was fully completed.
        let sync_point = frame.finish()?;

        // Copy offscreen buffer's content to the SHM buffer.
        shm::with_buffer_contents_mut(buffer, |shm_buffer, shm_len, buffer_data| {
            // Ensure SHM buffer is in an acceptable format.
            if buffer_data.format != wl_shm::Format::Argb8888
                || buffer_data.stride != region.size.w * 4
                || buffer_data.height != region.size.h
                || shm_len as i32 != buffer_data.stride * buffer_data.height
            {
                return Err::<_, Box<dyn Error>>("Invalid buffer format".into());
            }

            // Copy framebuffer data to the SHM buffer.
            self.gles.with_context(|gl| unsafe {
                gl.ReadPixels(
                    region.loc.x,
                    region.loc.y,
                    region.size.w,
                    region.size.h,
                    ffi::RGBA,
                    ffi::UNSIGNED_BYTE,
                    shm_buffer.cast(),
                );
            })?;

            // Convert OpenGL's RGBA to ARGB.
            for i in 0..(region.size.w * region.size.h) as usize {
                unsafe {
                    let src = shm_buffer.offset(i as isize * 4);
                    let dst = shm_buffer.offset(i as isize * 4 + 2);
                    ptr::swap(src, dst);
                }
            }

            Ok(sync_point)
        })?
    }

    /// Default dma surface feedback.
    fn default_dmabuf_feedback(
        &self,
        backend: &UdevBackend,
    ) -> Result<DmabufFeedback, Box<dyn Error>> {
        // Get planes for the DRM surface.
        let surface = self.drm_compositor.surface();
        let planes = surface.planes();

        // Get formats supported by ANY primary plane and the renderer.
        let dmabuf_formats = self.gles.dmabuf_formats();
        let dmabuf_formats = dmabuf_formats.indexset();
        let primary_formats: IndexSet<_> =
            planes.primary.iter().flat_map(|plane| plane.formats.iter()).copied().collect();
        let primary_formats = primary_formats.intersection(dmabuf_formats).copied();

        // Get formats supported by ANY overlay plane and the renderer.
        let any_overlay_formats: IndexSet<_> =
            planes.overlay.iter().flat_map(|plane| plane.formats.iter()).copied().collect();
        let any_overlay_formats = any_overlay_formats.intersection(dmabuf_formats).copied();

        // Get formats supported by ALL overlay planes and the renderer.
        let mut all_overlay_formats = dmabuf_formats.clone();
        all_overlay_formats
            .retain(|format| planes.overlay.iter().all(|plane| plane.formats.contains(format)));

        // The dmabuf feedback DRM device is expected to have a render node, so if this
        // device doesn't have one, we fall back to the first one that does.
        let mut gbm_id = self.gbm.dev_id()?;
        if !DrmNode::from_dev_id(gbm_id)?.has_render() {
            let drm_node = backend
                .device_list()
                .filter_map(|(_, path)| DrmNode::from_path(path).ok())
                .find(DrmNode::has_render)
                .ok_or("no drm device with render node")?;

            debug!(
                "{:?} has no render node, using {:?} for dmabuf feedback",
                self.gbm.dev_path().unwrap_or_default(),
                drm_node.dev_path().unwrap_or_default(),
            );

            gbm_id = drm_node.dev_id();
        }

        // Setup feedback builder.
        let feedback_builder = DmabufFeedbackBuilder::new(gbm_id, dmabuf_formats.iter().copied());

        // Create default feedback preference.
        let surface_id = surface.device_fd().dev_id()?;
        let flags = Some(TrancheFlags::Scanout);
        let feedback = feedback_builder
            // Ideally pick a format which can be scanned out on ALL overlay planes.
            .add_preference_tranche(surface_id, flags, all_overlay_formats)
            // Otherwise try formats which can be scanned out on ANY overlay plane.
            .add_preference_tranche(surface_id, flags, any_overlay_formats)
            // Fallback to primary formats, still supporting direct scanout in fullscreen.
            .add_preference_tranche(surface_id, flags, primary_formats)
            .build()?;

        Ok(feedback)
    }
}

/// Information about a rendered frame.
pub struct RenderedFrame {
    pub gpu_fence: Option<OwnedFd>,
    pub rendered: bool,
}

/// DRM compositor type alias.
type DrmCompositor = SmithayDrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;


