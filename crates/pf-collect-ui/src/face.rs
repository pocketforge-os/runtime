//! The canonical gamepad FACE: a generic-gamepad diagram whose control hotspots are keyed to the
//! engine plan's positional ids (`south`, `east`, `dpad`, `lstick`, `ltrig`, ...). Guided collection
//! has NO target descriptor — it is BUILDING one — so the face is deliberately a GENERIC controller
//! reference (the shape the `default_gamepad_plan` intends), not a device-specific skin. It is laid
//! out on a fixed 1280x720 logical canvas; the frame is letterboxed to the real fb resolution.

/// The logical canvas the face is authored against.
pub const CANVAS_W: i32 = 1280;
pub const CANVAS_H: i32 = 720;

#[derive(Clone, Copy)]
pub enum Shape {
    Circle { r: i32 },
    /// A rectangle centered on (cx, cy).
    Rect { w: i32, h: i32 },
    /// A d-pad plus: two arms of half-length `arm`, `thick` wide.
    Cross { arm: i32, thick: i32 },
}

pub struct Control {
    /// The engine plan id this hotspot highlights (`Collector::current().id`).
    pub id: &'static str,
    pub cx: i32,
    pub cy: i32,
    pub shape: Shape,
    /// A short on-face label (A/B/X/Y/L1/...); position, not the id, is what the user reads.
    pub label: &'static str,
    /// Draw the label centered INSIDE the control (for the wide trigger/shoulder bars, where a
    /// below-label would collide with the neighbour) rather than below it.
    pub label_inside: bool,
}

/// The generic-gamepad face, one `Control` per `default_gamepad_plan` id. Stick-clicks (`l3`/`r3`)
/// share their stick's center as a smaller inner hotspot.
pub fn generic_face() -> Vec<Control> {
    use Shape::*;
    vec![
        // Triggers (top edge, above the shoulders) — labels INSIDE (below would hit the shoulders).
        Control { id: "ltrig", cx: 415, cy: 128, shape: Rect { w: 120, h: 30 }, label: "L2", label_inside: true },
        Control { id: "rtrig", cx: 865, cy: 128, shape: Rect { w: 120, h: 30 }, label: "R2", label_inside: true },
        // Shoulders — labels INSIDE.
        Control { id: "l1", cx: 415, cy: 174, shape: Rect { w: 120, h: 28 }, label: "L1", label_inside: true },
        Control { id: "r1", cx: 865, cy: 174, shape: Rect { w: 120, h: 28 }, label: "R1", label_inside: true },
        // Left cluster: d-pad (upper), left stick (lower).
        Control { id: "dpad", cx: 430, cy: 268, shape: Cross { arm: 42, thick: 30 }, label: "DP", label_inside: false },
        Control { id: "lstick", cx: 430, cy: 408, shape: Circle { r: 44 }, label: "LS", label_inside: false },
        Control { id: "l3", cx: 430, cy: 408, shape: Circle { r: 20 }, label: "L3", label_inside: true },
        // Right cluster: face-button diamond (upper), right stick (lower).
        Control { id: "north", cx: 850, cy: 218, shape: Circle { r: 27 }, label: "Y", label_inside: true },
        Control { id: "south", cx: 850, cy: 308, shape: Circle { r: 27 }, label: "A", label_inside: true },
        Control { id: "west", cx: 805, cy: 263, shape: Circle { r: 27 }, label: "X", label_inside: true },
        Control { id: "east", cx: 895, cy: 263, shape: Circle { r: 27 }, label: "B", label_inside: true },
        Control { id: "rstick", cx: 850, cy: 408, shape: Circle { r: 44 }, label: "RS", label_inside: false },
        Control { id: "r3", cx: 850, cy: 408, shape: Circle { r: 20 }, label: "R3", label_inside: true },
        // Center system cluster.
        Control { id: "select", cx: 590, cy: 318, shape: Rect { w: 48, h: 22 }, label: "SEL", label_inside: false },
        Control { id: "start", cx: 690, cy: 318, shape: Rect { w: 48, h: 22 }, label: "STA", label_inside: false },
        Control { id: "guide", cx: 640, cy: 258, shape: Circle { r: 22 }, label: "MENU", label_inside: false },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn face_covers_every_plan_id() {
        // The face must have a hotspot for every id the engine plan can prompt for, or a prompt
        // would highlight nothing. (l3/r3 share stick centers but still get their own entry.)
        let face_ids: HashSet<&str> = generic_face().iter().map(|c| c.id).collect();
        let plan_ids = [
            "south", "east", "west", "north", "select", "start", "guide", "l1", "r1", "dpad",
            "lstick", "rstick", "l3", "r3", "ltrig", "rtrig",
        ];
        for id in plan_ids {
            assert!(face_ids.contains(id), "face is missing a hotspot for plan id '{id}'");
        }
    }

    #[test]
    fn all_hotspots_are_inside_the_canvas() {
        for c in generic_face() {
            assert!(c.cx > 0 && c.cx < CANVAS_W, "{} cx off-canvas", c.id);
            assert!(c.cy > 0 && c.cy < CANVAS_H, "{} cy off-canvas", c.id);
        }
    }
}
