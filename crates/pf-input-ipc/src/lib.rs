//! # pf-input-ipc — the router ↔ gamescope ↔ pf-osk ↔ system-menu contract
//!
//! Frozen interface for the PocketForge system input layer (epic `tsp-ihne1`, bead
//! `tsp-ihne1.36`). The human-readable contract is [`docs/input-ipc.md`](../../docs/input-ipc.md);
//! this crate is its machine-checkable half: the socket constants, the timeout constants, the
//! message types of every channel, the versioned envelope, and two pure state machines
//! (`MenuSession`, `PadGate`) that encode the ordering rules so the router and its tests share one
//! definition.
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
/// the image's tmpfiles entry. Apps share the session uid, so admission on every endpoint is
/// `SO_PEERCRED` uid == session uid **and** `SO_PEERGROUPS` contains the endpoint's group.
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
    pub const COMPOSITOR_GROUP: &str = "pf-input-compositor";

    /// pf-osk connects here (role `keyboard`); it receives its private pad fd on `registered`.
    pub const KEYBOARD_SOCKET: &str = "/run/pocketforge/input/keyboard.sock";
    pub const KEYBOARD_GROUP: &str = "pf-input-keyboard";

    /// The system menu connects here (role `menu`); it receives its private pad fd on
    /// `registered`. This is the endpoint `pf-input-broker --system-menu-sock` already serves.
    pub const MENU_SOCKET: &str = "/run/pocketforge/input/menu.sock";
    pub const MENU_GROUP: &str = "pf-input-menu";

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    Menu,
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
        /// Registration accepted. Carries, by `SCM_RIGHTS`, the read fd of the menu's private
        /// virtual pad (and, once provenance landed, the trusted Wayland client fd).
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
        /// Registration accepted. Carries the keyboard's private pad fd by `SCM_RIGHTS`.
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
        /// Sent on the frame the focus, the text-input state or `secure_entry` changes.
        /// `secure_entry` is computed here and only here.
        Focus {
            surface: FocusedSurface,
            text_input: TextInputState,
            secure_entry: bool,
        },
        /// A trusted overlay was mapped or unmapped.
        Overlay {
            role: OverlayRole,
            state: OverlayState,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            rect: Option<Rect>,
        },
        /// A real device produced input. Sent on the first event of a class and on every class
        /// change, never per event. The router owns modality.
        PhysicalInput {
            class: PhysicalInputClass,
        },
        /// Answer to `trusted_connection`.
        TrustedConnectionResult {
            request_id: u64,
            result: TrustedConnectionOutcome,
        },
        Pong {
            seq: u64,
        },
    }

    /// router → gamescope.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum RouterToCompositor {
        Registered,
        /// The provenance API. gamescope creates a socketpair, registers the server-side
        /// `wl_client` as trusted **for `role` only**, then replies `trusted_connection_result`
        /// with the client end. The router hands that fd to the system-launched process.
        TrustedConnection {
            request_id: u64,
            role: OverlayRole,
            /// systemd unit of the process that will own the connection (diagnostics only;
            /// never an input to the trust decision).
            unit: String,
        },
        /// Relay of a trusted surface's own `secure_entry` flag (menu channel).
        SurfaceSecureEntry {
            role: OverlayRole,
            active: bool,
        },
        Ping {
            seq: u64,
        },
    }

    pub const TO_ROUTER_TYPES: &[&str] = &[
        "register",
        "focus",
        "overlay",
        "physical_input",
        "trusted_connection_result",
        "pong",
    ];
    pub const FROM_ROUTER_TYPES: &[&str] = &[
        "registered",
        "trusted_connection",
        "surface_secure_entry",
        "ping",
    ];
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
}

/// Deterministic model of ordering rules (d) and (e): `press → ack ≤ 250 ms → shown ≤ 500 ms →
/// heartbeat every 500 ms, unhealthy after 1 s`. Time is `elapsed` since the press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuSession {
    phase: MenuPhase,
    acked_at: Option<Duration>,
    last_pong_at: Duration,
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
            return MenuStep::Dismiss(menu::DismissReason::ShownTimeout);
        }
        self.phase = MenuPhase::Open;
        self.last_pong_at = elapsed;
        MenuStep::RoutePadToMenu
    }

    pub fn pong(&mut self, elapsed: Duration) -> MenuStep {
        if self.phase == MenuPhase::Open {
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
        MenuStep::Dismiss(menu::DismissReason::HoldReturn)
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
                MenuStep::Dismiss(menu::DismissReason::ShownTimeout)
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
