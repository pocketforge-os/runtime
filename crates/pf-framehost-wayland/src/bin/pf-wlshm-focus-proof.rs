//! Minimal native-Wayland focus proof for the PocketForge Gamescope session.
//!
//! This deliberately has no renderer. It publishes one opaque `wl_shm` buffer
//! on an xdg-shell toplevel and then remains in the Wayland dispatch loop until
//! the service running it is stopped.

use std::{
    env,
    ffi::CString,
    fs::File,
    io::{self, Write},
    os::fd::{AsFd, FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::ExitCode,
};

use wayland_client::{
    delegate_noop,
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: i32 = 64;
const HEIGHT: i32 = 64;
const STRIDE: i32 = WIDTH * 4;
const BYTES: i32 = STRIDE * HEIGHT;

#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    socket: PathBuf,
}

fn parse_arguments<I>(arguments: I) -> Result<Arguments, String>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();
    let program = arguments
        .next()
        .unwrap_or_else(|| "pf-wlshm-focus-proof".to_owned());
    let mut socket = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--socket" => {
                if socket.is_some() {
                    return Err("--socket may be specified only once".to_owned());
                }
                socket =
                    Some(PathBuf::from(arguments.next().ok_or_else(|| {
                        "--socket requires an absolute path".to_owned()
                    })?));
            }
            "--help" | "-h" => {
                return Err(format!(
                    "usage: {program} --socket /absolute/wayland/socket"
                ));
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let socket = socket.ok_or_else(|| "--socket is required".to_owned())?;
    if !socket.is_absolute() {
        return Err("--socket requires an absolute path".to_owned());
    }
    Ok(Arguments { socket })
}

fn create_buffer_file() -> io::Result<(OwnedFd, File)> {
    let name = CString::new("pf-wlshm-focus-proof")
        .expect("the fixed memfd name does not contain a NUL byte");
    // SAFETY: `name` is a valid NUL-terminated string, and the returned file
    // descriptor is checked before ownership is transferred to `OwnedFd`.
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `memfd_create` returns a new descriptor owned here.
    let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    // SAFETY: `owned` is live and BYTES is a small, positive length.
    if unsafe { libc::ftruncate(raw_fd, i64::from(BYTES)) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = File::from(owned.try_clone()?);
    let pixel = [0x2a, 0x6a, 0xb8, 0xff];
    for _ in 0..(WIDTH * HEIGHT) {
        file.write_all(&pixel)?;
    }
    file.flush()?;
    Ok((owned, file))
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    pool: Option<wl_shm_pool::WlShmPool>,
    buffer: Option<wl_buffer::WlBuffer>,
    buffer_fd: Option<OwnedFd>,
    ready: bool,
    socket: PathBuf,
}

impl State {
    fn initialize(&mut self, queue: &QueueHandle<Self>) -> Result<(), String> {
        let compositor = self
            .compositor
            .as_ref()
            .ok_or_else(|| "wl_compositor global absent".to_owned())?;
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| "wl_shm global absent".to_owned())?;
        let wm_base = self
            .wm_base
            .as_ref()
            .ok_or_else(|| "xdg_wm_base global absent".to_owned())?;

        let surface = compositor.create_surface(queue, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, queue, ());
        let toplevel = xdg_surface.get_toplevel(queue, ());
        toplevel.set_title("PocketForge wl_shm focus proof".to_owned());
        toplevel.set_app_id("org.pocketforge.wlshm-proof".to_owned());

        let (buffer_fd, mut file) =
            create_buffer_file().map_err(|error| format!("wl_shm buffer file: {error}"))?;
        file.flush()
            .map_err(|error| format!("wl_shm buffer flush: {error}"))?;
        let pool = shm.create_pool(buffer_fd.as_fd(), BYTES, queue, ());
        let buffer = pool.create_buffer(
            0,
            WIDTH,
            HEIGHT,
            STRIDE,
            wl_shm::Format::Argb8888,
            queue,
            (),
        );

        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
        self.pool = Some(pool);
        self.buffer = Some(buffer);
        self.buffer_fd = Some(buffer_fd);
        Ok(())
    }

    fn publish(&mut self) {
        if self.ready {
            return;
        }
        let surface = self
            .surface
            .as_ref()
            .expect("configured xdg_surface has a wl_surface");
        let buffer = self
            .buffer
            .as_ref()
            .expect("configured xdg_surface has a wl_buffer");
        surface.attach(Some(buffer), 0, 0);
        surface.damage_buffer(0, 0, WIDTH, HEIGHT);
        surface.commit();
        self.ready = true;
        println!(
            "PF_WLSHM_FOCUS_PROOF ready socket={} compositor=bound shm=bound \
             surface=created buffer=wl_shm attach=done damage=done commit=done alive=dispatch",
            self.socket.display()
        );
        io::stdout().flush().expect("flush readiness line");
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" if state.compositor.is_none() && version >= 4 => {
                    state.compositor = Some(registry.bind(name, 4, queue, ()))
                }
                "wl_shm" if state.shm.is_none() => {
                    state.shm = Some(registry.bind(name, 1, queue, ()))
                }
                "xdg_wm_base" if state.wm_base.is_none() => {
                    state.wm_base = Some(registry.bind(name, 1, queue, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.publish();
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

fn run(arguments: Arguments) -> Result<(), String> {
    let stream = UnixStream::connect(&arguments.socket).map_err(|error| {
        format!(
            "connect public Wayland socket {}: {error}",
            arguments.socket.display()
        )
    })?;
    let connection = Connection::from_socket(stream)
        .map_err(|error| format!("initialize Wayland connection: {error}"))?;
    let mut event_queue = connection.new_event_queue();
    let queue = event_queue.handle();
    connection.display().get_registry(&queue, ());
    let mut state = State {
        socket: arguments.socket,
        ..State::default()
    };
    event_queue
        .roundtrip(&mut state)
        .map_err(|error| format!("Wayland registry roundtrip: {error}"))?;
    state.initialize(&queue)?;
    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .map_err(|error| format!("Wayland dispatch: {error}"))?;
    }
}

fn main() -> ExitCode {
    let result = parse_arguments(env::args()).and_then(run);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("PF_WLSHM_FOCUS_PROOF result=FAIL reason={error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn requires_one_absolute_socket() {
        assert_eq!(
            parse_arguments(arguments(&[
                "proof",
                "--socket",
                "/run/pocketforge/session/wayland-0"
            ])),
            Ok(Arguments {
                socket: Path::new("/run/pocketforge/session/wayland-0").to_owned()
            })
        );
        assert_eq!(
            parse_arguments(arguments(&["proof", "--socket", "wayland-0"])),
            Err("--socket requires an absolute path".to_owned())
        );
        assert_eq!(
            parse_arguments(arguments(&["proof"])),
            Err("--socket is required".to_owned())
        );
    }
}
