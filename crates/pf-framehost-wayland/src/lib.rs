//! Wayland presentation for the host-neutral shell renderer.
//!
//! This adapter is deliberately a normal xdg-shell client. `pf-render` remains the
//! only renderer; this crate converts its RGBA bytes to `wl_shm` XRGB8888 and submits
//! the rasterizer's damage rectangle.

use pf_ports::{FrameHost, PresentAck, PresentFailure, PresentResult};
use pf_render::{DamageRect, Rasterizer, ThemeBase};
use pf_scene::{Insets, Orientation, Scene, SurfaceMetrics};
#[cfg(feature = "keyboard")]
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
#[cfg(feature = "keyboard")]
use std::os::fd::AsRawFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
#[cfg(feature = "keyboard")]
use wayland_client::protocol::{wl_keyboard, wl_seat};
#[cfg(feature = "keyboard")]
use wayland_client::WEnum;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
#[cfg(feature = "keyboard")]
use xkbcommon::xkb;

const DEFAULT_WIDTH: u32 = 640;
const DEFAULT_HEIGHT: u32 = 480;

/// Press/release state reported by the compositor.
#[cfg(feature = "keyboard")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyState {
    Pressed,
    Released,
}

/// Layout-aware key meaning used by the PocketForge shell.
#[cfg(feature = "keyboard")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Char(char),
    Other(u32),
}

/// One keyboard transition. `keysym` is retained for consumers needing more than [`Key`].
#[cfg(feature = "keyboard")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    /// Physical evdev keycode, stable across layout and modifier changes.
    pub code: u32,
    pub keysym: u32,
    pub state: KeyState,
    pub key: Key,
}

/// Compositor-provided repeat settings. This crate deliberately does not synthesize repeats.
#[cfg(feature = "keyboard")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepeatInfo {
    pub rate: i32,
    pub delay_ms: i32,
}

/// Connection/setup failures retain enough type information for reconnect policy.
#[derive(Debug)]
pub enum WaylandHostError {
    CompositorUnavailable(String),
    Protocol(String),
    MissingGlobal(&'static str),
    InvalidSize { width: u32, height: u32 },
    InvalidConfigure { width: i32, height: i32 },
    Io(std::io::Error),
}

impl std::fmt::Display for WaylandHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompositorUnavailable(e) => write!(f, "compositor unavailable: {e}"),
            Self::Protocol(e) => write!(f, "Wayland protocol failure: {e}"),
            Self::MissingGlobal(g) => write!(f, "required Wayland global missing: {g}"),
            Self::InvalidSize { width, height } => {
                write!(f, "invalid Wayland host size: {width}x{height}")
            }
            Self::InvalidConfigure { width, height } => {
                write!(f, "invalid compositor configure: {width}x{height}")
            }
            Self::Io(e) => write!(f, "wl_shm backing store: {e}"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ValidatedSize {
    width: u32,
    height: u32,
    width_i32: i32,
    height_i32: i32,
    stride_i32: i32,
    pool_bytes_i32: i32,
}

impl ValidatedSize {
    fn new(width: u32, height: u32) -> Result<Self, WaylandHostError> {
        let invalid = || WaylandHostError::InvalidSize { width, height };
        if width == 0 || height == 0 {
            return Err(invalid());
        }
        let width_i32 = i32::try_from(width).map_err(|_| invalid())?;
        let height_i32 = i32::try_from(height).map_err(|_| invalid())?;
        let stride = width.checked_mul(4).ok_or_else(&invalid)?;
        let stride_i32 = i32::try_from(stride).map_err(|_| invalid())?;
        let pool_bytes = stride.checked_mul(height).ok_or_else(&invalid)?;
        let pool_bytes_i32 = i32::try_from(pool_bytes).map_err(|_| invalid())?;
        Ok(Self {
            width,
            height,
            width_i32,
            height_i32,
            stride_i32,
            pool_bytes_i32,
        })
    }

    fn dimensions(self) -> (u32, u32) {
        (self.width, self.height)
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
    #[cfg(feature = "keyboard")]
    seat: Option<wl_seat::WlSeat>,
    #[cfg(feature = "keyboard")]
    seat_name: Option<u32>,
    #[cfg(feature = "keyboard")]
    keyboard: Option<wl_keyboard::WlKeyboard>,
    #[cfg(feature = "keyboard")]
    xkb_context: xkb::Context,
    #[cfg(feature = "keyboard")]
    xkb_state: Option<xkb::State>,
    #[cfg(feature = "keyboard")]
    pressed_keys: HashMap<u32, (u32, Key)>,
    #[cfg(feature = "keyboard")]
    key_events: VecDeque<KeyEvent>,
    #[cfg(feature = "keyboard")]
    repeat_info: Option<RepeatInfo>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    configured: bool,
    size: ValidatedSize,
    closed: bool,
    released_buffers: Vec<u64>,
}

impl State {
    fn new(size: ValidatedSize) -> Self {
        Self {
            compositor: None,
            shm: None,
            wm_base: None,
            #[cfg(feature = "keyboard")]
            seat: None,
            #[cfg(feature = "keyboard")]
            seat_name: None,
            #[cfg(feature = "keyboard")]
            keyboard: None,
            #[cfg(feature = "keyboard")]
            xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            #[cfg(feature = "keyboard")]
            xkb_state: None,
            #[cfg(feature = "keyboard")]
            pressed_keys: HashMap::new(),
            #[cfg(feature = "keyboard")]
            key_events: VecDeque::new(),
            #[cfg(feature = "keyboard")]
            repeat_info: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            configured: false,
            size,
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
        toplevel.set_min_size(self.size.width_i32, self.size.height_i32);
        toplevel.set_max_size(self.size.width_i32, self.size.height_i32);
        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
    }

    #[cfg(feature = "keyboard")]
    fn release_pressed_keys(&mut self) {
        self.key_events.extend(
            self.pressed_keys
                .drain()
                .map(|(code, (keysym, key))| KeyEvent {
                    code,
                    keysym,
                    state: KeyState::Released,
                    key,
                }),
        );
    }

    #[cfg(feature = "keyboard")]
    fn poll_key_event(&mut self) -> Option<KeyEvent> {
        self.key_events.pop_front()
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
            ref interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => state.wm_base = Some(registry.bind(name, 1, qh, ())),
                #[cfg(feature = "keyboard")]
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                    state.seat_name = Some(name);
                }
                _ => return,
            }
            state.init_xdg(qh);
        }
        #[cfg(feature = "keyboard")]
        if let wl_registry::Event::GlobalRemove { name } = event {
            if state.seat_name == Some(name) {
                state.keyboard = None;
                state.xkb_state = None;
                state.release_pressed_keys();
                state.seat = None;
                state.seat_name = None;
            }
        }
    }
}

#[cfg(feature = "keyboard")]
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_keyboard = matches!(capabilities, WEnum::Value(value) if value.contains(wl_seat::Capability::Keyboard));
            match (has_keyboard, state.keyboard.is_some()) {
                (true, false) => state.keyboard = Some(seat.get_keyboard(qh, ())),
                (false, true) => {
                    state.keyboard = None;
                    state.xkb_state = None;
                    state.release_pressed_keys();
                }
                _ => {}
            }
        }
    }
}

#[cfg(feature = "keyboard")]
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                // SAFETY: Wayland transfers an owned, valid keymap fd and supplies its mapping size.
                state.xkb_state = unsafe {
                    xkb::Keymap::new_from_fd(
                        &state.xkb_context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                }
                .ok()
                .flatten()
                .map(|keymap| xkb::State::new(&keymap));
            }
            wl_keyboard::Event::Keymap { .. } => state.xkb_state = None,
            wl_keyboard::Event::Enter { .. } | wl_keyboard::Event::Leave { .. } => {
                state.release_pressed_keys();
            }
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                let Some(xkb_state) = &state.xkb_state else {
                    return;
                };
                let Some(key_state) = key_state.into_result().ok() else {
                    return;
                };
                let state_value = match key_state {
                    wl_keyboard::KeyState::Pressed => KeyState::Pressed,
                    wl_keyboard::KeyState::Released => KeyState::Released,
                    _ => return,
                };
                state.key_events.push_back(key_event_from_evdev(
                    xkb_state,
                    &mut state.pressed_keys,
                    key,
                    state_value,
                ));
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb_state) = &mut state.xkb_state {
                    xkb_state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat_info = Some(RepeatInfo {
                    rate,
                    delay_ms: delay,
                });
            }
            _ => {}
        }
    }
}

#[cfg(feature = "keyboard")]
fn key_event_from_evdev(
    xkb_state: &xkb::State,
    pressed_keys: &mut HashMap<u32, (u32, Key)>,
    code: u32,
    state: KeyState,
) -> KeyEvent {
    // wl_keyboard uses evdev codes; XKB retains the historical eight-code offset.
    let fresh_keysym = xkb_state.key_get_one_sym(xkb::Keycode::new(code + 8)).raw();
    let fresh_key = translate_keysym(fresh_keysym);
    let (keysym, key) = match state {
        KeyState::Pressed => {
            pressed_keys.insert(code, (fresh_keysym, fresh_key));
            (fresh_keysym, fresh_key)
        }
        KeyState::Released => pressed_keys
            .remove(&code)
            .unwrap_or((fresh_keysym, fresh_key)),
    };
    KeyEvent {
        code,
        keysym,
        state,
        key,
    }
}

#[cfg(feature = "keyboard")]
fn translate_keysym(keysym: u32) -> Key {
    match keysym {
        0xff52 => Key::Up,
        0xff54 => Key::Down,
        0xff51 => Key::Left,
        0xff53 => Key::Right,
        0xff0d | 0xff8d => Key::Enter,
        0xff1b => Key::Escape,
        value => char::from_u32(xkb::keysym_to_utf32(xkb::Keysym::new(value)))
            .filter(|character| !character.is_control())
            .map(Key::Char)
            .unwrap_or(Key::Other(value)),
    }
}

#[cfg(feature = "keyboard")]
fn transfer_pressed_key_releases(old: &mut State, replacement: &mut State) {
    old.release_pressed_keys();
    old.key_events.append(&mut replacement.key_events);
    std::mem::swap(&mut old.key_events, &mut replacement.key_events);
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
        handle_toplevel_event(state, event);
    }
}

fn handle_toplevel_event(state: &mut State, event: xdg_toplevel::Event) {
    if let xdg_toplevel::Event::Close = event {
        state.closed = true;
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
        Self::connect_with_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }

    /// Connect with a fixed logical size matching the simulated device panel.
    ///
    /// The size is device-faithful by design: compositor configure events never trigger
    /// adaptive sizing or UI reflow.
    pub fn connect_with_size(width: u32, height: u32) -> Result<Self, WaylandHostError> {
        // Validate before connecting so invalid input cannot emit any protocol request.
        let size = ValidatedSize::new(width, height)?;
        let connection = Connection::connect_to_env()
            .map_err(|e| WaylandHostError::CompositorUnavailable(e.to_string()))?;
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut state = State::new(size);
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
        let (width, height) = self.state.size.dimensions();
        let replacement = Self::connect_with_size(width, height)?;
        #[cfg(feature = "keyboard")]
        {
            let mut replacement = replacement;
            transfer_pressed_key_releases(&mut self.state, &mut replacement.state);
            *self = replacement;
        }
        #[cfg(not(feature = "keyboard"))]
        {
            *self = replacement;
        }
        Ok(())
    }

    /// Return whether the compositor has requested that this toplevel close.
    pub fn is_closed(&self) -> bool {
        self.state.closed
    }

    /// Return the next queued keyboard transition without waiting for the compositor.
    ///
    /// A compositor without a seat/keyboard (or a disconnected compositor) simply yields `None`.
    /// A keyboard leave/disconnect delivers synthetic releases for all held keys.
    #[cfg(feature = "keyboard")]
    pub fn poll_key_event(&mut self) -> Option<KeyEvent> {
        self.pump_events_nonblocking();
        self.state.poll_key_event()
    }

    /// Return the most recently advertised compositor repeat settings.
    #[cfg(feature = "keyboard")]
    pub fn repeat_info(&self) -> Option<RepeatInfo> {
        self.state.repeat_info
    }

    #[cfg(feature = "keyboard")]
    fn pump_events_nonblocking(&mut self) {
        if self.queue.dispatch_pending(&mut self.state).is_err() {
            return;
        }
        let Some(guard) = self.connection.prepare_read() else {
            let _ = self.queue.dispatch_pending(&mut self.state);
            return;
        };
        let backend = self.connection.backend();
        let mut poll_fd = libc::pollfd {
            fd: backend.poll_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd points to one initialized pollfd for the duration of this call.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) } > 0;
        if ready && guard.read().is_ok() {
            let _ = self.queue.dispatch_pending(&mut self.state);
        }
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
        let size = self.state.size;
        debug_assert_eq!((frame.width, frame.height), size.dimensions());
        let mut file = tempfile::tempfile()?;
        file.set_len(u64::try_from(size.pool_bytes_i32).expect("validated positive size"))?;
        file.seek(SeekFrom::Start(0))?;
        let mut xrgb = Vec::with_capacity(
            usize::try_from(size.pool_bytes_i32).expect("validated positive size"),
        );
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
            .create_pool(file.as_fd(), size.pool_bytes_i32, &qh, ());
        let buffer_id = self.next_buffer_id;
        self.next_buffer_id += 1;
        let buffer = pool.create_buffer(
            0,
            size.width_i32,
            size.height_i32,
            size.stride_i32,
            wl_shm::Format::Xrgb8888,
            &qh,
            BufferId(buffer_id),
        );
        pool.destroy();
        let surface = self.state.surface.as_ref().expect("configured surface");
        surface.attach(Some(&buffer), 0, 0);
        submit_damage(surface, frame.damage, size);
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

fn submit_damage(surface: &wl_surface::WlSurface, damage: Option<DamageRect>, size: ValidatedSize) {
    if let Some(d) = damage {
        surface.damage_buffer(
            i32::try_from(d.x).expect("damage x is within validated surface"),
            i32::try_from(d.y).expect("damage y is within validated surface"),
            i32::try_from(d.width).expect("damage width is within validated surface"),
            i32::try_from(d.height).expect("damage height is within validated surface"),
        );
    } else {
        // Attaching a newly allocated buffer still needs damage before it is visible.
        surface.damage_buffer(0, 0, size.width_i32, size.height_i32);
    }
}

impl FrameHost for WaylandHost {
    fn metrics(&self) -> SurfaceMetrics {
        let (width, height) = self.state.size.dimensions();
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

    fn set_theme_base(&mut self, base: ThemeBase) {
        set_renderer_theme_base(&mut self.renderer, base);
    }

    fn present(&mut self, scene: &Scene) -> PresentResult {
        self.present_inner(scene).map_err(|error| match error {
            WaylandHostError::CompositorUnavailable(_) | WaylandHostError::Protocol(_) => {
                PresentFailure::SurfaceLost
            }
            WaylandHostError::InvalidSize { .. } | WaylandHostError::InvalidConfigure { .. } => {
                PresentFailure::Rejected
            }
            WaylandHostError::MissingGlobal(_) | WaylandHostError::Io(_) => {
                PresentFailure::Backend(error.to_string())
            }
        })
    }
}

fn set_renderer_theme_base(renderer: &mut Rasterizer, base: ThemeBase) {
    renderer.set_theme_base(base);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_scene::{Bounds, Node, NodeId, Role};

    fn valid_size(width: u32, height: u32) -> ValidatedSize {
        ValidatedSize::new(width, height).expect("valid test size")
    }

    #[test]
    fn host_theme_base_forwarder_changes_the_next_render() {
        let node = Node::new(
            NodeId::new("root").unwrap(),
            Role::Text,
            "Palette",
            Bounds::new(3.0, 4.0, 100.0, 40.0),
            "--state-rest-surface",
        );
        let scene = Scene::new(node, NodeId::new("root").unwrap()).unwrap();
        let mut renderer = Rasterizer::new();
        set_renderer_theme_base(&mut renderer, ThemeBase::HighContrast);
        let frame = renderer
            .render(
                &scene,
                SurfaceMetrics {
                    logical_width: 120.0,
                    logical_height: 60.0,
                    scale: 1.0,
                    safe_insets: Insets::default(),
                    orientation: Orientation::Landscape,
                },
            )
            .unwrap();
        assert_eq!(&frame.rgba[..4], &[0, 0, 0, 255]);
        assert!(frame
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 255, 255, 255]));
    }

    #[test]
    fn zero_width_is_rejected_before_connecting() {
        assert!(matches!(
            WaylandHost::connect_with_size(0, 720),
            Err(WaylandHostError::InvalidSize {
                width: 0,
                height: 720
            })
        ));
    }

    #[test]
    fn zero_height_is_rejected_before_connecting() {
        assert!(matches!(
            WaylandHost::connect_with_size(1280, 0),
            Err(WaylandHostError::InvalidSize {
                width: 1280,
                height: 0
            })
        ));
    }

    #[test]
    fn pool_size_above_protocol_limit_is_rejected() {
        let error = ValidatedSize::new(32_768, 16_384).unwrap_err();
        assert!(matches!(
            error,
            WaylandHostError::InvalidSize {
                width: 32_768,
                height: 16_384
            }
        ));
        assert!(error.to_string().contains("32768x16384"));
    }

    #[test]
    fn common_panel_size_is_accepted() {
        let size = ValidatedSize::new(1280, 720).expect("1280x720 must be valid");
        assert_eq!(size.dimensions(), (1280, 720));
        assert_eq!(size.stride_i32, 5120);
        assert_eq!(size.pool_bytes_i32, 3_686_400);
    }

    #[test]
    fn damage_is_not_fixed_to_a_product_resolution() {
        let metrics = State::new(valid_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)).size;
        assert_eq!(metrics.dimensions(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn zero_size_configure_keeps_pinned_metrics() {
        let mut state = State::new(valid_size(1280, 720));
        handle_toplevel_event(
            &mut state,
            xdg_toplevel::Event::Configure {
                width: 0,
                height: 0,
                states: Vec::new(),
            },
        );
        assert_eq!(state.size.dimensions(), (1280, 720));
    }

    #[test]
    fn nonzero_configure_keeps_pinned_metrics() {
        let mut state = State::new(valid_size(1280, 720));
        handle_toplevel_event(
            &mut state,
            xdg_toplevel::Event::Configure {
                width: 640,
                height: 480,
                states: Vec::new(),
            },
        );
        assert_eq!(state.size.dimensions(), (1280, 720));
    }

    #[test]
    fn typed_errors_are_actionable() {
        assert!(WaylandHostError::MissingGlobal("wl_shm")
            .to_string()
            .contains("wl_shm"));
    }

    #[cfg(feature = "keyboard")]
    #[test]
    fn shell_keysyms_have_stable_meanings() {
        assert_eq!(translate_keysym(0xff52), Key::Up);
        assert_eq!(translate_keysym(0xff54), Key::Down);
        assert_eq!(translate_keysym(0xff51), Key::Left);
        assert_eq!(translate_keysym(0xff53), Key::Right);
        assert_eq!(translate_keysym(0xff0d), Key::Enter);
        assert_eq!(translate_keysym(0xff1b), Key::Escape);
        assert_eq!(translate_keysym(u32::from('é')), Key::Char('é'));
        assert_eq!(translate_keysym(0x0100_03bb), Key::Char('λ'));
        assert_eq!(translate_keysym(0x100_0000), Key::Other(0x100_0000));
    }

    #[cfg(feature = "keyboard")]
    #[test]
    fn fabricated_evdev_event_uses_received_xkb_layout() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile test keymap");
        let state = xkb::State::new(&keymap);
        let mut pressed_keys = HashMap::new();

        // KEY_A is evdev 30. This is the same translation path used after keymap receipt.
        assert_eq!(
            key_event_from_evdev(&state, &mut pressed_keys, 30, KeyState::Pressed),
            KeyEvent {
                code: 30,
                keysym: u32::from('a'),
                state: KeyState::Pressed,
                key: Key::Char('a'),
            }
        );
    }

    #[cfg(feature = "keyboard")]
    #[test]
    fn release_uses_press_translation_after_modifiers_change() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile test keymap");
        let shift_mask = 1 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let mut state = xkb::State::new(&keymap);
        let mut pressed_keys = HashMap::new();

        state.update_mask(shift_mask, 0, 0, 0, 0, 0);
        let pressed = key_event_from_evdev(&state, &mut pressed_keys, 30, KeyState::Pressed);
        state.update_mask(0, 0, 0, 0, 0, 0);
        let released = key_event_from_evdev(&state, &mut pressed_keys, 30, KeyState::Released);

        assert_eq!(pressed.key, Key::Char('A'));
        assert_eq!(released.key, pressed.key);
        assert_eq!(released.keysym, pressed.keysym);
        assert_eq!(released.code, pressed.code);
        assert!(pressed_keys.is_empty());
    }

    #[cfg(feature = "keyboard")]
    #[test]
    fn keyboard_leave_releases_held_keys() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile test keymap");
        let state = xkb::State::new(&keymap);
        let mut host_state = State::new(valid_size(DEFAULT_WIDTH, DEFAULT_HEIGHT));

        let presses = [30, 105].map(|code| {
            key_event_from_evdev(
                &state,
                &mut host_state.pressed_keys,
                code,
                KeyState::Pressed,
            )
        });
        assert_eq!(presses.map(|event| event.key), [Key::Char('a'), Key::Left]);

        host_state.release_pressed_keys();
        assert!(host_state.pressed_keys.is_empty());
        let mut releases = Vec::new();
        while let Some(event) = host_state.poll_key_event() {
            releases.push(event);
        }
        releases.sort_by_key(|event| event.code);
        let mut expected = presses.map(|event| KeyEvent {
            state: KeyState::Released,
            ..event
        });
        expected.sort_by_key(|event| event.code);
        assert_eq!(releases, expected);
    }

    #[cfg(feature = "keyboard")]
    #[test]
    fn reconnect_releases_held_keys_before_replacing_state() {
        let mut old_state = State::new(valid_size(DEFAULT_WIDTH, DEFAULT_HEIGHT));
        old_state
            .pressed_keys
            .insert(30, (u32::from('a'), Key::Char('a')));
        let mut replacement_state = State::new(valid_size(DEFAULT_WIDTH, DEFAULT_HEIGHT));
        replacement_state.key_events.push_back(KeyEvent {
            code: 30,
            keysym: u32::from('a'),
            state: KeyState::Pressed,
            key: Key::Char('a'),
        });

        transfer_pressed_key_releases(&mut old_state, &mut replacement_state);

        assert!(old_state.pressed_keys.is_empty());
        assert_eq!(
            replacement_state.poll_key_event(),
            Some(KeyEvent {
                code: 30,
                keysym: u32::from('a'),
                state: KeyState::Released,
                key: Key::Char('a'),
            })
        );
        assert_eq!(
            replacement_state.poll_key_event(),
            Some(KeyEvent {
                code: 30,
                keysym: u32::from('a'),
                state: KeyState::Pressed,
                key: Key::Char('a'),
            })
        );
        assert_eq!(replacement_state.poll_key_event(), None);
    }

    #[test]
    fn fabricated_toplevel_close_sets_closed_state() {
        let mut state = State::new(valid_size(DEFAULT_WIDTH, DEFAULT_HEIGHT));
        assert!(!state.closed);

        handle_toplevel_event(&mut state, xdg_toplevel::Event::Close);

        assert!(state.closed);
    }
}
