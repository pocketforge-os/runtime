//! Preference reads on the broker's session socket reach the live prefsd named by
//! `$PF_PREFSD_SOCK` through `pocketforge::server::handle_request` (`tsp-f3fm.202.1`).
//!
//! This is its own test binary because it sets a process-wide environment variable. The
//! broker's fallback backend is store-less and answers Dark/defaults. Seeing stored appearance,
//! source, bool, and scalar values proves the answer came from prefsd.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use pf_input_broker::serve_client;
use pf_prefs::{PrefValue, PrefsStore};
use pocketforge::backends::{BrokerClientBackend, InProcessBackend};
use pocketforge::{Appearance, Backend};

#[test]
fn app_preference_reads_are_passed_through_to_prefsd() {
    let dir = std::env::temp_dir().join(format!("pf-broker-prefsd-{}", std::process::id()));
    let socket = dir.with_extension("sock");
    let _ = std::fs::remove_file(&socket);
    let store = PrefsStore::at(&dir);
    store.apply("appearance", PrefValue::Enum("light")).unwrap();
    store.apply("reduceMotion", PrefValue::Bool(true)).unwrap();
    store.apply("brightness", PrefValue::Scalar(73)).unwrap();

    let listener = UnixListener::bind(&socket).unwrap();
    let prefsd_store = store.clone();
    let prefsd = std::thread::spawn(move || {
        // One prefsd connection per read, as the broker opens a fresh one each call.
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            pf_prefsd::serve_connection(&prefsd_store, &mut stream).unwrap();
        }
    });
    std::env::set_var("PF_PREFSD_SOCK", &socket);

    let fallback: Arc<dyn Backend> = Arc::new(InProcessBackend::new(Arc::new(
        pocketforge::test_support::gnss_descriptor(),
    )));
    assert_eq!(fallback.appearance(), Appearance::Dark, "fallback differs");

    let (client, server) = UnixStream::pair().unwrap();
    let broker = std::thread::spawn(move || serve_client(server, "/unused", &*fallback));
    let app = BrokerClientBackend::from_stream(client);

    assert_eq!(app.appearance(), Appearance::Light);
    assert_eq!(app.appearance_source(), pocketforge::AppearanceSource::User);
    assert!(app.preference_bool("reduceMotion", false));
    assert_eq!(app.preference_scalar("brightness", 100), 73);

    drop(app);
    broker.join().unwrap().unwrap();
    prefsd.join().unwrap();
    std::env::remove_var("PF_PREFSD_SOCK");
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_dir_all(dir);
}
