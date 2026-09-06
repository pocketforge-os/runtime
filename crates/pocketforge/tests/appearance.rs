use std::os::unix::net::UnixStream;
use std::sync::Arc;

use pf_prefs::PrefValue;
use pocketforge::backends::{BrokerClientBackend, InProcessBackend};
use pocketforge::{server, Appearance, Backend, Descriptor, Pf};

fn descriptor() -> Arc<Descriptor> {
    Arc::new(
        Descriptor::from_toml(
            r#"
[identity]
id = "appearance-test"
manufacturer = "PocketForge"
model = "Test"
sdl_guid = "00000000000000000000000000000000"
"#,
        )
        .unwrap(),
    )
}

fn assert_all_values(backend: &InProcessBackend, read: impl Fn() -> Appearance) {
    assert_eq!(read(), Appearance::Dark);
    backend.set_preference("appearance", PrefValue::Enum("light"));
    assert_eq!(read(), Appearance::Light);
    backend.set_preference_bool("highContrast", true);
    assert_eq!(read(), Appearance::HighContrast);
}

#[test]
fn in_process_appearance_covers_default_light_and_contrast_overlay() {
    let backend = InProcessBackend::shared(descriptor());
    let pf = Pf::over_in_process(backend.clone());
    assert_all_values(&backend, || pf.appearance());
}

#[test]
fn broker_appearance_round_trips_all_values() {
    let backend = InProcessBackend::shared(descriptor());
    let (client, server_stream) = UnixStream::pair().unwrap();
    let server_backend = backend.clone();
    let server = std::thread::spawn(move || {
        server::serve_connection(&*server_backend, server_stream).unwrap()
    });
    let client = BrokerClientBackend::from_stream(client);

    assert_all_values(&backend, || client.appearance());
    drop(client);
    server.join().unwrap();
}
