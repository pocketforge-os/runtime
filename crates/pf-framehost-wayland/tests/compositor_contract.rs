use pf_framehost_wayland::{
    BufferTransform, Key, KeyEvent, KeyState, WaylandHost, WaylandHostError,
};
use pf_ports::{FrameHost, PresentFailure};
use pf_render::Rasterizer;
use pf_scene::{Bounds, Node, NodeId, Role, Scene, SurfaceMetrics};
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wayland_protocols::xdg::shell::server::{
    xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_server::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_region, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};
use xkbcommon::xkb;

type DamageObservation = (i32, i32, i32, i32);

#[derive(Clone, Debug, Default)]
struct FrameObservation {
    width: i32,
    height: i32,
    stride: i32,
    transform: Option<wl_output::Transform>,
    damage: Option<DamageObservation>,
    xrgb: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct Observations {
    min_size: Option<(i32, i32)>,
    max_size: Option<(i32, i32)>,
    frames: Vec<FrameObservation>,
}

#[derive(Clone)]
struct PoolData {
    file: Arc<File>,
}

#[derive(Clone)]
struct BufferData {
    file: Arc<File>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
}

struct CompositorState {
    observations: Arc<Mutex<Observations>>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    surface: Option<wl_surface::WlSurface>,
    attached: Option<wl_buffer::WlBuffer>,
    transform: Option<wl_output::Transform>,
    damage: Option<DamageObservation>,
    configured: bool,
    keymap: String,
}

impl CompositorState {
    fn new(observations: Arc<Mutex<Observations>>) -> Self {
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
        .expect("compile fixture keymap")
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        Self {
            observations,
            xdg_surface: None,
            toplevel: None,
            surface: None,
            attached: None,
            transform: None,
            damage: None,
            configured: false,
            keymap,
        }
    }

    fn configure_surface(&mut self) {
        if self.configured {
            return;
        }
        let (Some(toplevel), Some(xdg_surface)) = (&self.toplevel, &self.xdg_surface) else {
            return;
        };
        toplevel.configure(0, 0, Vec::new());
        xdg_surface.configure(1);
        self.configured = true;
    }

    fn record_frame(&mut self) {
        let Some(buffer) = self.attached.take() else {
            return;
        };
        let data = buffer.data::<BufferData>().expect("fixture buffer data");
        let byte_count = usize::try_from(data.stride * data.height).expect("fixture buffer size");
        let mut xrgb = vec![0; byte_count];
        data.file
            .read_exact_at(
                &mut xrgb,
                u64::try_from(data.offset).expect("fixture buffer offset"),
            )
            .expect("read submitted wl_shm buffer");
        self.observations
            .lock()
            .expect("observations lock")
            .frames
            .push(FrameObservation {
                width: data.width,
                height: data.height,
                stride: data.stride,
                transform: self.transform,
                damage: self.damage.take(),
                xrgb,
            });
    }

    fn send_keyboard_fixture(&self, keyboard: &wl_keyboard::WlKeyboard) {
        let mut keymap = self.keymap.clone();
        keymap.push('\0');
        let mut keymap_file = tempfile::tempfile().expect("fixture keymap file");
        std::io::Write::write_all(&mut keymap_file, keymap.as_bytes())
            .expect("write fixture keymap");
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1,
            keymap_file.as_fd(),
            u32::try_from(keymap.len()).expect("keymap size"),
        );
        if let Some(surface) = &self.surface {
            keyboard.enter(2, surface, Vec::new());
        }
        keyboard.repeat_info(25, 400);
        keyboard.key(3, 1, 30, wl_keyboard::KeyState::Pressed);
        keyboard.key(4, 2, 30, wl_keyboard::KeyState::Released);
    }
}

macro_rules! global {
    ($interface:ty) => {
        impl GlobalDispatch<$interface, ()> for CompositorState {
            fn bind(
                _state: &mut Self,
                _handle: &DisplayHandle,
                _client: &Client,
                resource: New<$interface>,
                _global_data: &(),
                data_init: &mut DataInit<'_, Self>,
            ) {
                data_init.init(resource, ());
            }
        }
    };
}

global!(wl_compositor::WlCompositor);
global!(wl_shm::WlShm);
global!(xdg_wm_base::XdgWmBase);

impl GlobalDispatch<wl_seat::WlSeat, ()> for CompositorState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_seat::WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        seat.name("contract-seat".to_owned());
        seat.capabilities(wl_seat::Capability::Keyboard);
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                data_init.init(id, ());
            }
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_region::WlRegion,
        _request: wl_region::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => state.attached = buffer,
            wl_surface::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => state.damage = Some((x, y, width, height)),
            wl_surface::Request::SetBufferTransform { transform } => {
                state.transform = transform.into_result().ok();
            }
            wl_surface::Request::Commit => {
                state.surface = Some(surface.clone());
                state.configure_surface();
                state.record_frame();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_shm::WlShm,
        request: wl_shm::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, .. } = request {
            data_init.init(
                id,
                PoolData {
                    file: Arc::new(File::from(fd)),
                },
            );
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, PoolData> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &PoolData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm_pool::Request::CreateBuffer {
            id,
            offset,
            width,
            height,
            stride,
            ..
        } = request
        {
            data_init.init(
                id,
                BufferData {
                    file: data.file.clone(),
                    offset,
                    width,
                    height,
                    stride,
                },
            );
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::CreatePositioner { id } => {
                data_init.init(id, ());
            }
            xdg_wm_base::Request::GetXdgSurface { id, .. } => {
                state.xdg_surface = Some(data_init.init(id, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_positioner::XdgPositioner,
        _request: xdg_positioner::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_surface::Request::GetToplevel { id } = request {
            state.toplevel = Some(data_init.init(id, ()));
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut observations = state.observations.lock().expect("observations lock");
        match request {
            xdg_toplevel::Request::SetMinSize { width, height } => {
                observations.min_size = Some((width, height));
            }
            xdg_toplevel::Request::SetMaxSize { width, height } => {
                observations.max_size = Some((width, height));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for CompositorState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_seat::Request::GetKeyboard { id } = request {
            let keyboard = data_init.init(id, ());
            state.send_keyboard_fixture(&keyboard);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for CompositorState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_keyboard::WlKeyboard,
        _request: wl_keyboard::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

struct TestCompositor {
    socket_path: PathBuf,
    observations: Arc<Mutex<Observations>>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestCompositor {
    fn start(socket_path: PathBuf) -> Self {
        let listener = UnixListener::bind(&socket_path).expect("bind compositor fixture socket");
        listener
            .set_nonblocking(true)
            .expect("set fixture listener nonblocking");
        let observations = Arc::new(Mutex::new(Observations::default()));
        let thread_observations = observations.clone();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread = thread::spawn(move || {
            let mut display = Display::<CompositorState>::new().expect("create fixture display");
            let handle = display.handle();
            handle.create_global::<CompositorState, wl_compositor::WlCompositor, _>(4, ());
            handle.create_global::<CompositorState, wl_shm::WlShm, _>(1, ());
            handle.create_global::<CompositorState, xdg_wm_base::XdgWmBase, _>(1, ());
            handle.create_global::<CompositorState, wl_seat::WlSeat, _>(7, ());
            let mut state = CompositorState::new(thread_observations);

            while thread_running.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        display
                            .handle()
                            .insert_client(stream, Arc::new(()))
                            .expect("insert fixture client");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
                display
                    .dispatch_clients(&mut state)
                    .expect("dispatch fixture clients");
                display.flush_clients().expect("flush fixture clients");
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            socket_path,
            observations,
            running,
            thread: Some(thread),
        }
    }

    fn snapshot(&self) -> Observations {
        self.observations.lock().expect("observations lock").clone()
    }

    fn wait_for_frames(&self, count: usize) -> Observations {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = self.snapshot();
            if snapshot.frames.len() >= count {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "fixture frame timeout");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join fixture compositor");
        }
    }
}

impl Drop for TestCompositor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn fixture_scene() -> Scene {
    let root = Node::new(
        NodeId::new("contract-root").unwrap(),
        Role::Button,
        "native panel",
        Bounds::new(7.0, 11.0, 173.0, 67.0),
        "--state-rest-surface",
    );
    Scene::new(root, NodeId::new("contract-root").unwrap()).unwrap()
}

fn expected_rotated_xrgb(
    scene: &Scene,
    metrics: SurfaceMetrics,
) -> (Vec<u8>, Option<DamageObservation>) {
    let mut rasterizer = Rasterizer::new();
    let frame = rasterizer
        .render(scene, metrics)
        .expect("render expected frame");
    let width = usize::try_from(frame.width).unwrap();
    let height = usize::try_from(frame.height).unwrap();
    let mut xrgb = vec![0; frame.rgba.len()];
    for y in 0..height {
        for x in 0..width {
            let source = (y * width + x) * 4;
            let target_x = height - y - 1;
            let target_y = x;
            let target = (target_y * height + target_x) * 4;
            xrgb[target..target + 4].copy_from_slice(&[
                frame.rgba[source + 2],
                frame.rgba[source + 1],
                frame.rgba[source],
                0xff,
            ]);
        }
    }
    let damage = frame.damage.map(|damage| {
        (
            i32::try_from(frame.height - damage.y - damage.height).unwrap(),
            i32::try_from(damage.x).unwrap(),
            i32::try_from(damage.height).unwrap(),
            i32::try_from(damage.width).unwrap(),
        )
    });
    (xrgb, damage)
}

fn drain_fixture_keys(host: &mut WaylandHost) -> Vec<KeyEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while events.len() < 2 {
        if let Some(event) = host
            .poll_key_event_checked()
            .expect("fixture keyboard transport")
        {
            events.push(event);
        }
        assert!(Instant::now() < deadline, "fixture keyboard timeout");
        thread::sleep(Duration::from_millis(2));
    }
    events
}

fn wait_for_transport_error(host: &mut WaylandHost) -> WaylandHostError {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Err(error) = host.poll_key_event_checked() {
            return error;
        }
        assert!(Instant::now() < deadline, "transport failure timeout");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn compositor_adapter_contract_matrix() {
    let runtime = TempDir::new().expect("fixture runtime directory");
    let missing = runtime.path().join("missing-wayland");
    assert!(matches!(
        WaylandHost::connect_with_socket_and_transform(
            Path::new("relative-wayland"),
            1280,
            720,
            BufferTransform::Rotate90,
        ),
        Err(WaylandHostError::CompositorUnavailable(_))
    ));
    assert!(matches!(
        WaylandHost::connect_with_socket_and_transform(
            &missing,
            1280,
            720,
            BufferTransform::Rotate90,
        ),
        Err(WaylandHostError::CompositorUnavailable(_))
    ));

    let socket_a = runtime.path().join("wayland-generation-a");
    let server_a = TestCompositor::start(socket_a.clone());
    assert_eq!(server_a.socket_path, socket_a);
    let mut host = WaylandHost::connect_with_socket_and_transform(
        &socket_a,
        1280,
        720,
        BufferTransform::Rotate90,
    )
    .expect("connect explicit generation A socket");
    assert_eq!(host.metrics().logical_width, 1280.0);
    assert_eq!(host.metrics().logical_height, 720.0);
    let scene = fixture_scene();
    host.present(&scene).expect("present generation A");
    let snapshot_a = server_a.wait_for_frames(1);
    assert_eq!(snapshot_a.min_size, Some((1280, 720)));
    assert_eq!(snapshot_a.max_size, Some((1280, 720)));
    let frame_a = &snapshot_a.frames[0];
    assert_eq!(
        (frame_a.width, frame_a.height, frame_a.stride),
        (720, 1280, 2880)
    );
    assert_eq!(frame_a.transform, Some(wl_output::Transform::_90));
    let (expected_xrgb, expected_damage) = expected_rotated_xrgb(&scene, host.metrics());
    assert_eq!(frame_a.damage, expected_damage);
    assert_eq!(frame_a.xrgb, expected_xrgb);

    assert_eq!(
        drain_fixture_keys(&mut host),
        vec![
            KeyEvent {
                code: 30,
                keysym: u32::from('a'),
                state: KeyState::Pressed,
                key: Key::Char('a'),
            },
            KeyEvent {
                code: 30,
                keysym: u32::from('a'),
                state: KeyState::Released,
                key: Key::Char('a'),
            },
        ]
    );

    server_a.stop();
    assert!(matches!(
        wait_for_transport_error(&mut host),
        WaylandHostError::Protocol(_) | WaylandHostError::Io(_)
    ));
    assert_eq!(host.present(&scene), Err(PresentFailure::SurfaceLost));
    assert!(matches!(
        host.reconnect_with_socket_and_transform(&missing, BufferTransform::Rotate90),
        Err(WaylandHostError::CompositorUnavailable(_))
    ));

    let socket_b = runtime.path().join("wayland-generation-b");
    let server_b = TestCompositor::start(socket_b.clone());
    host.reconnect_with_socket_and_transform(&socket_b, BufferTransform::Rotate90)
        .expect("reconnect generation B socket");
    host.present(&scene).expect("present generation B");
    let snapshot_b = server_b.wait_for_frames(1);
    assert_eq!(
        snapshot_b.frames[0].transform,
        Some(wl_output::Transform::_90)
    );
    assert_eq!(
        (snapshot_b.frames[0].width, snapshot_b.frames[0].height),
        (720, 1280)
    );
    assert_eq!(snapshot_b.frames[0].xrgb, expected_xrgb);
    server_b.stop();

    let socket_default = runtime.path().join("wayland-default");
    let server_default = TestCompositor::start(socket_default.clone());
    std::env::set_var("WAYLAND_DISPLAY", &socket_default);
    let mut default_host = WaylandHost::connect_with_size(320, 240).expect("default connection");
    default_host.present(&scene).expect("default present");
    let default_snapshot = server_default.wait_for_frames(1);
    assert_eq!(
        (
            default_snapshot.frames[0].width,
            default_snapshot.frames[0].height,
            default_snapshot.frames[0].stride,
        ),
        (320, 240, 1280)
    );
    assert_ne!(
        default_snapshot.frames[0].transform,
        Some(wl_output::Transform::_90)
    );
    server_default.stop();
}
