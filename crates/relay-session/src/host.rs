//! Host-side session state machine and its safety rules (FR-8 to FR-10,
//! FR-33, FR-39).
//!
//! Pure and clock-injected: the caller feeds events and executes the
//! returned [`Effect`]s (show dialog, show banner, write audit entry...).
//!
//! Invariants enforced here, never in UI code:
//! - An attended session is never active without the Host user accepting.
//! - The consent dialog auto-declines after [`CONSENT_TIMEOUT`].
//! - Whenever a session is active and a user is logged in, the banner is
//!   shown, unattended or not. The Controller has no event that hides it.
//! - Every session start and end produces an audit effect.

use std::time::{Duration, Instant};

pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub device_id: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// One-time password: requires Accept.
    Attended,
    /// Permanent password via the Service: skips Accept (FR-33).
    Unattended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Declined,
    ConsentTimeout,
    HostEnded,
    ControllerLeft,
    AuthFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Authenticating,
    AwaitingConsent { since: Instant },
    Active { since: Instant },
    Ended(EndReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    AuthSucceeded,
    AuthFailed,
    Accept,
    Decline,
    /// Banner button or Ctrl+Alt+Shift+X / ⌃⌥⇧X (FR-10).
    HostEndRequested,
    ControllerLeft,
    /// A user logged in or out at the Host (unattended sessions).
    UserPresence(bool),
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    ShowConsentDialog(Peer),
    CloseConsentDialog,
    ShowBanner(Peer),
    HideBanner,
    StartStreaming,
    StopStreaming,
    AuditStart {
        peer: Peer,
        access: Access,
    },
    AuditEnd {
        peer: Peer,
        reason: EndReason,
        duration: Duration,
    },
    /// Rotate the one-time password after each session (FR-2).
    RotateOneTimePassword,
}

#[derive(Debug)]
pub struct HostSession {
    peer: Peer,
    access: Access,
    user_logged_in: bool,
    banner_shown: bool,
    state: State,
}

impl HostSession {
    pub fn new(peer: Peer, access: Access, user_logged_in: bool) -> Self {
        Self {
            peer,
            access,
            user_logged_in,
            banner_shown: false,
            state: State::Authenticating,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn banner_shown(&self) -> bool {
        self.banner_shown
    }

    pub fn handle(&mut self, ev: Event, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        match (&self.state, ev) {
            (State::Ended(_), _) => {}

            (State::Authenticating, Event::AuthSucceeded) => match self.access {
                Access::Attended => {
                    self.state = State::AwaitingConsent { since: now };
                    fx.push(Effect::ShowConsentDialog(self.peer.clone()));
                }
                Access::Unattended => self.activate(now, &mut fx),
            },
            (State::Authenticating, Event::AuthFailed) => {
                self.state = State::Ended(EndReason::AuthFailed);
            }

            (State::AwaitingConsent { .. }, Event::Accept) => {
                fx.push(Effect::CloseConsentDialog);
                self.activate(now, &mut fx);
            }
            (State::AwaitingConsent { .. }, Event::Decline) => {
                fx.push(Effect::CloseConsentDialog);
                self.end_unstarted(EndReason::Declined, &mut fx);
            }
            (State::AwaitingConsent { since }, Event::Tick)
                if now.saturating_duration_since(*since) >= CONSENT_TIMEOUT =>
            {
                fx.push(Effect::CloseConsentDialog);
                self.end_unstarted(EndReason::ConsentTimeout, &mut fx);
            }
            (
                State::AwaitingConsent { .. },
                e @ (Event::HostEndRequested | Event::ControllerLeft),
            ) => {
                fx.push(Effect::CloseConsentDialog);
                let r = if e == Event::HostEndRequested {
                    EndReason::HostEnded
                } else {
                    EndReason::ControllerLeft
                };
                self.end_unstarted(r, &mut fx);
            }

            (State::Active { since }, Event::HostEndRequested) => {
                let since = *since;
                self.end_active(EndReason::HostEnded, since, now, &mut fx);
            }
            (State::Active { since }, Event::ControllerLeft) => {
                let since = *since;
                self.end_active(EndReason::ControllerLeft, since, now, &mut fx);
            }

            (_, Event::UserPresence(present)) => {
                self.user_logged_in = present;
                if matches!(self.state, State::Active { .. }) {
                    self.sync_banner(&mut fx);
                }
            }

            _ => {}
        }
        fx
    }

    fn activate(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        self.state = State::Active { since: now };
        fx.push(Effect::AuditStart {
            peer: self.peer.clone(),
            access: self.access,
        });
        self.sync_banner(fx);
        fx.push(Effect::StartStreaming);
    }

    fn sync_banner(&mut self, fx: &mut Vec<Effect>) {
        // Attended sessions always have a user present to accept, so the
        // banner is on. Unattended: on whenever someone is logged in.
        let want = self.access == Access::Attended || self.user_logged_in;
        if want && !self.banner_shown {
            fx.push(Effect::ShowBanner(self.peer.clone()));
        } else if !want && self.banner_shown {
            fx.push(Effect::HideBanner);
        }
        self.banner_shown = want;
    }

    fn end_unstarted(&mut self, reason: EndReason, fx: &mut Vec<Effect>) {
        self.state = State::Ended(reason);
        if self.access == Access::Attended {
            fx.push(Effect::RotateOneTimePassword);
        }
    }

    fn end_active(
        &mut self,
        reason: EndReason,
        since: Instant,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        self.state = State::Ended(reason);
        fx.push(Effect::StopStreaming);
        if self.banner_shown {
            self.banner_shown = false;
            fx.push(Effect::HideBanner);
        }
        fx.push(Effect::AuditEnd {
            peer: self.peer.clone(),
            reason,
            duration: now.saturating_duration_since(since),
        });
        if self.access == Access::Attended {
            fx.push(Effect::RotateOneTimePassword);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> Peer {
        Peer {
            device_id: 123_456_789,
            name: "Alice".into(),
        }
    }

    #[test]
    fn attended_requires_accept_and_shows_banner() {
        let t = Instant::now();
        let mut s = HostSession::new(peer(), Access::Attended, true);
        let fx = s.handle(Event::AuthSucceeded, t);
        assert_eq!(fx, vec![Effect::ShowConsentDialog(peer())]);
        assert!(!fx.contains(&Effect::StartStreaming));

        let fx = s.handle(Event::Accept, t);
        assert!(fx.contains(&Effect::ShowBanner(peer())));
        assert!(fx.contains(&Effect::StartStreaming));
        let audit = fx
            .iter()
            .position(|e| matches!(e, Effect::AuditStart { .. }));
        let stream = fx.iter().position(|e| *e == Effect::StartStreaming);
        assert!(audit < stream, "audit is written before streaming starts");
    }

    #[test]
    fn consent_times_out_after_30s() {
        let t = Instant::now();
        let mut s = HostSession::new(peer(), Access::Attended, true);
        s.handle(Event::AuthSucceeded, t);
        assert!(
            s.handle(Event::Tick, t + Duration::from_secs(29))
                .is_empty()
        );
        let fx = s.handle(Event::Tick, t + CONSENT_TIMEOUT);
        assert!(fx.contains(&Effect::CloseConsentDialog));
        assert_eq!(s.state(), &State::Ended(EndReason::ConsentTimeout));
        assert!(s.handle(Event::Accept, t + CONSENT_TIMEOUT).is_empty());
    }

    #[test]
    fn decline_never_streams() {
        let t = Instant::now();
        let mut s = HostSession::new(peer(), Access::Attended, true);
        s.handle(Event::AuthSucceeded, t);
        let fx = s.handle(Event::Decline, t);
        assert!(!fx.contains(&Effect::StartStreaming));
        assert_eq!(s.state(), &State::Ended(EndReason::Declined));
    }

    #[test]
    fn unattended_skips_consent_but_banner_follows_user() {
        let t = Instant::now();
        let mut s = HostSession::new(peer(), Access::Unattended, false);
        let fx = s.handle(Event::AuthSucceeded, t);
        assert!(!fx.iter().any(|e| matches!(e, Effect::ShowConsentDialog(_))));
        assert!(fx.contains(&Effect::StartStreaming));
        assert!(!s.banner_shown(), "nobody logged in");

        let fx = s.handle(Event::UserPresence(true), t);
        assert_eq!(fx, vec![Effect::ShowBanner(peer())]);
        let fx = s.handle(Event::UserPresence(false), t);
        assert_eq!(fx, vec![Effect::HideBanner]);
    }

    #[test]
    fn unattended_with_user_shows_banner_immediately() {
        let mut s = HostSession::new(peer(), Access::Unattended, true);
        let fx = s.handle(Event::AuthSucceeded, Instant::now());
        assert!(fx.contains(&Effect::ShowBanner(peer())));
    }

    #[test]
    fn host_can_always_end_and_audit_records_duration() {
        let t = Instant::now();
        let mut s = HostSession::new(peer(), Access::Attended, true);
        s.handle(Event::AuthSucceeded, t);
        s.handle(Event::Accept, t);
        let fx = s.handle(Event::HostEndRequested, t + Duration::from_secs(90));
        assert!(fx.contains(&Effect::StopStreaming));
        assert!(fx.contains(&Effect::HideBanner));
        assert!(fx.contains(&Effect::AuditEnd {
            peer: peer(),
            reason: EndReason::HostEnded,
            duration: Duration::from_secs(90),
        }));
        assert!(fx.contains(&Effect::RotateOneTimePassword));
    }
}
