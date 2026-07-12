# PFW1 — PocketForge capability-broker wire protocol (v1)

> **Status:** v0/v1 of the E2 runtime (`tsp-e1b.2`). Versioned, documented, and
> **reimplementable from this file alone** — `crates/pf-wire` is one conforming
> implementation, this doc is the contract. A second-language client (C/Zig/Go/Python)
> can be written from this page with no reference to the Rust source.

This is the **control channel** between `libpocketforge` (the in-app client library) and the
capability broker (the reference `pf-broker-ref`, and the real default-deny daemon in
`tsp-e1b.3`). It is deliberately tiny, default-deny-friendly, and audit-friendly.

## 1. Transport

* A **`SOCK_STREAM` `AF_UNIX`** socket at a well-known path. v0 default:
  `$PF_BROKER_SOCK` if set, else `$XDG_RUNTIME_DIR/pocketforge/broker.sock`, else
  `/run/pocketforge/broker.sock`. (The broker creates it `0600`, owned by the supervisor;
  it is bind-mounted into each app namespace by `.3` — until then `pf-broker-ref` listens
  on any path passed to it.)
* One request → one response, strictly ordered, on the same connection. A client MAY open
  multiple connections; the broker treats each independently.
* The socket carries **only low-rate control traffic** (see §5). The INPUT capability's
  per-event stream is NOT on this socket.

## 2. Framing

Every message — request or response — is:

```
+--------------------+------------------------+
| len : u32 (BE)     | body : len bytes       |
+--------------------+------------------------+
```

* `len` is the body length in bytes, big-endian (network byte order).
* `len` MUST be ≤ **65536** (`MAX_FRAME`). A larger prefix is a protocol violation; the
  peer MUST close the connection without allocating the buffer (DoS bound).
* A short read on either the prefix or the body is a truncation error; close the connection.

## 3. Body encoding (protobuf wire format, restricted)

The body is a sequence of fields using **protocol-buffers wire encoding**, restricted to two
wire types. Each field is:

```
key = (field_number << 3) | wire_type      ; encoded as a base-128 varint (LEB128, unsigned)
```

* **wire_type 0 — varint:** the value is a base-128 varint (used for enums and integers).
* **wire_type 2 — length-delimited:** a varint byte-count `n`, then `n` raw bytes (used for
  UTF-8 strings and opaque payloads).

Rules:

* Fields MAY appear in any order. A repeated field takes the last value.
* **Unknown field numbers MUST be skipped** (read the wire type, consume the value) so the
  protocol can add fields without breaking old peers. A decoder MUST still reject an unknown
  *wire type* (1, 3, 4, 5) as malformed.
* A varint MUST terminate within 10 bytes.
* Omitted scalar fields take their default (0 / empty / first enum value).

### Varint (LEB128, unsigned)

Little-endian groups of 7 bits, high bit = "more bytes follow":

```
encode(v): while true { b = v & 0x7f; v >>= 7; if v != 0 { b |= 0x80 }; emit(b); if v == 0 break }
```

## 4. Messages

### 4.1 Request (client → broker)

| field # | wire type | name      | meaning                                            |
|--------:|-----------|-----------|----------------------------------------------------|
| 1       | varint    | `op`      | [`Op`](#op-values) — REQUIRED                       |
| 2       | len       | `name`    | capability name, UTF-8 (e.g. `"vibration"`)         |
| 3       | len       | `payload` | opaque value (e.g. bytes to `SetCapability`)        |
| 4       | varint    | `arg`     | scalar arg (e.g. rumble duration in ms)             |

#### `Op` values

| value | op              | response carries                                  |
|------:|-----------------|---------------------------------------------------|
| 1     | `IsPresent`     | `status=Ok`, `flag` = 0/1 (descriptor+probe)      |
| 2     | `IsGranted`     | `status=Ok`, `flag` = 0/1 (present AND allowed)    |
| 3     | `Query`         | `status=Ok`, `permission` = Granted/Denied/Prompt |
| 4     | `Acquire`       | `status` = the four-way taxonomy, or `Ok`         |
| 5     | `GetCapability` | `status`, `payload` = value                        |
| 6     | `SetCapability` | `status`                                           |
| 7     | `RumblePulse`   | `status=Ok`, `flag` = [`RumbleStatus`]            |
| 8     | `GetPose`       | `status`, `payload` = pose (9× f64 LE, 72 bytes)  |
| 9     | `SetPose`       | `payload` = pose; → `status`, `payload` = new pose |

Pose payload is **9 IEEE-754 `binary64` little-endian** values in order
`yaw, pitch, roll, x, y, z, wx, wy, wz` (orientation in degrees, angular velocity in deg/s,
position in metres). `GetPose`/`SetPose` on a device with no IMU return `status=HardwareAbsent`.

> **Acquiring INPUT** (added by `.6`): `Acquire` with `name="input"` returns the shared
> `uinput` device fd out-of-band via `SCM_RIGHTS` on the same socket. The fd, not RPC, is the
> hot path. `.6` specifies the ancillary-data layout; PFW1 itself does not frame the fd.
>
> The C ABI surfaces this as `pf_acquire_input_fd` (`tsp-e1b.10`, additive) — **no new `Op`**:
> the broker-client backend sends this exact `Acquire("input")` and reads the framed `Response`
> payload + the `SCM_RIGHTS` fd in one `recvmsg`; the in-process backend opens the
> platform-provided node directly. Same facade, backend-swappable, wire unchanged.

### 4.2 Response (broker → client)

| field # | wire type | name         | meaning                                          |
|--------:|-----------|--------------|--------------------------------------------------|
| 1       | varint    | `status`     | [`Status`](#status-values) — REQUIRED            |
| 2       | len       | `payload`    | result bytes (e.g. a `GetCapability` value)      |
| 3       | varint    | `flag`       | small scalar result (bool, or `RumbleStatus`)    |
| 4       | varint    | `permission` | [`Permission`](#permission-values) for `Query`   |

#### `Status` values — the four-way taxonomy (briefing §A)

| value | status           | meaning                                             |
|------:|------------------|-----------------------------------------------------|
| 0     | `Ok`             | success                                             |
| 1     | `Unsupported`    | the platform has no such capability type            |
| 2     | `PolicyBlocked`  | refused by policy (e.g. default-deny privacy cap)   |
| 3     | `ConsentDenied`  | the user/consent layer denied it                    |
| 4     | `HardwareAbsent` | the descriptor advertises no such hardware          |

#### `Permission` values (Permissions-API `query()` shape)

| value | permission |
|------:|------------|
| 0     | `Granted`  |
| 1     | `Denied`   |
| 2     | `Prompt`   |

#### `RumbleStatus` values (the unified no-op shape)

| value | status            | meaning                                              |
|------:|-------------------|------------------------------------------------------|
| 0     | `Fired`           | motor present AND haptics enabled → would actuate    |
| 1     | `NoopAbsent`      | no rumble motor on this descriptor (a133)            |
| 2     | `NoopSuppressed`  | motor present but `hapticsEnabled` is off (E4)       |

`NoopAbsent` and `NoopSuppressed` are **both `status=Ok`** — a cosmetic-tier call never
fails. The reason lives in `flag`, so the app does not special-case absence.

## 5. Channel split — folds in SPIKE-1 (`tsp-e1b.1`)

SPIKE-1 (`spikes/ipc-60hz/`) measured an `AF_UNIX` round-trip at ~25× a shared-fd `read()`
and showed it couples the broker's ms-scale scheduling tail to the caller. Therefore:

* **Low-rate capabilities → PFW1 RPC (this protocol).** Vibration, sensors, location, audio,
  settings, entropy, and all presence/permission queries. These fire at most a few times per
  second; at an A53-estimated ~100 µs p99 per round-trip that is <0.001% of a 60 Hz frame.
  Threshold: PFW1 RPC is acceptable for any capability under **~1000 calls/sec on A53**.
* **INPUT → shared/handed fd, NEVER per-event RPC.** The input hot path is the `uinput`
  device the broker `EVIOCGRAB`s and re-emits (child `.6`); the app `read()`s it directly.
  PFW1 carries only the *acquisition* of that fd (via `SCM_RIGHTS`), not the event stream.

This split is the load-bearing design constraint SPIKE-1 confirmed; it is not negotiable at
the wire layer.

## 6. Versioning

* `WIRE_VERSION = 1`. Backward-compatible growth is by **adding new field numbers** (old
  peers skip them, §3) or **new `Op`/`Status` values** (peers reject unknown enums rather
  than guessing — see `BadEnum`). A breaking change bumps `WIRE_VERSION` and the socket-path
  basename (`broker.sock` → `broker.v2.sock`), never silently.
* The frozen, never-silently-broken public surface (this file + the C ABI header) is formally
  version-frozen at **v1** by `STABILITY.md` (the compat/deprecation policy + the CI guards
  `crates/pf-wire/tests/frozen_contract.rs` and `abi/check-abi.sh`); the Platform/SDK split that
  hands it to E8 is `docs/RUNTIME-SDK-SPLIT.md` (`tsp-e1b.5`).

## 7. Security notes (honesty: contract now, enforce later — R-A)

* In **v0** the broker is the reference loopback (`pf-broker-ref`) wrapping the same
  in-process backend, used to prove the **backend-swap seam** off-hardware. It does **not**
  yet enforce confinement; the v0 facade is cooperative (an app running as `gamer` with the
  `input`/`video`/`render` groups holds ambient `/dev/*` authority regardless). INPUT is the
  one exception (`.6`, `uinput`+`EVIOCGRAB`).
* Real enforcement — default-deny against a hostile peer, peer-credential checks
  (`SO_PEERCRED`), per-op rate limits/quotas, and unforgeable routed handles — lands in the
  out-of-process daemon (`.3`) on the Phase-2 substrate. This doc fixes the *wire*; `.3`
  fixes the *trust*.
