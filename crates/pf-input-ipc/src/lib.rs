//! # pf-input-ipc — the router ↔ gamescope ↔ pf-osk ↔ system-menu contract
//!
//! Frozen interface for the PocketForge system input layer (epic `tsp-ihne1`, bead
//! `tsp-ihne1.36`). The human-readable contract is [`docs/input-ipc.md`](../../docs/input-ipc.md);
//! this crate is its machine-checkable half: the socket constants, the timeout constants, the
//! message types of every channel, the versioned envelope, and pure state machines
//! (`MenuSession`, `PadGate`, `CompositorSession`) that encode the ordering and recovery rules so
//! the router and its tests share one definition.
//!
//! The crate performs **no I/O** and links only `serde` + `serde_json`. The input router
//! (`pf-input-broker`), gamescope (C++) and pf-osk (Rust) each implement the transport
//! described in the doc: `AF_UNIX` `SOCK_STREAM`, pf-wire framing (`u32` big-endian length
//! prefix, ≤ 64 KiB) and a JSON object body that always carries `"version"` and `"type"`.
//!
//! ## Versioning
//!
//! `version` is the schema **major**. A peer refuses any frame whose major it does not speak
//! ([`IpcError::UnsupportedVersion`]) before it looks at `type`. Within a major, new optional
//! fields and new message types may be added; unknown fields are ignored, an unknown `type`
//! is [`IpcError::Malformed`] and the receiver drops that frame (not the connection) unless the
//! channel doc says otherwise. Changing the meaning of a field, removing one, or making a new
//! field mandatory requires a new major.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// The schema major this crate speaks. Every encoded frame carries it as `"version"`.
pub const SCHEMA_MAJOR: u32 = 1;

/// Largest frame body a peer accepts (pf-wire `MAX_FRAME`). Larger prefixes close the connection.
pub const MAX_FRAME: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

/// Socket paths, ownership and modes. Created by the router; `INPUT_RUN_DIR` itself comes from
/// the image's tmpfiles entry. Each trusted endpoint pins its service user, role group and exact
/// systemd unit; ordinary app clients retain the separate session-uid rule.
pub mod sockets {
    /// Parent of every input-layer endpoint.
    pub const INPUT_RUN_DIR: &str = "/run/pocketforge/input";
    /// Mode of [`INPUT_RUN_DIR`]: `root:pf-input`, group-traversable only.
    pub const INPUT_RUN_DIR_MODE: u32 = 0o750;
    /// Group that may traverse [`INPUT_RUN_DIR`]. Apps never carry it.
    pub const INPUT_GROUP: &str = "pf-input";

    /// Mode of every listener socket: owner root, the role group, nobody else.
    pub const SOCKET_MODE: u32 = 0o660;

    /// gamescope connects here (role `compositor`).
    pub const COMPOSITOR_SOCKET: &str = "/run/pocketforge/input/compositor.sock";
    pub const COMPOSITOR_USER: &str = "pf-gamescope";
    pub const COMPOSITOR_GROUP: &str = "pf-input-compositor";
    pub const COMPOSITOR_UNIT: &str = "pocketforge-gamescope.service";

    /// pf-osk connects here (role `keyboard`); it receives its private pad fd on `registered`.
    pub const KEYBOARD_SOCKET: &str = "/run/pocketforge/input/keyboard.sock";
    pub const KEYBOARD_USER: &str = "pf-ime";
    pub const KEYBOARD_GROUP: &str = "pf-input-keyboard";
    pub const KEYBOARD_UNIT: &str = "pf-osk.service";

    /// The system menu connects here (role `menu`); it receives its private pad fd on
    /// `registered`. This is the endpoint `pf-input-broker --system-menu-sock` already serves.
    pub const MENU_SOCKET: &str = "/run/pocketforge/input/menu.sock";
    pub const MENU_USER: &str = "pf-menu";
    pub const MENU_GROUP: &str = "pf-input-menu";
    pub const MENU_UNIT: &str = "pf-system-menu.service";

    /// Reconnect policy for every client after EOF: first retry after this delay…
    pub const RECONNECT_INITIAL: std::time::Duration = std::time::Duration::from_millis(100);
    /// …doubling up to this ceiling. The router restarts under systemd; clients simply reconnect.
    pub const RECONNECT_MAX: std::time::Duration = std::time::Duration::from_secs(2);
}

// ---------------------------------------------------------------------------
// Timeouts (defined once here; the router, the menu and every test reference these)
// ---------------------------------------------------------------------------

/// Every time budget of the contract. Changing one is a contract change.
pub mod timeouts {
    use std::time::Duration;

    /// The registered menu provider must `ack` a `system_menu` within this time, or the router
    /// drops the provider and performs the return path itself (runtime#108, adjudication D2-G1).
    pub const MENU_ACK_TIMEOUT: Duration = Duration::from_millis(250);
    /// After `ack`, the provider must report `shown` (its first frame committed) within this
    /// time, or the router dismisses it and returns to the shell.
    pub const MENU_SHOWN_TIMEOUT: Duration = Duration::from_millis(500);
    /// While the menu is open the router pings it this often…
    pub const MENU_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
    /// …and a provider that has not answered any ping for this long is unhealthy: the router
    /// routes the pad back to the app and dismisses the provider.
    pub const MENU_HEALTH_TIMEOUT: Duration = Duration::from_secs(1);
    /// Holding Menu this long returns to the shell from anywhere. Handled by the router itself,
    /// never by the provider, so a menu that acks but never draws cannot trap the user.
    pub const MENU_HOLD_RETURN: Duration = Duration::from_secs(2);
    /// A modal keyboard (`show_modal`) opens only within this window after a user press the
    /// router routed to that app (adjudication item 15). One press admits one open.
    pub const USER_PRESS_WINDOW: Duration = Duration::from_secs(1);
    /// A heartbeat `pong` must echo the `seq` of a `ping` sent no longer ago than this; older
    /// echoes are ignored (they cannot make an unhealthy peer healthy again).
    pub const PONG_MAX_AGE: Duration = MENU_HEALTH_TIMEOUT;
}

// ---------------------------------------------------------------------------
// Envelope + errors
// ---------------------------------------------------------------------------

/// Errors from decoding a frame body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    /// `version` is a major this crate does not speak. Checked before anything else.
    UnsupportedVersion { got: u32, supported: u32 },
    /// Not a JSON object, no `version`, no `type`, unknown `type`, or a field of the wrong shape.
    Malformed(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { got, supported } => {
                write!(
                    f,
                    "unsupported input-ipc version {got} (this peer speaks {supported})"
                )
            }
            Self::Malformed(reason) => write!(f, "malformed input-ipc frame: {reason}"),
        }
    }
}

impl std::error::Error for IpcError {}

/// Every frame body: `{"version": <major>, "type": "<name>", ...fields}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame<M> {
    pub version: u32,
    #[serde(flatten)]
    pub message: M,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: Option<u32>,
}

/// Encode one message as a frame body with the current [`SCHEMA_MAJOR`].
pub fn encode<M: Serialize>(message: &M) -> Vec<u8> {
    let frame = Frame {
        version: SCHEMA_MAJOR,
        message,
    };
    serde_json::to_vec(&frame).expect("contract types always serialize")
}

/// Decode one frame body. The schema major is checked before the message shape.
pub fn decode<M: DeserializeOwned>(body: &[u8]) -> Result<M, IpcError> {
    let probe: VersionProbe =
        serde_json::from_slice(body).map_err(|error| IpcError::Malformed(error.to_string()))?;
    match probe.version {
        Some(got) if got == SCHEMA_MAJOR => {}
        Some(got) => {
            return Err(IpcError::UnsupportedVersion {
                got,
                supported: SCHEMA_MAJOR,
            })
        }
        None => return Err(IpcError::Malformed("missing version".into())),
    }
    let frame: Frame<M> =
        serde_json::from_slice(body).map_err(|error| IpcError::Malformed(error.to_string()))?;
    Ok(frame.message)
}

// ---------------------------------------------------------------------------
// Shared vocabulary
// ---------------------------------------------------------------------------

/// Which endpoint a peer registered on. Implied by the socket, repeated in `register` so a
/// misconnected client fails loudly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Compositor,
    Keyboard,
    Menu,
}

/// The trusted overlay roles gamescope admits. A trusted connection is bound to exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayRole {
    Keyboard,
    Menu,
    Toast,
}

/// Where the router currently sends the built-in pad. Exactly one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadDestination {
    App,
    Keyboard,
    Pointer,
    Shell,
    Menu,
    /// No recipient while compositor authority is absent or its snapshot is incomplete.
    Quarantined,
}

/// A decoded transition from the router-owned built-in pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadControl {
    /// The protected system Menu control.
    Menu,
    /// Any non-Menu pad control; the concrete key/axis remains outside this contract crate.
    Ordinary,
}

/// Result of applying the Menu-first rule before selecting a pad destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadInputDisposition {
    ConsumedByRouter,
    RoutedTo(PadDestination),
}

/// Apply ordering rule 8(a). This function deliberately takes the destination as an argument:
/// callers must run it before writing to *any* destination, including an already-open menu.
pub fn route_pad_input(destination: PadDestination, control: PadControl) -> PadInputDisposition {
    match control {
        PadControl::Menu => PadInputDisposition::ConsumedByRouter,
        PadControl::Ordinary => PadInputDisposition::RoutedTo(destination),
    }
}

/// Observable gamescope steps for a protected Power transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GamescopeInputStep {
    Consume,
    RelayToRouter,
    SeatDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectedPowerOrderError;

/// Protected Power has exactly one valid disposition: consume it in gamescope, then relay it to
/// the router. A seat-delivery step anywhere is a contract violation.
pub fn validate_protected_power_order(
    steps: &[GamescopeInputStep],
) -> Result<(), ProtectedPowerOrderError> {
    if steps
        == [
            GamescopeInputStep::Consume,
            GamescopeInputStep::RelayToRouter,
        ]
    {
        Ok(())
    } else {
        Err(ProtectedPowerOrderError)
    }
}

/// Integer rectangle in compositor (logical, rotated) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The app in front, as the router knows it from the session authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontApp {
    /// Manifest id (`validate_app_id` shape).
    pub app_id: String,
    /// Manifest display name; the only name the menu and the modal header ever show.
    pub name: String,
    /// systemd unit, e.g. `pf-app@<id>.service`.
    pub unit: String,
}

/// text-input-v3 `content_purpose`. `password` and `pin` make gamescope raise `secure_entry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentPurpose {
    Normal,
    Alpha,
    Digits,
    Number,
    Phone,
    Url,
    Email,
    Name,
    Password,
    Pin,
    Date,
    Time,
    Datetime,
    Terminal,
}

impl ContentPurpose {
    /// The purposes that are secret by definition (adjudication item 14).
    pub fn is_secret(self) -> bool {
        matches!(self, Self::Password | Self::Pin)
    }
}

/// A string that must never be logged. `Debug` and `Display` redact it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

// ---------------------------------------------------------------------------
// Menu channel (router ↔ system menu). Extends the runtime#108 provider frames, same major.
// ---------------------------------------------------------------------------

pub mod menu {
    use super::*;

    /// Actions a menu-channel `ack` can name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum MenuAction {
        SystemMenu,
        Return,
    }

    /// Why the provider closed its own sheet.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CloseReason {
        /// Resume, B on the root, or any item that closes the sheet.
        Resume,
        /// The provider started the return path (`return` was sent first).
        Returning,
        /// The provider handed focus to another trusted surface (Settings).
        Handoff,
    }

    /// Why the router told the provider to hide.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum DismissReason {
        /// `shown` did not arrive within `MENU_SHOWN_TIMEOUT`.
        ShownTimeout,
        /// No `pong` for `MENU_HEALTH_TIMEOUT`.
        Unhealthy,
        /// Menu was held for `MENU_HOLD_RETURN`; the router is returning to the shell.
        HoldReturn,
        /// The app in front exited or was replaced while the menu was open.
        FrontChanged,
    }

    /// Provider → router.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum MenuToRouter {
        /// First frame on the connection. The router answers `registered` with the pad fd.
        Register,
        /// Ownership of the action was accepted (not: a frame was drawn).
        Ack { action: MenuAction },
        /// First frame of the sheet committed to gamescope.
        Shown,
        /// The Return item: the router performs the one return path.
        Return,
        /// The sheet closed by the provider's own choice; the pad goes back to the app.
        Closed { reason: CloseReason },
        /// A field on the provider's own surface is secret (Wi-Fi password, QR share). The
        /// router relays it to gamescope as `surface_secure_entry`; gamescope stays the producer.
        SecureEntry { active: bool },
        /// Echo of `ping`.
        Pong { seq: u64 },
    }

    /// Router → provider.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum RouterToMenu {
        /// Registration accepted. Carries exactly two `SCM_RIGHTS` descriptors in one sendmsg,
        /// ordered `[private_pad_evdev_fd, trusted_wayland_client_fd]`.
        Registered,
        /// A Menu press. `front` is `None` in the shell.
        SystemMenu {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            front: Option<FrontApp>,
        },
        /// Reply to `return` (the only action acked in this direction).
        Ack { action: MenuAction },
        /// Hide now; the pad is already back with the app.
        Dismiss { reason: DismissReason },
        /// Health probe, every `MENU_HEARTBEAT_INTERVAL` while the menu is open.
        Ping { seq: u64 },
    }

    pub const TO_ROUTER_TYPES: &[&str] = &[
        "register",
        "ack",
        "shown",
        "return",
        "closed",
        "secure_entry",
        "pong",
    ];
    pub const FROM_ROUTER_TYPES: &[&str] = &["registered", "system_menu", "ack", "dismiss", "ping"];
}

// ---------------------------------------------------------------------------
// Keyboard channel (router ↔ pf-osk)
// ---------------------------------------------------------------------------

pub mod keyboard {
    use super::*;

    /// The keyboard surfaces pf-osk can show. Only `wants_pad` decides routing, not the kind.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum KeyboardSurface {
        Docked,
        Floating,
        NumberPad,
        EditPad,
        Modal,
        /// Suggestion strip while a real keyboard is in use. Never takes the pad.
        Strip,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ModalOutcome {
        Submitted,
        Cancelled,
    }

    /// Why the router told pf-osk to hide.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum HideReason {
        /// The system menu opened; the keyboard comes back unchanged on Resume.
        MenuOpen,
        /// The first real keypress closes the on-screen keyboard (a strip may stay).
        PhysicalKeyboard,
        /// The app that owned the field lost front.
        AppLostFront,
        /// The app that owned the field exited.
        AppExit,
    }

    /// pf-osk → router.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum KeyboardToRouter {
        /// First frame. Lists the surfaces this build can show.
        Register {
            surfaces: Vec<KeyboardSurface>,
        },
        /// A surface appeared or disappeared. `wants_pad` is pf-osk's half of the pad gate;
        /// gamescope's `overlay{keyboard, mapped}` is the other half.
        Visibility {
            surface: KeyboardSurface,
            visible: bool,
            wants_pad: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            rect: Option<Rect>,
        },
        /// Result of a `show_modal`. `text` is present only for `submitted` and is never logged.
        ModalResult {
            request_id: u64,
            outcome: ModalOutcome,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            text: Option<Secret>,
        },
        Pong {
            seq: u64,
        },
    }

    /// router → pf-osk.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum RouterToKeyboard {
        /// Registration accepted. Carries exactly two `SCM_RIGHTS` descriptors in one sendmsg,
        /// ordered `[private_pad_evdev_fd, trusted_wayland_client_fd]`.
        Registered,
        /// Open the modal keyboard for the app in front (SDK `pf_osk_show` MODAL, spec §12.1).
        /// The router has already applied the `USER_PRESS_WINDOW` open guard.
        ShowModal {
            request_id: u64,
            app: FrontApp,
            purpose: ContentPurpose,
            title: String,
            initial_text: Secret,
            max_chars: u32,
            multiline: bool,
        },
        Hide {
            reason: HideReason,
        },
        Ping {
            seq: u64,
        },
    }

    pub const TO_ROUTER_TYPES: &[&str] = &["register", "visibility", "modal_result", "pong"];
    pub const FROM_ROUTER_TYPES: &[&str] = &["registered", "show_modal", "hide", "ping"];
}

// ---------------------------------------------------------------------------
// Compositor channel (router ↔ gamescope)
// ---------------------------------------------------------------------------

pub mod compositor {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// What has keyboard focus in gamescope.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum FocusedSurface {
        None,
        App { app_id: String },
        Trusted { role: OverlayRole },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TextInputState {
        pub active: bool,
        pub purpose: ContentPurpose,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum OverlayState {
        Mapped,
        Unmapped,
    }

    /// One overlay's authoritative state in a registration snapshot.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct OverlaySnapshot {
        pub role: OverlayRole,
        pub state: OverlayState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub rect: Option<Rect>,
    }

    /// Focus and secure-entry state replayed on every compositor registration.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FocusSnapshot {
        pub surface: FocusedSurface,
        pub text_input: TextInputState,
        pub secure_entry: bool,
    }

    /// Protected Power's level and sequence watermark. Applying this never synthesises an action.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ProtectedPowerSnapshot {
        pub pressed: bool,
        pub last_sequence: u64,
    }

    /// Complete compositor authority replay. It must name every overlay role exactly once.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CompositorSnapshot {
        pub epoch: u64,
        pub focus: FocusSnapshot,
        pub overlays: Vec<OverlaySnapshot>,
        pub power: ProtectedPowerSnapshot,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PowerState {
        Pressed,
        Released,
    }

    /// Classes of real devices gamescope owns through libinput.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PhysicalInputClass {
        Keyboard,
        Mouse,
        Touch,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    pub enum TrustedConnectionOutcome {
        /// The client fd travels with this frame by `SCM_RIGHTS`.
        Ok,
        Refused {
            reason: String,
        },
    }

    /// gamescope → router.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum CompositorToRouter {
        Register,
        /// Mandatory full replay after `registered`; no deltas are accepted before it.
        StateSnapshot {
            #[serde(flatten)]
            snapshot: CompositorSnapshot,
        },
        /// Sent on the frame the focus, the text-input state or `secure_entry` changes.
        /// `secure_entry` is computed here and only here.
        Focus {
            epoch: u64,
            surface: FocusedSurface,
            text_input: TextInputState,
            secure_entry: bool,
        },
        /// A trusted overlay was mapped or unmapped.
        Overlay {
            epoch: u64,
            role: OverlayRole,
            state: OverlayState,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            rect: Option<Rect>,
        },
        /// A real device produced input. Sent on the first event of a class and on every class
        /// change, never per event. The router owns modality.
        PhysicalInput {
            epoch: u64,
            class: PhysicalInputClass,
        },
        /// KEY_POWER/AXP PEK consumed by gamescope before normal seat or hotkey delivery.
        ProtectedPower {
            epoch: u64,
            sequence: u64,
            state: PowerState,
        },
        /// Answer to `trusted_connection`.
        TrustedConnectionResult {
            epoch: u64,
            request_id: u64,
            result: TrustedConnectionOutcome,
        },
        Pong {
            epoch: u64,
            seq: u64,
        },
    }

    /// router → gamescope.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum RouterToCompositor {
        Registered {
            epoch: u64,
        },
        /// The provenance API. gamescope creates a socketpair, registers the server-side
        /// `wl_client` as trusted **for `role` only**, then replies `trusted_connection_result`
        /// with the client end. The router hands that fd to the system-launched process.
        TrustedConnection {
            epoch: u64,
            request_id: u64,
            role: OverlayRole,
            /// systemd unit of the process that will own the connection (diagnostics only;
            /// never an input to the trust decision).
            unit: String,
        },
        /// Relay of a trusted surface's own `secure_entry` flag (menu channel).
        SurfaceSecureEntry {
            epoch: u64,
            role: OverlayRole,
            active: bool,
        },
        Ping {
            epoch: u64,
            seq: u64,
        },
    }

    pub const TO_ROUTER_TYPES: &[&str] = &[
        "register",
        "state_snapshot",
        "focus",
        "overlay",
        "physical_input",
        "protected_power",
        "trusted_connection_result",
        "pong",
    ];
    pub const FROM_ROUTER_TYPES: &[&str] = &[
        "registered",
        "trusted_connection",
        "surface_secure_entry",
        "ping",
    ];

    /// Refusals made by the pure recovery model before callers mutate router state.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum CompositorStateError {
        OldEpoch { got: u64, current: u64 },
        SnapshotRequired,
        SnapshotAlreadyApplied,
        IncompleteSnapshot,
        NonMonotonicPowerSequence { got: u64, last: u64 },
        DuplicatePowerState { state: PowerState },
        UnexpectedRequestId(u64),
        MissingFileDescriptor(u64),
        UnexpectedFileDescriptorCount { request_id: u64, got: usize },
        RoleAlreadyBound(OverlayRole),
        RoleNotBound(OverlayRole),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PowerStep {
        Accepted,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DisconnectEffects {
        pub secure_entry: bool,
        pub quarantine_pad: bool,
        pub close_role_peers: bool,
        pub safe_return: bool,
        pub invalidated_request_ids: Vec<u64>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SessionPhase {
        AwaitingSnapshot,
        Active,
        Disconnected,
    }

    /// Transport-independent enforcement for one compositor connection epoch.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CompositorSession {
        epoch: u64,
        phase: SessionPhase,
        last_power_sequence: u64,
        pending_requests: BTreeMap<u64, OverlayRole>,
        live_roles: BTreeSet<OverlayRole>,
        snapshot: Option<CompositorSnapshot>,
    }

    impl CompositorSession {
        pub fn new(epoch: u64) -> Self {
            assert_ne!(epoch, 0, "connection epochs are nonzero");
            Self {
                epoch,
                phase: SessionPhase::AwaitingSnapshot,
                last_power_sequence: 0,
                pending_requests: BTreeMap::new(),
                live_roles: BTreeSet::new(),
                snapshot: None,
            }
        }

        fn check_epoch(&self, epoch: u64) -> Result<(), CompositorStateError> {
            if epoch != self.epoch {
                Err(CompositorStateError::OldEpoch {
                    got: epoch,
                    current: self.epoch,
                })
            } else {
                Ok(())
            }
        }

        pub fn apply_snapshot(
            &mut self,
            snapshot: CompositorSnapshot,
        ) -> Result<(), CompositorStateError> {
            self.check_epoch(snapshot.epoch)?;
            if self.phase != SessionPhase::AwaitingSnapshot {
                return Err(CompositorStateError::SnapshotAlreadyApplied);
            }
            let mut roles = BTreeSet::new();
            for overlay in &snapshot.overlays {
                roles.insert(match overlay.role {
                    OverlayRole::Keyboard => 0,
                    OverlayRole::Menu => 1,
                    OverlayRole::Toast => 2,
                });
            }
            if snapshot.overlays.len() != 3 || roles.len() != 3 {
                return Err(CompositorStateError::IncompleteSnapshot);
            }
            self.last_power_sequence = snapshot.power.last_sequence;
            self.snapshot = Some(snapshot);
            self.phase = SessionPhase::Active;
            Ok(())
        }

        pub fn snapshot(&self) -> Option<&CompositorSnapshot> {
            self.snapshot.as_ref()
        }

        pub fn accept_delta(&self, epoch: u64) -> Result<(), CompositorStateError> {
            self.check_epoch(epoch)?;
            if self.phase != SessionPhase::Active {
                return Err(CompositorStateError::SnapshotRequired);
            }
            Ok(())
        }

        pub fn begin_trusted_connection(
            &mut self,
            epoch: u64,
            request_id: u64,
            role: OverlayRole,
        ) -> Result<(), CompositorStateError> {
            self.accept_delta(epoch)?;
            if self.pending_requests.contains_key(&request_id) {
                return Err(CompositorStateError::UnexpectedRequestId(request_id));
            }
            if self.live_roles.contains(&role)
                || self
                    .pending_requests
                    .values()
                    .any(|pending| *pending == role)
            {
                return Err(CompositorStateError::RoleAlreadyBound(role));
            }
            self.pending_requests.insert(request_id, role);
            Ok(())
        }

        pub fn finish_trusted_connection(
            &mut self,
            epoch: u64,
            request_id: u64,
            fd_count: usize,
        ) -> Result<(), CompositorStateError> {
            self.accept_delta(epoch)?;
            let Some(&role) = self.pending_requests.get(&request_id) else {
                return Err(CompositorStateError::UnexpectedRequestId(request_id));
            };
            if fd_count == 0 {
                return Err(CompositorStateError::MissingFileDescriptor(request_id));
            }
            if fd_count != 1 {
                return Err(CompositorStateError::UnexpectedFileDescriptorCount {
                    request_id,
                    got: fd_count,
                });
            }
            self.pending_requests.remove(&request_id);
            self.live_roles.insert(role);
            Ok(())
        }

        /// A compositor refusal consumes the pending id and never carries an fd.
        pub fn refuse_trusted_connection(
            &mut self,
            epoch: u64,
            request_id: u64,
            fd_count: usize,
        ) -> Result<(), CompositorStateError> {
            self.accept_delta(epoch)?;
            if !self.pending_requests.contains_key(&request_id) {
                return Err(CompositorStateError::UnexpectedRequestId(request_id));
            }
            if fd_count != 0 {
                return Err(CompositorStateError::UnexpectedFileDescriptorCount {
                    request_id,
                    got: fd_count,
                });
            }
            self.pending_requests.remove(&request_id);
            Ok(())
        }

        /// Release one role when its trusted `wl_client` is destroyed. A later request may then
        /// mint a fresh socketpair/fd for that role; the previous fd is never reusable.
        pub fn release_trusted_role(
            &mut self,
            epoch: u64,
            role: OverlayRole,
        ) -> Result<(), CompositorStateError> {
            self.accept_delta(epoch)?;
            if !self.live_roles.remove(&role) {
                return Err(CompositorStateError::RoleNotBound(role));
            }
            Ok(())
        }

        pub fn protected_power(
            &mut self,
            epoch: u64,
            sequence: u64,
            state: PowerState,
        ) -> Result<PowerStep, CompositorStateError> {
            self.accept_delta(epoch)?;
            if sequence <= self.last_power_sequence {
                return Err(CompositorStateError::NonMonotonicPowerSequence {
                    got: sequence,
                    last: self.last_power_sequence,
                });
            }
            let pressed = state == PowerState::Pressed;
            if self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.power.pressed == pressed)
            {
                return Err(CompositorStateError::DuplicatePowerState { state });
            }
            self.last_power_sequence = sequence;
            if let Some(snapshot) = &mut self.snapshot {
                snapshot.power.pressed = pressed;
                snapshot.power.last_sequence = sequence;
            }
            Ok(PowerStep::Accepted)
        }

        pub fn disconnect(&mut self, menu_open: bool) -> DisconnectEffects {
            self.phase = SessionPhase::Disconnected;
            self.snapshot = None;
            let invalidated_request_ids = self.pending_requests.keys().copied().collect();
            self.pending_requests.clear();
            self.live_roles.clear();
            DisconnectEffects {
                secure_entry: true,
                quarantine_pad: true,
                close_role_peers: true,
                safe_return: menu_open,
                invalidated_request_ids,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Router outbound events (the capture lane consumes `secure_entry`)
// ---------------------------------------------------------------------------

pub mod events {
    use super::*;

    /// What the router publishes to its own subscribers. It is a relay: `secure_entry` is
    /// exactly gamescope's last `focus.secure_entry`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum RouterEvent {
        SecureEntry { active: bool },
        PadDestination { destination: PadDestination },
    }

    pub const TYPES: &[&str] = &["secure_entry", "pad_destination"];
}

// ---------------------------------------------------------------------------
// Pure state: the pad gate (ordering rule c) and the menu session (rules d, e)
// ---------------------------------------------------------------------------

/// Decides where the pad goes from the facts the router has, with no clock.
///
/// * The keyboard gets the pad only when pf-osk said `wants_pad: true` **and** gamescope said
///   the keyboard overlay is mapped. Either side alone is not enough.
/// * The pad leaves the keyboard on the first of: `wants_pad: false`, overlay unmapped, or the
///   keyboard connection closing. Failure is toward the app.
/// * An open menu wins over everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadGate {
    keyboard_wants_pad: bool,
    keyboard_mapped: bool,
    menu_open: bool,
}

impl PadGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn keyboard_visibility(&mut self, visible: bool, wants_pad: bool) {
        self.keyboard_wants_pad = visible && wants_pad;
    }

    pub fn keyboard_overlay(&mut self, state: compositor::OverlayState) {
        self.keyboard_mapped = state == compositor::OverlayState::Mapped;
    }

    /// pf-osk's connection closed: both halves of its claim are gone.
    pub fn keyboard_disconnected(&mut self) {
        self.keyboard_wants_pad = false;
        self.keyboard_mapped = false;
    }

    pub fn menu_open(&mut self, open: bool) {
        self.menu_open = open;
    }

    pub fn destination(&self) -> PadDestination {
        if self.menu_open {
            PadDestination::Menu
        } else if self.keyboard_wants_pad && self.keyboard_mapped {
            PadDestination::Keyboard
        } else {
            PadDestination::App
        }
    }
}

/// The router's view of one menu open, driven by elapsed time since the press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPhase {
    /// `system_menu` sent; waiting for `ack`.
    AwaitingAck,
    /// `ack` received; waiting for `shown`.
    AwaitingShown,
    /// Shown; the pad is with the menu; heartbeats run.
    Open,
    /// Terminal: the router took the pad back and (if `fallback`) ran the return path.
    Closed { fallback: bool },
}

/// What the router must do after an input to [`MenuSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuStep {
    /// Nothing to do.
    None,
    /// Re-point the pad to the menu (after synthesised releases on the app device).
    RoutePadToMenu,
    /// Re-point the pad to the app (after synthesised releases on the menu device).
    RoutePadToApp,
    /// Run the one return path (safe_return) and take the pad back.
    FallbackReturn,
    /// Send `dismiss{reason}` and take the pad back.
    Dismiss(menu::DismissReason),
    /// Send `dismiss{reason}`, take the pad back, and run the one safe return path.
    DismissAndReturn(menu::DismissReason),
}

/// Deterministic model of ordering rules (d) and (e): `press → ack ≤ 250 ms → shown ≤ 500 ms →
/// heartbeat every 500 ms, unhealthy after 1 s`. Time is `elapsed` since the press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuSession {
    phase: MenuPhase,
    acked_at: Option<Duration>,
    last_pong_at: Duration,
    outstanding_ping: Option<(u64, Duration)>,
}

impl Default for MenuSession {
    fn default() -> Self {
        Self::new()
    }
}

impl MenuSession {
    /// The press just went out as `system_menu`.
    pub fn new() -> Self {
        Self {
            phase: MenuPhase::AwaitingAck,
            acked_at: None,
            last_pong_at: Duration::ZERO,
            outstanding_ping: None,
        }
    }

    pub fn phase(&self) -> MenuPhase {
        self.phase
    }

    pub fn ack(&mut self, elapsed: Duration) -> MenuStep {
        if self.phase != MenuPhase::AwaitingAck {
            return MenuStep::None;
        }
        if elapsed > timeouts::MENU_ACK_TIMEOUT {
            self.phase = MenuPhase::Closed { fallback: true };
            return MenuStep::FallbackReturn;
        }
        self.phase = MenuPhase::AwaitingShown;
        self.acked_at = Some(elapsed);
        MenuStep::None
    }

    pub fn shown(&mut self, elapsed: Duration) -> MenuStep {
        let Some(acked_at) = self.acked_at else {
            return MenuStep::None;
        };
        if self.phase != MenuPhase::AwaitingShown {
            return MenuStep::None;
        }
        if elapsed.saturating_sub(acked_at) > timeouts::MENU_SHOWN_TIMEOUT {
            self.phase = MenuPhase::Closed { fallback: true };
            return MenuStep::DismissAndReturn(menu::DismissReason::ShownTimeout);
        }
        self.phase = MenuPhase::Open;
        self.last_pong_at = elapsed;
        self.outstanding_ping = None;
        MenuStep::RoutePadToMenu
    }

    /// Record a health probe sent by the router. Only the newest outstanding sequence is valid.
    pub fn ping(&mut self, seq: u64, elapsed: Duration) -> MenuStep {
        if self.phase == MenuPhase::Open {
            self.outstanding_ping = Some((seq, elapsed));
        }
        MenuStep::None
    }

    /// Accept a health reply only when it matches the outstanding probe and is fresh. A pong is
    /// one-shot: unsolicited, wrong-sequence, duplicate, stale and time-inverted replies cannot
    /// move the health deadline.
    pub fn pong(&mut self, seq: u64, elapsed: Duration) -> MenuStep {
        if self.phase != MenuPhase::Open {
            return MenuStep::None;
        }
        let Some((expected, sent_at)) = self.outstanding_ping else {
            return MenuStep::None;
        };
        if seq == expected
            && elapsed >= sent_at
            && elapsed.saturating_sub(sent_at) <= timeouts::PONG_MAX_AGE
        {
            self.outstanding_ping = None;
            self.last_pong_at = elapsed;
        }
        MenuStep::None
    }

    /// The provider closed its sheet or sent `return`.
    pub fn provider_closed(&mut self) -> MenuStep {
        match self.phase {
            MenuPhase::Closed { .. } => MenuStep::None,
            _ => {
                self.phase = MenuPhase::Closed { fallback: false };
                MenuStep::RoutePadToApp
            }
        }
    }

    /// Menu held past `MENU_HOLD_RETURN`, measured by the router from the hardware press.
    pub fn held(&mut self, held_for: Duration) -> MenuStep {
        if held_for < timeouts::MENU_HOLD_RETURN || matches!(self.phase, MenuPhase::Closed { .. }) {
            return MenuStep::None;
        }
        self.phase = MenuPhase::Closed { fallback: true };
        MenuStep::DismissAndReturn(menu::DismissReason::HoldReturn)
    }

    /// Advance the clock. Returns the first step the deadlines demand.
    pub fn tick(&mut self, elapsed: Duration) -> MenuStep {
        match self.phase {
            MenuPhase::AwaitingAck if elapsed > timeouts::MENU_ACK_TIMEOUT => {
                self.phase = MenuPhase::Closed { fallback: true };
                MenuStep::FallbackReturn
            }
            MenuPhase::AwaitingShown
                if elapsed.saturating_sub(self.acked_at.unwrap_or(Duration::ZERO))
                    > timeouts::MENU_SHOWN_TIMEOUT =>
            {
                self.phase = MenuPhase::Closed { fallback: true };
                MenuStep::DismissAndReturn(menu::DismissReason::ShownTimeout)
            }
            MenuPhase::Open
                if elapsed.saturating_sub(self.last_pong_at) > timeouts::MENU_HEALTH_TIMEOUT =>
            {
                self.phase = MenuPhase::Closed { fallback: false };
                MenuStep::Dismiss(menu::DismissReason::Unhealthy)
            }
            _ => MenuStep::None,
        }
    }
}
