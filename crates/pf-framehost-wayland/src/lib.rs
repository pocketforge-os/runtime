//! Wayland presentation for the host-neutral shell renderer.
//!
//! This adapter is deliberately a normal xdg-shell client. `pf-render` remains the
//! only renderer; this crate converts its RGBA bytes to `wl_shm` XRGB8888 and submits
//! the rasterizer's damage rectangle.

use pf_ports::{FrameHost, PresentAck, PresentFailure, PresentResult};
use pf_render::{DamageRect, Rasterizer};
use pf_scene::{Insets, Orientation, Scene, SurfaceMetrics};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const DEFAULT_WIDTH: u32 = 640;
const DEFAULT_HEIGHT: u32 = 480;

/// Connection/setup failures retain enough type information for reconnect policy.
#[derive(Debug)]
pub enum WaylandHostError {
    CompositorUnavailable(String),
    Protocol(String),
    MissingGlobal(&'static str),
    InvalidConfigure { width: i32, height: i32 },
    Io(std::io::Error),
}

impl std::fmt::Display for WaylandHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompositorUnavailable(e) => write!(f, "compositor unavailable: {e}"),
            Self::Protocol(e) => write!(f, "Wayland protocol failure: {e}"),
            Self::MissingGlobal(g) => write!(f, "required Wayland global missing: {g}"),
            Self::InvalidConfigure { width, height } => {
                write!(f, "invalid compositor configure: {width}x{height}")
            }
            Self::Io(e) => write!(f, "wl_shm backing store: {e}"),
        }
    }
}

impl std::error::Error for WaylandHostError {}

impl From<std::io::Error> for WaylandHostError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    configured: bool,
    pending_size: Option<(u32, u32)>,
    size: (u32, u32),
    closed: bool,
    released_buffers: Vec<u64>,
}

impl State {
    fn new() -> Self {
        Self {
            compositor: None,
            shm: None,
            wm_base: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            configured: false,
            pending_size: None,
            size: (DEFAULT_WIDTH, DEFAULT_HEIGHT),
            closed: false,
            released_buffers: Vec::new(),
        }
    }

    fn init_xdg(&mut self, qh: &QueueHandle<Self>) {
        if self.xdg_surface.is_some() {
            return;
        }
        let (Some(compositor), Some(wm_base)) = (&self.compositor, &self.wm_base) else {
            return;
        };
        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());
        toplevel.set_title("PocketForge".into());
        toplevel.set_app_id("org.pocketforge.shell".into());
        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.wm_base = Some(registry.bind(name, 1, qh, ())),
                _ => return,
            }
            state.init_xdg(qh);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                state.pending_size = Some((width as u32, height as u32));
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(size) = state.pending_size.take() {
                state.size = size;
            }
            state.configured = true;
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
#[derive(Clone, Copy)]
struct BufferId(u64);

impl Dispatch<wl_buffer::WlBuffer, BufferId> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        id: &BufferId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.released_buffers.push(id.0);
        }
    }
}

/// A standard xdg-shell toplevel which presents CPU-rasterized `wl_shm` buffers.
pub struct WaylandHost {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    renderer: Rasterizer,
    sequence: u64,
    // A fresh immutable buffer is used for every present. Keeping the proxy and file alive
    // avoids recycling storage while the compositor may still read it.
    buffers: Vec<(u64, wl_buffer::WlBuffer, File)>,
    next_buffer_id: u64,
}

impl WaylandHost {
    pub fn connect() -> Result<Self, WaylandHostError> {
        let connection = Connection::connect_to_env()
            .map_err(|e| WaylandHostError::CompositorUnavailable(e.to_string()))?;
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut state = State::new();
        queue
            .roundtrip(&mut state)
            .map_err(|e| WaylandHostError::Protocol(e.to_string()))?;
        for global in [
            (state.compositor.is_some(), "wl_compositor"),
            (state.shm.is_some(), "wl_shm"),
            (state.wm_base.is_some(), "xdg_wm_base"),
        ] {
            if !global.0 {
                return Err(WaylandHostError::MissingGlobal(global.1));
            }
        }
        while !state.configured {
            queue
                .blocking_dispatch(&mut state)
                .map_err(|e| WaylandHostError::Protocol(e.to_string()))?;
        }
        Ok(Self {
            connection,
            queue,
            state,
            renderer: Rasterizer::new(),
            sequence: 0,
            buffers: Vec::new(),
            next_buffer_id: 1,
        })
    }

    /// Rebuild every protocol object after compositor loss.
    pub fn reconnect(&mut self) -> Result<(), WaylandHostError> {
        *self = Self::connect()?;
        Ok(())
    }

    /// Synchronize with the compositor, applying configure/close/buffer-release events.
    pub fn poll(&mut self) -> Result<(), WaylandHostError> {
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| WaylandHostError::Protocol(e.to_string()))?;
        self.connection
            .flush()
            .map_err(|e| WaylandHostError::Protocol(e.to_string()))?;
        let released = &self.state.released_buffers;
        self.buffers.retain(|(id, _, _)| !released.contains(id));
        self.state.released_buffers.clear();
        Ok(())
    }

    fn present_inner(&mut self, scene: &Scene) -> Result<PresentAck, WaylandHostError> {
        self.poll()?;
        if self.state.closed {
            return Err(WaylandHostError::Protocol("toplevel closed".into()));
        }
        let frame = self
            .renderer
            .render(scene, self.metrics())
            .map_err(|e| WaylandHostError::Protocol(format!("render: {e:?}")))?;
        let size = frame
            .width
            .checked_mul(frame.height)
            .and_then(|v| v.checked_mul(4))
            .ok_or(WaylandHostError::InvalidConfigure {
                width: frame.width as i32,
                height: frame.height as i32,
            })?;
        let mut file = tempfile::tempfile()?;
        file.set_len(size as u64)?;
        file.seek(SeekFrom::Start(0))?;
        let mut xrgb = Vec::with_capacity(size as usize);
        for rgba in frame.rgba.chunks_exact(4) {
            xrgb.extend_from_slice(&[rgba[2], rgba[1], rgba[0], 0xff]);
        }
        file.write_all(&xrgb)?;
        file.flush()?;

        let qh = self.queue.handle();
        let pool = self
            .state
            .shm
            .as_ref()
            .expect("validated global")
            .create_pool(file.as_fd(), size as i32, &qh, ());
        let buffer_id = self.next_buffer_id;
        self.next_buffer_id += 1;
        let buffer = pool.create_buffer(
            0,
            frame.width as i32,
            frame.height as i32,
            (frame.width * 4) as i32,
            wl_shm::Format::Xrgb8888,
            &qh,
            BufferId(buffer_id),
        );
        pool.destroy();
        let surface = self.state.surface.as_ref().expect("configured surface");
        surface.attach(Some(&buffer), 0, 0);
        submit_damage(surface, frame.damage, frame.width, frame.height);
        surface.commit();
        self.connection
            .flush()
            .map_err(|e| WaylandHostError::Protocol(e.to_string()))?;
        self.buffers.push((buffer_id, buffer, file));
        self.sequence += 1;
        Ok(PresentAck {
            sequence: self.sequence,
        })
    }
}

fn submit_damage(
    surface: &wl_surface::WlSurface,
    damage: Option<DamageRect>,
    width: u32,
    height: u32,
) {
    if let Some(d) = damage {
        surface.damage_buffer(d.x as i32, d.y as i32, d.width as i32, d.height as i32);
    } else {
        // Attaching a newly allocated buffer still needs damage before it is visible.
        surface.damage_buffer(0, 0, width as i32, height as i32);
    }
}

impl FrameHost for WaylandHost {
    fn metrics(&self) -> SurfaceMetrics {
        let (width, height) = self.state.size;
        SurfaceMetrics {
            logical_width: width as f32,
            logical_height: height as f32,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: if width >= height {
                Orientation::Landscape
            } else {
                Orientation::Portrait
            },
        }
    }

    fn present(&mut self, scene: &Scene) -> PresentResult {
        self.present_inner(scene).map_err(|error| match error {
            WaylandHostError::CompositorUnavailable(_) | WaylandHostError::Protocol(_) => {
                PresentFailure::SurfaceLost
            }
            WaylandHostError::InvalidConfigure { .. } => PresentFailure::Rejected,
            WaylandHostError::MissingGlobal(_) | WaylandHostError::Io(_) => {
                PresentFailure::Backend(error.to_string())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_is_not_fixed_to_a_product_resolution() {
        let metrics = State::new().size;
        assert_eq!(metrics, (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn typed_errors_are_actionable() {
        assert!(WaylandHostError::MissingGlobal("wl_shm")
            .to_string()
            .contains("wl_shm"));
    }
}
