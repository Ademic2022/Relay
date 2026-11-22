//! macOS injection via `CGEventPost` (requires Accessibility). Scroll uses
//! `CGEventCreateScrollWheelEvent2` in pixel units. M2.

use crate::*;

#[derive(Debug)]
pub struct CgEventInjector;

impl InputInjector for CgEventInjector {
    fn inject(&mut self, _event: &InputEvent) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    fn release_all(&mut self) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
}
