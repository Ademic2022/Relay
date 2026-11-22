//! Conversion between `relay_input::InputEvent` and its wire form.

use relay_input::{InputEvent, Modifiers, MouseButton};
use relay_proto::v1::{self, input_event::Event as E};

pub fn to_proto(ev: &InputEvent, ts_us: u64) -> v1::InputEvent {
    let event = match ev {
        InputEvent::MouseMove { x, y } => E::MouseMove(v1::MouseMove { x: *x, y: *y }),
        InputEvent::MouseButton { button, down } => E::MouseButton(v1::MouseButtonEvent {
            button: match button {
                MouseButton::Left => v1::MouseButton::Left,
                MouseButton::Right => v1::MouseButton::Right,
                MouseButton::Middle => v1::MouseButton::Middle,
                MouseButton::Back => v1::MouseButton::Back,
                MouseButton::Forward => v1::MouseButton::Forward,
            } as i32,
            down: *down,
        }),
        InputEvent::Scroll { dx, dy, precise } => E::Scroll(v1::Scroll {
            dx: *dx,
            dy: *dy,
            precise: *precise,
        }),
        InputEvent::Key {
            usage,
            down,
            modifiers,
        } => E::Key(v1::KeyEvent {
            usb_hid_usage: u32::from(*usage),
            down: *down,
            modifiers: modifiers.0,
        }),
        InputEvent::Text(t) => E::Text(v1::TextInput { text: t.clone() }),
    };
    v1::InputEvent {
        ts_us,
        event: Some(event),
    }
}

/// `None` for malformed or unknown events (ignored, never fatal).
pub fn from_proto(p: &v1::InputEvent) -> Option<InputEvent> {
    Some(match p.event.as_ref()? {
        E::MouseMove(m) if m.x.is_finite() && m.y.is_finite() => InputEvent::MouseMove {
            x: m.x.clamp(0.0, 1.0),
            y: m.y.clamp(0.0, 1.0),
        },
        E::MouseMove(_) => return None,
        E::MouseButton(b) => InputEvent::MouseButton {
            button: match v1::MouseButton::try_from(b.button).ok()? {
                v1::MouseButton::Left => MouseButton::Left,
                v1::MouseButton::Right => MouseButton::Right,
                v1::MouseButton::Middle => MouseButton::Middle,
                v1::MouseButton::Back => MouseButton::Back,
                v1::MouseButton::Forward => MouseButton::Forward,
            },
            down: b.down,
        },
        E::Scroll(s) if s.dx.is_finite() && s.dy.is_finite() => InputEvent::Scroll {
            // Bound a single event to something sane.
            dx: s.dx.clamp(-10_000.0, 10_000.0),
            dy: s.dy.clamp(-10_000.0, 10_000.0),
            precise: s.precise,
        },
        E::Scroll(_) => return None,
        E::Key(k) => InputEvent::Key {
            usage: u16::try_from(k.usb_hid_usage).ok()?,
            down: k.down,
            modifiers: Modifiers(k.modifiers),
        },
        E::Text(t) if t.text.chars().count() <= 256 => InputEvent::Text(t.text.clone()),
        E::Text(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let evs = [
            InputEvent::MouseMove { x: 0.25, y: 0.75 },
            InputEvent::MouseButton {
                button: MouseButton::Forward,
                down: true,
            },
            InputEvent::Scroll {
                dx: -1.5,
                dy: 40.0,
                precise: true,
            },
            InputEvent::Key {
                usage: 0xE3,
                down: false,
                modifiers: Modifiers(Modifiers::META),
            },
            InputEvent::Text("héllo".into()),
        ];
        for e in evs {
            assert_eq!(from_proto(&to_proto(&e, 7)).as_ref(), Some(&e));
        }
    }

    #[test]
    fn rejects_garbage() {
        let bad = v1::InputEvent {
            ts_us: 0,
            event: Some(E::MouseMove(v1::MouseMove {
                x: f32::NAN,
                y: 0.0,
            })),
        };
        assert!(from_proto(&bad).is_none());
        let bad = v1::InputEvent {
            ts_us: 0,
            event: Some(E::Key(v1::KeyEvent {
                usb_hid_usage: 70_000,
                down: true,
                modifiers: 0,
            })),
        };
        assert!(from_proto(&bad).is_none());
    }
}
