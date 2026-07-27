//! The **emit model** — an in-memory candidate `capabilities.toml` and its TOML serializer.
//!
//! A candidate emitted by guided collection carries only what the schema REQUIRES to validate
//! (`identity` + `screens` + `inputs`) plus the observed axis ranges + trigger `semantics`.
//! `sensors`/`actuators`/`skin` are OPTIONAL and are left for later authoring passes (the skin
//! atlas, the LED/rumble/sensor rows) — this engine emits only what it can OBSERVE from presses.
//! Field names/values are matched EXACTLY to `platform/schemas/capabilities.schema.json`
//! (`additionalProperties:false` is strict), so the candidate passes `caps.py validate`.

/// A `struct input_absinfo`-shaped axis range, emitted for `range`/`x`/`y`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
    /// Observed rest/centre position (evdev `input_absinfo.value`), when this axis was measured
    /// from a live sweep rather than the driver's declared range. `None` for declared-only axes.
    pub value: Option<i32>,
}

/// One `[[inputs]]` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub id: String,
    pub kind: String,
    pub ev_type: String,
    /// The schema `code` string: a single symbol (`BTN_A`, `ABS_Z`) or a comma pair for
    /// two-axis controls (`ABS_X,ABS_Y`, `ABS_HAT0X,ABS_HAT0Y`).
    pub code: String,
    pub semantics: Option<String>,
    pub range: Option<Axis>,
    pub x: Option<Axis>,
    pub y: Option<Axis>,
}

/// The `[identity]` block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub manufacturer: String,
    pub model: String,
    pub sdl_guid: String,
    pub evdev_name: String,
    pub vid: Option<String>,
    pub pid: Option<String>,
}

/// One `[[screens]]` block (minimal render intent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    pub role: String,
    pub present: String,
    pub rotation: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
}

impl Screen {
    /// A minimal, always-valid primary screen (a landscape 1280x720 canvas). Guided collection
    /// records CONTROLS, not panel geometry — the real `display_rect`/rotation is authored later
    /// from the device's DTS + model, so the candidate carries only a schema-valid placeholder.
    pub fn minimal_primary() -> Screen {
        Screen {
            role: "primary".into(),
            present: "landscape".into(),
            rotation: "none".into(),
            canvas_w: 1280,
            canvas_h: 720,
        }
    }
}

/// A complete candidate descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub accept_default: Option<String>,
    pub identity: Identity,
    pub screens: Vec<Screen>,
    pub inputs: Vec<Input>,
}

/// Build the SDL2 joystick GUID from `(bus, vid, pid, version)` using the standard USB layout —
/// `le16(bus) 0000 le16(vid) 0000 le16(pid) 0000 le16(version) 0000` — the exact scheme SDL uses
/// for an evdev device with no CRC. (Verified against the a133's shipped
/// `030000005e0400008e02000010010000`.)
pub fn sdl_guid(bus: u16, vid: u16, pid: u16, version: u16) -> String {
    fn le16(x: u16) -> String {
        format!("{:02x}{:02x}", x & 0xff, (x >> 8) & 0xff)
    }
    format!(
        "{}0000{}0000{}0000{}0000",
        le16(bus),
        le16(vid),
        le16(pid),
        le16(version)
    )
}

fn axis_inline(a: &Axis) -> String {
    let mut s = format!("{{ min = {}, max = {}, fuzz = {}, flat = {}", a.min, a.max, a.fuzz, a.flat);
    if a.resolution != 0 {
        s.push_str(&format!(", resolution = {}", a.resolution));
    }
    if let Some(v) = a.value {
        s.push_str(&format!(", value = {v}"));
    }
    s.push_str(" }");
    s
}

impl Capabilities {
    /// Serialize to a `capabilities.toml` string that passes `caps.py validate`.
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "# CANDIDATE capabilities.toml — emitted by pf-input-collect (guided ground-truth\n\
             # collection, tsp-bwrg.2). Contains only what was OBSERVED from prompted presses:\n\
             # identity + a minimal primary screen + the input rows (codes, axis ranges, and\n\
             # binary/analog trigger semantics). skin_part / skin / sensors / actuators are\n\
             # left for later authoring passes — a candidate emits only what it can observe.\n\n",
        );
        if let Some(ad) = &self.accept_default {
            out.push_str(&format!("accept_default = \"{ad}\"\n\n"));
        }

        // [identity]
        out.push_str("[identity]\n");
        out.push_str(&format!("id           = \"{}\"\n", self.identity.id));
        out.push_str(&format!("manufacturer = \"{}\"\n", self.identity.manufacturer));
        out.push_str(&format!("model        = \"{}\"\n", self.identity.model));
        out.push_str(&format!("sdl_guid     = \"{}\"\n", self.identity.sdl_guid));
        let mut m = format!("evdev_name = \"{}\"", self.identity.evdev_name);
        if let Some(v) = &self.identity.vid {
            m.push_str(&format!(", vid = \"{v}\""));
        }
        if let Some(p) = &self.identity.pid {
            m.push_str(&format!(", pid = \"{p}\""));
        }
        out.push_str(&format!("match        = {{ {m} }}\n\n"));

        // [[screens]]
        for s in &self.screens {
            out.push_str("[[screens]]\n");
            out.push_str(&format!("role          = \"{}\"\n", s.role));
            out.push_str(&format!("present       = \"{}\"\n", s.present));
            out.push_str(&format!("rotation      = \"{}\"\n", s.rotation));
            out.push_str(&format!(
                "render_canvas = {{ w = {}, h = {} }}\n\n",
                s.canvas_w, s.canvas_h
            ));
        }

        // [[inputs]]
        for i in &self.inputs {
            out.push_str("[[inputs]]\n");
            out.push_str(&format!("id = \"{}\"\n", i.id));
            out.push_str(&format!("kind = \"{}\"\n", i.kind));
            out.push_str(&format!("ev_type = \"{}\"\n", i.ev_type));
            out.push_str(&format!("code = \"{}\"\n", i.code));
            if let Some(sem) = &i.semantics {
                out.push_str(&format!("semantics = \"{sem}\"\n"));
            }
            if let Some(r) = &i.range {
                out.push_str(&format!("range = {}\n", axis_inline(r)));
            }
            if let Some(x) = &i.x {
                out.push_str(&format!("x = {}\n", axis_inline(x)));
            }
            if let Some(y) = &i.y {
                out.push_str(&format!("y = {}\n", axis_inline(y)));
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdl_guid_matches_the_shipped_a133_value() {
        // bus=USB(3), vid=045e (MS), pid=028e (X360), version=0110.
        assert_eq!(sdl_guid(3, 0x045e, 0x028e, 0x0110), "030000005e0400008e02000010010000");
    }

    #[test]
    fn axis_inline_omits_zero_resolution() {
        let a = Axis { min: 0, max: 255, fuzz: 0, flat: 0, resolution: 0, value: None };
        assert_eq!(axis_inline(&a), "{ min = 0, max = 255, fuzz = 0, flat = 0 }");
        let b = Axis { min: -32768, max: 32767, fuzz: 16, flat: 128, resolution: 5, value: None };
        assert_eq!(
            axis_inline(&b),
            "{ min = -32768, max = 32767, fuzz = 16, flat = 128, resolution = 5 }"
        );
        // A measured axis carries its observed rest/centre as `value`.
        let c = Axis { min: 12, max: 4083, fuzz: 0, flat: 0, resolution: 0, value: Some(2097) };
        assert_eq!(axis_inline(&c), "{ min = 12, max = 4083, fuzz = 0, flat = 0, value = 2097 }");
    }
}
