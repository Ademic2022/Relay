//! Recording injector for tests.

use crate::*;

#[derive(Debug, Default)]
pub struct MockInjector {
    pub events: Vec<InputEvent>,
    pub released: usize,
}

impl InputInjector for MockInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        self.events.push(event.clone());
        Ok(())
    }
    fn release_all(&mut self) -> Result<(), InputError> {
        self.released += 1;
        Ok(())
    }
}
