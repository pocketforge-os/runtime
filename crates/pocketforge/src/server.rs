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

use pf_wire::{recv_request, send_response, Op, Request, Response, Status};

use crate::backend::{Backend, Pose};

/// Compute the response to one request by delegating to the backend. Pure (no I/O), so it is
/// directly unit-testable and shared by every transport.
pub fn handle_request(backend: &dyn Backend, req: &Request) -> Response {
    match req.op {
        Op::IsPresent => Response::boolean(backend.is_present(&req.name)),
        Op::IsGranted => Response::boolean(backend.is_granted(&req.name)),
        Op::Query => Response { permission: backend.query(&req.name).to_wire(), ..Response::ok() },
        Op::Acquire => match backend.acquire(&req.name) {
            Ok(()) => Response::ok(),
            Err(e) => Response::err(e.status()),
        },
        Op::RumblePulse => {
            let st = backend.rumble_pulse(req.arg as u32);
            Response { flag: st as u64, ..Response::ok() }
        }
        Op::GetCapability => match backend.get_capability(&req.name) {
            Ok(v) => Response { payload: v, ..Response::ok() },
            Err(e) => Response::err(e.status()),
        },
        Op::SetCapability => match backend.set_capability(&req.name, &req.payload) {
            Ok(()) => Response::ok(),
            Err(e) => Response::err(e.status()),
        },
        Op::GetPose => match backend.get_pose() {
            Ok(p) => Response { payload: p.to_bytes().to_vec(), ..Response::ok() },
            Err(e) => Response::err(e.status()),
        },
        Op::SetPose => match Pose::from_bytes(&req.payload) {
            Some(p) => match backend.set_pose(p) {
                Ok(np) => Response { payload: np.to_bytes().to_vec(), ..Response::ok() },
                Err(e) => Response::err(e.status()),
            },
            // Malformed pose payload is a bad request, not a capability error.
            None => Response::err(Status::Unsupported),
        },
    }
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
