//! DRM/KMS Output backend.

use std::collections::HashMap;
use std::error::Error;
use std::os::fd::OwnedFd;
use std::{io, mem, ptr};

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
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer, ffi};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    self, Bind, BufferType, Frame, ImportDma, ImportEgl, Offscreen, Renderer, utils,
};
use smithay::backend::udev::UdevBackend;
use smithay::output::{Mode, OutputModeSource, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    Dispatcher, Interest, LoopHandle, Mode as TriggerMode, PostAction, RegistrationToken,
};
use smithay::reexports::drm::control::connector::{Info as ConnectorInfo, State as ConnectorState};
use smithay::reexports::drm::control::property::{
    Handle as PropertyHandle, Value as PropertyValue,
};
use smithay::reexports::drm::control::{Device, Mode as DrmMode, ModeTypeFlags, ResourceHandles};
use smithay::reexports::wayland_protocols::wp::linux_dmabuf as _linux_dmabuf;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{DevPath, Logical, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder};
use smithay::wayland::{dmabuf, shm};
use tracing::{debug, error};

use crate::state::Catacomb;
use crate::output::drawing::{CatacombElement, Graphics};
use crate::output::Output;
use crate::protocols::screencopy::frame::Screencopy;
use crate::trace_error;
use crate::shell::windows::Windows;

/// Default background color.
pub const CLEAR_COLOR: [f32; 4] = [0., 0., 0., 1.];

/// Supported DRM color formats.
pub const SUPPORTED_COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
    Fourcc::Xrgb8888,
    Fourcc::Xbgr8888,
];

/// Information about a rendered frame.
pub struct RenderedFrame {
    pub gpu_fence: Option<OwnedFd>,
    pub rendered: bool,
}

/// DRM compositor type alias.
pub type DrmCompositor = SmithayDrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// Target device for rendering.
pub struct OutputDevice {
    pub last_render_states: RenderElementStates,
    pub screencopy: Option<Screencopy>,
    pub drm_compositor: DrmCompositor,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub gles: GlesRenderer,
    pub graphics: Graphics,
    pub drm: DrmDevice,
    pub id: DeviceId,
    pub connector: smithay::reexports::drm::control::connector::Handle,

    pub token: RegistrationToken,
}

impl OutputDevice {
    pub fn new(
        display_handle: &DisplayHandle,
        device_fd: DrmDeviceFd,
        windows: &mut Windows,
        event_loop: LoopHandle<'static, Catacomb>,
    ) -> Result<Self, Box<dyn Error>> {
        let (mut drm, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
        let gbm = GbmDevice::new(device_fd)?;

        let display = unsafe { EGLDisplay::new(gbm.clone())? };
        let context = EGLContext::new(&display)?;

        let mut gles = unsafe { GlesRenderer::new(context).expect("create renderer") };

        // Initialize GPU for EGL rendering
        let _ = gles.bind_wl_display(display_handle);

        // Create the DRM compositor.
        let (drm_compositor, connector) = Self::create_drm_compositor(display_handle, windows, &gles, &mut drm, &gbm)
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
        let token = event_loop.register_dispatcher(dispatcher.clone())?;

        // Create OpenGL textures.
        let graphics = Graphics::new();

        // Initialize last render state as empty.
        let last_render_states = RenderElementStates { states: HashMap::new() };

        Ok(OutputDevice {
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
        })
    }

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

        let mut drm_mode = if let Some(m) = found_mode {
            m
        } else {
             return Err("Mode not found".into());
        };

        // HACK: Inject YCbCr420_ONLY flag (0x8000) for 4K modes
        // This is required because some HDMI 2.0 sinks only support 4K@60Hz in YCbCr 4:2:0,
        // but the kernel/driver might not set this flag automatically or correctly.
        let (w, h) = drm_mode.size();
        if (w == 3840 && h == 2160) || (w == 4096 && h == 2160) {
             const DRM_MODE_FLAG_YCBCR420_ONLY: u32 = 0x8000;
             unsafe {
                  let ptr = &mut drm_mode as *mut _ as *mut u8;
                  // Offset 28 is flags in drm_mode_modeinfo struct (u32)
                  let flags_ptr = ptr.add(28) as *mut u32;
                  let current_flags = *flags_ptr;
                  
                  if (current_flags & DRM_MODE_FLAG_YCBCR420_ONLY) == 0 {
                      *flags_ptr = current_flags | DRM_MODE_FLAG_YCBCR420_ONLY;
                      tracing::info!("🔧 set_mode: Forced YCbCr420_ONLY (0x8000) for 4K mode. Old flags: 0x{:x}, New flags: 0x{:x}", current_flags, *flags_ptr);
                  }
             }
        }

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
    pub fn set_enabled(&mut self, enabled: bool) {
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
    pub fn render(
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
    pub fn default_dmabuf_feedback(
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

    /// Create the DRM compositor.
    fn create_drm_compositor(
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
                                for (idx, m) in modes.iter().take(10).enumerate() {
                                    // Smithay/DRM vrefresh is in Hz (u32)
                                    tracing::info!("      Mode[{}]: {}x{} @ {}Hz (Clock: {}kHz)", 
                                        idx, m.size().0, m.size().1, m.vrefresh(), m.clock());
                                }
                            }

                            // Try to log EDID presence and other properties
                            if let Ok(props) = drm.get_properties(c.handle()) {
                                let (handles, values) = props.as_props_and_values();
                                tracing::info!("      Connector Properties:");
                                for (i, handle) in handles.iter().enumerate() {
                                    if let Ok(info) = drm.get_property(*handle) {
                                        let name = info.name().to_str().unwrap_or("unknown");
                                        let value = values[i];
                                        tracing::info!("        - {}: {} (Raw: {})", name, value, u64::from(value));
                                        
                                        if name == "EDID" {
                                            tracing::info!("          ✅ EDID Blob detected.");
                                        }
                                    }
                                }
                            }
                        }

                        if state == ConnectorState::Connected {
                            // Check if modes are plausible (e.g. not just 640x480 safe mode)
                            // If we find 1080p+, great. If not, after 5 seconds (10 attempts), accept anything usable.
                            if max_width >= 1920 || (attempt > 10 && max_width >= 640) {
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

        // Try to optimize HDMI 2.0 bandwidth usage
        let connector_handle = connector.handle();
        if let Ok(props) = drm.get_properties(connector_handle) {
            let (handles, _values) = props.as_props_and_values();
            for (_i, handle) in handles.iter().enumerate() {
                if let Ok(info) = drm.get_property(*handle) {
                    let name = info.name().to_str().unwrap_or("unknown");
                    
                    if name == "Colorspace" {
                         // Force BT2020_YCC (10) to support 4K@60Hz YUV420 on HDMI 2.0
                         // User's working log confirmed this was set to 10.
                         let target_value = 10u64;
                         
                         if let Err(e) = drm.set_property(connector_handle, *handle, target_value.into()) {
                             tracing::warn!("Failed to set 'Colorspace' to 10: {:?}", e);
                         } else {
                             tracing::info!("✅ Set 'Colorspace' to 10 (BT2020_YCC)");
                         }
                    }

                    if name == "max bpc" {
                        // Limit to 8 bpc to save bandwidth for 4K@60Hz
                        if let Err(e) = drm.set_property(connector_handle, *handle, 8u64.into()) {
                             tracing::warn!("Failed to set 'max bpc': {:?}", e);
                        } else {
                             tracing::info!("✅ Set 'max bpc' to 8");
                        }
                    }
                }
            }
        }

        // Refresh connector to apply property changes
        let connector = match drm.get_connector(connector_handle, true) {
            Ok(c) => {
                tracing::info!("🔄 Refreshed connector info after setting properties");
                c
            },
            Err(e) => {
                tracing::warn!("Failed to refresh connector after setting properties: {:?}", e);
                connector // Fallback to old info
            }
        };

        let mut modes = connector.modes().to_vec();
        
        // HACK: Manually inject YCbCr420_ONLY flag (0x8000) for 4K modes if missing.
        for mode in modes.iter_mut() {
             let (w, h) = mode.size();
             // Check for 4K resolution (3840x2160 or 4096x2160)
             if (w == 3840 && h == 2160) || (w == 4096 && h == 2160) {
                 // Check if refresh rate is high enough (>= 50Hz) to require 4:2:0 on HDMI 2.0
                 // Note: vrefresh is in Hz (u32)
                 if mode.vrefresh() >= 50 {
                     const DRM_MODE_FLAG_YCBCR420_ONLY: u32 = 0x8000;
                     unsafe {
                          let ptr = mode as *mut _ as *mut u8;
                          // Offset 28 is flags in drm_mode_modeinfo struct (u32)
                          let flags_ptr = ptr.add(28) as *mut u32;
                          let current_flags = *flags_ptr;
                          
                          if (current_flags & DRM_MODE_FLAG_YCBCR420_ONLY) == 0 {
                              *flags_ptr = current_flags | DRM_MODE_FLAG_YCBCR420_ONLY;
                              tracing::info!("🔧 Forcing YCbCr420_ONLY (0x8000) for 4K mode: {}x{} @ {}Hz. Old flags: 0x{:x}, New flags: 0x{:x}", w, h, mode.vrefresh(), current_flags, *flags_ptr);
                          }
                     }
                 }
             }
        }
        
        // Try to preserve existing mode if the output matches.
        let output_name = format!("{:?}", connector.interface());
        let current_mode = windows.output.canvas().physical_resolution();
        let existing_name = windows.output.smithay_output().name();

        let preserved_mode = if existing_name == output_name {
            modes.iter().find(|m| {
                let (w, h) = m.size();
                w as i32 == current_mode.w && h as i32 == current_mode.h &&
                (m.vrefresh() as i32 * 1000) == windows.output.canvas().frame_interval().as_millis() as i32 * 1000 
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
            tracing::info!("  [#{}] {}x{} @ {}Hz (Clock: {}kHz, Flags: 0x{:x})", 
                i, m.size().0, m.size().1, m.vrefresh(), m.clock(), m.flags().bits());
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

            let (physical_width, physical_height) = connector.size().unwrap_or((0, 0));
            let output_name = format!("{:?}", connector.interface());
            
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
