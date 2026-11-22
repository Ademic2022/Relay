//! Windows injection via `SendInput` with `KEYEVENTF_SCANCODE`. M2.
//! For the secure desktop the Agent must be attached to it (§10).

use crate::*;

#[derive(Debug)]
pub struct SendInputInjector;

impl InputInjector for SendInputInjector {
    fn inject(&mut self, _event: &InputEvent) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
    fn release_all(&mut self) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }
}
