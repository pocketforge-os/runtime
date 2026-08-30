//! The **reference broker server** — a minimal PFW1 server that wraps any [`Backend`] and
//! answers requests over a Unix socket. It exists to prove the **backend-swap seam** off
//! hardware: point a [`crate::backends::BrokerClientBackend`] at this server and the SAME app
//! code runs unchanged (the load-bearing "survives the runtime fork" demo, epic acceptance).
//!
//! This is NOT the real broker. The real `tsp-e1b.3` daemon adds default-deny-vs-hostile
//! enforcement, `SO_PEERCRED` checks, per-op quotas, app.toml `use=[]` graph validation, and
//! namespace fd-routing. This reference server is the cooperative loopback `.2` uses to
//! demonstrate the wire + client + swap; `pf-broker-ref` is its CLI wrapper.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::Duration;

use pf_prefsd::{RpcRequest as PrefsRequest, RpcResponse as PrefsResponse};
use pf_wire::{recv_request, send_response, Op, PreferenceKind, Request, Response, Status};

use crate::backend::{Backend, Pose};

// Keep the broker's client round trip below prefsd's 3s CONNECTION_TIMEOUT.
const PREFSD_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(2);

/// Compute the response to one request by delegating to the backend. Pure (no I/O), so it is
/// directly unit-testable and shared by every transport.
pub fn handle_request(backend: &dyn Backend, req: &Request) -> Response {
    match req.op {
        Op::IsPresent => Response::boolean(backend.is_present(&req.name)),
        Op::IsGranted => Response::boolean(backend.is_granted(&req.name)),
        Op::Query => Response {
            permission: backend.query(&req.name).to_wire(),
            ..Response::ok()
        },
        Op::Acquire => match backend.acquire(&req.name) {
            Ok(()) => Response::ok(),
            Err(e) => Response::err(e.status()),
        },
        Op::RumblePulse => {
            let st = backend.rumble_pulse(req.arg as u32);
            Response {
                flag: st as u64,
                ..Response::ok()
            }
        }
        Op::GetCapability => match backend.get_capability(&req.name) {
            Ok(v) => Response {
                payload: v,
                ..Response::ok()
            },
            Err(e) => Response::err(e.status()),
        },
        Op::SetCapability => match backend.set_capability(&req.name, &req.payload) {
            Ok(()) => Response::ok(),
            Err(e) => Response::err(e.status()),
        },
        Op::GetPose => match backend.get_pose() {
            Ok(p) => Response {
                payload: p.to_bytes().to_vec(),
                ..Response::ok()
            },
            Err(e) => Response::err(e.status()),
        },
        Op::SetPose => match Pose::from_bytes(&req.payload) {
            Some(p) => match backend.set_pose(p) {
                Ok(np) => Response {
                    payload: np.to_bytes().to_vec(),
                    ..Response::ok()
                },
                Err(e) => Response::err(e.status()),
            },
            // Malformed pose payload is a bad request, not a capability error.
            None => Response::err(Status::Unsupported),
        },
        Op::GetPreference => get_preference(&req.pref_key),
    }
}

/// Forward one preference read over a fresh prefsd connection. Any missing configuration,
/// transport failure, daemon error, or unknown key degrades honestly to NotFound.
fn get_preference(key: &str) -> Response {
    let Some(socket) = std::env::var_os("PF_PREFSD_SOCK") else {
        return Response::ok();
    };
    get_preference_at(&socket, key)
}

fn get_preference_at(socket: impl AsRef<std::path::Path>, key: &str) -> Response {
    get_preference_at_with_timeout(socket, key, PREFSD_ROUND_TRIP_TIMEOUT)
}

fn get_preference_at_with_timeout(
    socket: impl AsRef<std::path::Path>,
    key: &str,
    timeout: Duration,
) -> Response {
    let result = (|| -> Result<PrefsResponse, Box<dyn std::error::Error>> {
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let body = serde_json::to_vec(&PrefsRequest::Get {
            key: key.to_owned(),
        })?;
        pf_wire::write_frame(&mut stream, &body)?;
        let body = pf_wire::read_frame(&mut stream)?;
        Ok(serde_json::from_slice(&body)?)
    })();
    let mut response = Response::ok();
    if let Ok(PrefsResponse::Value { value }) = result {
        if let Some(value) = value.as_bool() {
            response.preference_kind = PreferenceKind::Bool;
            response.preference_bool = value;
        } else if let Some(value) = value.as_i64() {
            response.preference_kind = PreferenceKind::Integer;
            response.preference_integer = value;
        } else if let Some(value) = value.as_str() {
            response.preference_kind = PreferenceKind::Text;
            response.preference_text = value.to_owned();
        }
    }
    // No apply-acknowledgement exists in v1; Response::ok() keeps applied=false.
    response
}

/// Serve one connection: a request/response loop until EOF or a protocol error.
pub fn serve_connection(backend: &dyn Backend, stream: UnixStream) -> io::Result<()> {
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    // Clean disconnect or any protocol error ends the loop → drop the connection (the real
    // broker logs + rate-limits; the reference server just closes).
    while let Ok(req) = recv_request(&mut reader) {
        let resp = handle_request(backend, &req);
        if send_response(&mut writer, &resp).is_err() {
            break;
        }
    }
    Ok(())
}

/// Serve a listener forever, one thread per connection. Blocks the calling thread.
pub fn serve(listener: UnixListener, backend: Arc<dyn Backend>) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let b = backend.clone();
        std::thread::spawn(move || {
            let _ = serve_connection(&*b, stream);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_prefs::{PrefValue, PrefsStore};

    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pocketforge-pref-{tag}-{}", std::process::id()))
    }

    fn serve_n(
        socket: &std::path::Path,
        store: PrefsStore,
        count: usize,
    ) -> std::thread::JoinHandle<()> {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                pf_prefsd::serve_connection(&store, &mut stream).unwrap();
            }
        })
    }

    #[test]
    fn preference_forwarding_is_typed_and_never_claims_applied() {
        let dir = scratch("forward");
        let socket = dir.with_extension("sock");
        let store = PrefsStore::at(&dir);
        store.apply("reduceMotion", PrefValue::Bool(true)).unwrap();
        store
            .apply("hapticsEnabled", PrefValue::Bool(false))
            .unwrap();
        store.apply("monoAudio", PrefValue::Bool(true)).unwrap();
        store.apply("brightness", PrefValue::Scalar(73)).unwrap();
        let server = serve_n(&socket, store, 4);
        for (key, expected) in [
            ("reduceMotion", true),
            ("hapticsEnabled", false),
            ("monoAudio", true),
        ] {
            let response = get_preference_at(&socket, key);
            assert_eq!(response.preference_kind, PreferenceKind::Bool);
            assert_eq!(response.preference_bool, expected);
            assert!(!response.applied);
        }
        let brightness = get_preference_at(&socket, "brightness");
        assert_eq!(brightness.preference_kind, PreferenceKind::Integer);
        assert_eq!(brightness.preference_integer, 73);
        assert!(!brightness.applied);
        server.join().unwrap();
        let _ = std::fs::remove_file(socket);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unavailable_prefsd_degrades_to_not_found() {
        let response = get_preference_at(scratch("absent"), "reduceMotion");
        assert_eq!(response.preference_kind, PreferenceKind::NotFound);
        assert!(!response.applied);
    }

    #[test]
    fn unresponsive_prefsd_degrades_within_client_deadline() {
        let socket = scratch("unresponsive").with_extension("sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            release_rx.recv().unwrap();
        });

        let timeout = Duration::from_millis(50);
        let started = std::time::Instant::now();
        let response = get_preference_at_with_timeout(&socket, "reduceMotion", timeout);
        let elapsed = started.elapsed();

        assert_eq!(response.preference_kind, PreferenceKind::NotFound);
        assert!(!response.applied);
        assert!(
            elapsed < Duration::from_secs(1),
            "unresponsive prefsd took {elapsed:?} to degrade"
        );
        release_tx.send(()).unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_file(socket);
    }
}
