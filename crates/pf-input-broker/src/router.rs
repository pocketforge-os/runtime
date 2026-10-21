//! Deterministic policy core for the session-wide input router.
//!
//! Hardware and socket ownership stay in the daemon.  This module contains the ordering and
//! validation decisions which must be independently testable without evdev, uinput or gamescope.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Duration;

use pf_input_ipc::{timeouts, MenuSession, MenuStep};

use crate::safe_return::GUIDE_CODE;

/// A mutually-exclusive destination for the built-in pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Destination {
    App,
    Keyboard,
    Pointer,
    Shell,
    Menu,
    Quarantined,
}

impl Destination {
    pub const ALL: [Self; 6] = [
        Self::App,
        Self::Keyboard,
        Self::Pointer,
        Self::Shell,
        Self::Menu,
        Self::Quarantined,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonEvent {
    pub code: u16,
    pub pressed: bool,
}

impl ButtonEvent {
    pub const fn new(code: u16, pressed: bool) -> Self {
        Self { code, pressed }
    }
}

/// Ordered effects emitted by [`SessionRouter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteEffect {
    MenuPress,
    Button {
        destination: Destination,
        code: u16,
        pressed: bool,
    },
    Sync {
        destination: Destination,
    },
    DestinationChanged(Destination),
    DismissMenu,
    SafeReturn,
}

#[derive(Debug)]
pub struct SessionRouter {
    destination: Destination,
    destination_before_menu: Destination,
    pressed: HashMap<Destination, BTreeSet<u16>>,
    guide_down: bool,
    menu_started_at: Option<Duration>,
    menu: Option<MenuSession>,
}

impl SessionRouter {
    pub fn new(destination: Destination) -> Self {
        Self {
            destination,
            destination_before_menu: destination,
            pressed: HashMap::new(),
            guide_down: false,
            menu_started_at: None,
            menu: None,
        }
    }

    pub fn destination(&self) -> Destination {
        self.destination
    }

    /// Route one decoded button.  Guide/Menu is consumed before consulting the destination.
    pub fn route_button(&mut self, event: ButtonEvent, now: Duration) -> Vec<RouteEffect> {
        if event.code == GUIDE_CODE {
            let rising = event.pressed && !self.guide_down;
            self.guide_down = event.pressed;
            if rising {
                self.destination_before_menu = self.destination;
                self.menu_started_at = Some(now);
                self.menu = Some(MenuSession::new());
                return vec![RouteEffect::MenuPress];
            }
            return Vec::new();
        }

        let held = self.pressed.entry(self.destination).or_default();
        if event.pressed {
            held.insert(event.code);
        } else {
            held.remove(&event.code);
        }
        vec![RouteEffect::Button {
            destination: self.destination,
            code: event.code,
            pressed: event.pressed,
        }]
    }

    /// Apply an authoritative post-drop button snapshot.  Menu is always reconciled first.
    pub fn resynchronize<I>(&mut self, snapshot: I, now: Duration) -> Vec<RouteEffect>
    where
        I: IntoIterator<Item = (u16, bool)>,
    {
        let snapshot: Vec<_> = snapshot.into_iter().collect();
        let mut effects = Vec::new();
        if let Some((_, pressed)) = snapshot.iter().find(|(code, _)| *code == GUIDE_CODE) {
            effects.extend(self.route_button(ButtonEvent::new(GUIDE_CODE, *pressed), now));
        }
        for (code, pressed) in snapshot {
            if code != GUIDE_CODE {
                effects.extend(self.route_button(ButtonEvent::new(code, pressed), now));
            }
        }
        effects
    }

    /// Switch destinations, releasing every held button on the old virtual device first.
    pub fn switch_to(&mut self, destination: Destination) -> Vec<RouteEffect> {
        if destination == self.destination {
            return Vec::new();
        }
        let old = self.destination;
        let held = self.pressed.entry(old).or_default();
        let mut effects: Vec<_> = held
            .iter()
            .copied()
            .map(|code| RouteEffect::Button {
                destination: old,
                code,
                pressed: false,
            })
            .collect();
        if !held.is_empty() {
            effects.push(RouteEffect::Sync { destination: old });
            held.clear();
        }
        self.destination = destination;
        effects.push(RouteEffect::DestinationChanged(destination));
        effects
    }

    pub fn menu_ack(&mut self, now: Duration) -> Vec<RouteEffect> {
        self.menu_step(now, |menu, elapsed| menu.ack(elapsed))
    }

    pub fn menu_shown(&mut self, now: Duration) -> Vec<RouteEffect> {
        self.menu_step(now, |menu, elapsed| menu.shown(elapsed))
    }

    pub fn menu_ping(&mut self, sequence: u64, now: Duration) -> Vec<RouteEffect> {
        self.menu_step(now, |menu, elapsed| menu.ping(sequence, elapsed))
    }

    pub fn menu_pong(&mut self, sequence: u64, now: Duration) -> Vec<RouteEffect> {
        self.menu_step(now, |menu, elapsed| menu.pong(sequence, elapsed))
    }

    pub fn menu_closed(&mut self) -> Vec<RouteEffect> {
        let Some(menu) = self.menu.as_mut() else {
            return Vec::new();
        };
        let step = menu.provider_closed();
        self.apply_menu_step(step)
    }

    pub fn tick(&mut self, now: Duration) -> Vec<RouteEffect> {
        let Some(started) = self.menu_started_at else {
            return Vec::new();
        };
        let elapsed = now.saturating_sub(started);
        let Some(menu) = self.menu.as_mut() else {
            return Vec::new();
        };
        let step = if self.guide_down && elapsed >= timeouts::MENU_HOLD_RETURN {
            menu.held(elapsed)
        } else {
            menu.tick(elapsed)
        };
        self.apply_menu_step(step)
    }

    fn menu_step(
        &mut self,
        now: Duration,
        apply: impl FnOnce(&mut MenuSession, Duration) -> MenuStep,
    ) -> Vec<RouteEffect> {
        let Some(started) = self.menu_started_at else {
            return Vec::new();
        };
        let Some(menu) = self.menu.as_mut() else {
            return Vec::new();
        };
        let step = apply(menu, now.saturating_sub(started));
        self.apply_menu_step(step)
    }

    fn apply_menu_step(&mut self, step: MenuStep) -> Vec<RouteEffect> {
        match step {
            MenuStep::None => Vec::new(),
            MenuStep::RoutePadToMenu => self.switch_to(Destination::Menu),
            MenuStep::RoutePadToApp => self.switch_to(self.destination_before_menu),
            MenuStep::FallbackReturn => vec![RouteEffect::SafeReturn],
            MenuStep::Dismiss(_) => {
                let mut effects = vec![RouteEffect::DismissMenu];
                effects.extend(self.switch_to(self.destination_before_menu));
                effects
            }
            MenuStep::DismissAndReturn(_) => {
                vec![RouteEffect::DismissMenu, RouteEffect::SafeReturn]
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerFrame {
    pub dx_per_second: f64,
    pub dy_per_second: f64,
}

impl PointerFrame {
    pub const ZERO: Self = Self {
        dx_per_second: 0.0,
        dy_per_second: 0.0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Axis {
    None,
    Vertical(f64),
    Horizontal(f64),
}

#[derive(Debug, Clone, Copy)]
struct PointerAssist {
    strength: f64,
    expires_at: Duration,
}

#[derive(Debug, Default)]
pub struct PointerEngine {
    assist: Option<PointerAssist>,
}

impl PointerEngine {
    pub fn set_assist(&mut self, strength: f64, now: Duration) -> Result<(), &'static str> {
        if !(0.0..=1.0).contains(&strength) || !strength.is_finite() {
            return Err("assist strength must be finite and in 0..=1");
        }
        self.assist = Some(PointerAssist {
            strength,
            expires_at: now + Duration::from_millis(120),
        });
        Ok(())
    }

    /// 10% radial deadzone, rescaled then squared, with a 1280 px/s full-scale speed.
    pub fn frame(&self, x: f64, y: f64, precision: bool, now: Duration) -> PointerFrame {
        let magnitude = x.hypot(y).min(1.0);
        if !magnitude.is_finite() || magnitude <= 0.1 {
            return PointerFrame::ZERO;
        }
        let curved = ((magnitude - 0.1) / 0.9).powi(2);
        let mut speed = curved * 1280.0;
        if precision {
            speed *= 0.35;
        }
        // Assist is a bounded slow-speed damping hint, never a coordinate or a snap target.
        if magnitude < 0.5 {
            if let Some(assist) = self.assist.filter(|assist| now <= assist.expires_at) {
                speed *= 1.0 - assist.strength * 0.5;
            }
        }
        PointerFrame {
            dx_per_second: x / magnitude * speed,
            dy_per_second: y / magnitude * speed,
        }
    }

    /// Emit only the dominant high-resolution scroll axis.
    pub fn scroll_axis(vertical: f64, horizontal: f64) -> Axis {
        if vertical == 0.0 && horizontal == 0.0 {
            Axis::None
        } else if vertical.abs() >= horizontal.abs() {
            Axis::Vertical(vertical)
        } else {
            Axis::Horizontal(horizontal)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMenuKind {
    Action,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppMenuItem {
    pub id: String,
    pub label: String,
    pub kind: AppMenuKind,
}

impl AppMenuItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: AppMenuKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSectionError {
    TooLarge,
    TooManyItems,
    RateLimited,
    InvalidLabel,
    InvalidId,
}

#[derive(Debug, Default)]
pub struct AppSectionStore {
    items: HashMap<String, Vec<AppMenuItem>>,
    updates: HashMap<String, VecDeque<Duration>>,
}

impl AppSectionStore {
    pub fn items(&self, app_id: &str) -> Option<&[AppMenuItem]> {
        self.items.get(app_id).map(Vec::as_slice)
    }

    /// Validate the complete replacement before changing either content or rate state.
    pub fn update(
        &mut self,
        app_id: &str,
        replacement: Vec<AppMenuItem>,
        encoded_bytes: usize,
        now: Duration,
    ) -> Result<(), AppSectionError> {
        if encoded_bytes > 4096 {
            return Err(AppSectionError::TooLarge);
        }
        if replacement.len() > 8 {
            return Err(AppSectionError::TooManyItems);
        }
        if replacement.iter().any(|item| {
            item.id.is_empty()
                || item.id.len() > 64
                || !item
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        }) {
            return Err(AppSectionError::InvalidId);
        }
        if replacement.iter().any(|item| !valid_app_label(&item.label)) {
            return Err(AppSectionError::InvalidLabel);
        }

        let window = self.updates.entry(app_id.to_owned()).or_default();
        while window
            .front()
            .is_some_and(|at| now.saturating_sub(*at) >= Duration::from_secs(1))
        {
            window.pop_front();
        }
        if window.len() >= 10 {
            return Err(AppSectionError::RateLimited);
        }
        window.push_back(now);
        self.items.insert(app_id.to_owned(), replacement);
        Ok(())
    }
}

fn valid_app_label(label: &str) -> bool {
    if label.is_empty() || label.chars().count() > 24 {
        return false;
    }
    if label.chars().any(|character| {
        matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            || matches!(character, '\u{0370}'..='\u{03ff}' | '\u{0400}'..='\u{052f}')
    }) {
        return false;
    }
    let normalized = label.trim().to_ascii_lowercase();
    let first_word = normalized
        .split(|character: char| !character.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    !matches!(first_word, "close" | "return" | "home" | "power")
}
