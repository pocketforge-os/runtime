//! The engine's value type. It is `Copy` so effects never carry an owned string; a choice
//! value is an option index and the host reads the option text from the item.

/// A value held by a toggle, choice, stepper, slider or progress item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuValue {
    Bool(bool),
    Int(i64),
    /// Index into the choice item's `options`.
    Choice(u16),
}
