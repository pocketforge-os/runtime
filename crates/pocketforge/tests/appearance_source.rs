use std::os::unix::net::UnixStream;
use std::sync::Arc;

use pf_prefs::PrefValue;
use pocketforge::backends::{BrokerClientBackend, InProcessBackend};
use pocketforge::{server, AppearanceSource, Backend, Descriptor, Pf};

fn descriptor() -> Arc<Descriptor> {
    Arc::new(
        Descriptor::from_toml(
            r#"
[identity]
id = "appearance-source-test"
manufacturer = "PocketForge"
model = "Test"
sdl_guid = "00000000000000000000000000000000"
"#,
        )
        .unwrap(),
    )
}

fn assert_source_semantics(backend: &InProcessBackend, read: impl Fn() -> AppearanceSource) {
    assert_eq!(read(), AppearanceSource::Default);

    backend.set_preference("appearance", PrefValue::Enum("dark"));
    assert_eq!(read(), AppearanceSource::User);

    backend.set_preference("appearance", PrefValue::Enum("light"));
    assert_eq!(read(), AppearanceSource::User);

    backend.set_preference_bool("highContrast", true);
    assert_eq!(read(), AppearanceSource::User);
}

#[test]
fn in_process_source_tracks_only_the_appearance_key() {
    let backend = InProcessBackend::shared(descriptor());
    let pf = Pf::over_in_process(backend.clone());
    assert_source_semantics(&backend, || pf.appearance_source());
}

#[test]
fn broker_source_round_trips_and_ignores_high_contrast() {
    let backend = InProcessBackend::shared(descriptor());
    let (client, server_stream) = UnixStream::pair().unwrap();
    let server_backend = backend.clone();
    let server = std::thread::spawn(move || {
        server::serve_connection(&*server_backend, server_stream).unwrap()
    });
    let client = BrokerClientBackend::from_stream(client);

    assert_source_semantics(&backend, || client.appearance_source());
    drop(client);
    server.join().unwrap();
}
