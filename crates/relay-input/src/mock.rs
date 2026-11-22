//! Recording injector for tests.

use crate::*;

#[derive(Debug, Default)]
pub struct MockInjector {
    pub events: Vec<InputEvent>,
    pub released: usize,
    pub target: Option<DisplayTarget>,
}

impl InputInjector for MockInjector {
    fn set_display(&mut self, target: DisplayTarget) {
        self.target = Some(target);
    }
    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        self.events.push(event.clone());
        Ok(())
    }
    fn release_all(&mut self) -> Result<(), InputError> {
        self.released += 1;
        Ok(())
    }
}
