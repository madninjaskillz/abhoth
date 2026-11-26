//! The engine instance state machine (ROUTER-CONFIG.md 5.1). One instance per (card, engine).
//!
//! ```text
//!             plan/start                     health ok        verify ok
//!   Off ──────────────────▶ Preparing ──▶ Starting ──────▶ Warming ──────▶ Ready
//!    ▲                           │             │                │              │
//!    │                           │fail         │fail/timeout    │fail          │ drain
//!    │                           ▼             ▼                ▼              ▼
//!    │                         Failed ◀────────┴────────────────┘          Draining
//!    │        stop ok            │ stop/clear                                  │
//!    └──────────────────────── Stopping ◀──────────────────────────────────────┘
//!   Unknown (drift, foreign, unlabelled) ──adopt / stop──▶ Ready / Stopping
//! ```
//!
//! Only `Ready` is routable (and `Draining` for requests already in flight). Health polling can
//! mark a Ready instance *unhealthy* (not routable, not stopped); only a gone process is Failed.
//! Nothing in this module can start a layout switch: transitions are driven by the lifecycle
//! manager and the orchestrator, never by a probe result alone.

use serde::{Deserialize, Serialize};

use super::schema::CardSpec;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    #[default]
    Off,
    Preparing,
    Starting,
    Warming,
    Ready,
    Draining,
    Stopping,
    Failed,
    Unknown,
}

impl InstanceState {
    pub const ALL: [InstanceState; 9] = [
        Self::Off,
        Self::Preparing,
        Self::Starting,
        Self::Warming,
        Self::Ready,
        Self::Draining,
        Self::Stopping,
        Self::Failed,
        Self::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Warming => "warming",
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.as_str() == s)
    }

    /// New requests may be sent here.
    pub fn routable(self) -> bool {
        self == Self::Ready
    }

    /// Requests already in flight may finish here.
    pub fn serves_in_flight(self) -> bool {
        matches!(self, Self::Ready | Self::Draining)
    }

    /// A process is (or may be) holding the card.
    pub fn holds_card(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Allowed transitions (5.1). `Failed` and `Unknown` can be stopped; `Unknown` can be
    /// adopted as Ready; any state may go to `Failed`, except `Off`.
    pub fn can_go(self, to: InstanceState) -> bool {
        use InstanceState::*;
        if self == to {
            return false;
        }
        match (self, to) {
            (Off, Preparing) => true,
            // Adoption at router start goes through Unknown -> Ready; a fresh observation of a
            // running process on an Off card is Unknown first.
            (Off, Unknown) => true,
            (Preparing, Starting) => true,
            (Starting, Warming) => true,
            (Warming, Ready) => true,
            (Ready, Draining) => true,
            (Draining, Stopping) => true,
            // A drain refused/cancelled (switch undone before stop).
            (Draining, Ready) => true,
            (Stopping, Off) => true,
            (Unknown, Ready) => true,
            (Unknown, Stopping) => true,
            (Unknown, Off) => true,
            (Failed, Stopping) => true,
            // Admin clear when nothing runs any more.
            (Failed, Off) => true,
            // Stops of instances that never became Ready (rollback of a half-done start).
            (Preparing | Starting | Warming, Stopping) => true,
            (Off, Failed) => false,
            (_, Failed) => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for InstanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an instance is not where it should be.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TransitionError {
    pub from: InstanceState,
    pub to: InstanceState,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal instance transition {} -> {}",
            self.from, self.to
        )
    }
}

impl std::error::Error for TransitionError {}

/// What runs (or should run) on one card. Persisted in `router.db` so a restarted router knows
/// what it last did, but the truth at start is always what the driver observes.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Instance {
    pub server: String,
    pub card: String,
    /// Empty when the card is off.
    pub engine: String,
    pub spec: Option<CardSpec>,
    pub state: InstanceState,
    /// Ready but failing health polls: not routable, not stopped.
    #[serde(default)]
    pub unhealthy: bool,
    /// Unix seconds of the last state change.
    pub since: i64,
    /// Human-readable reason of the last change (failure text, "adopted", …).
    #[serde(default)]
    pub detail: String,
    /// Started (or restarted) within the current job: only these get the verify chat.
    #[serde(default)]
    pub restarted_in_job: bool,
}

impl Instance {
    pub fn off(server: &str, card: &str) -> Self {
        Self {
            server: server.into(),
            card: card.into(),
            ..Default::default()
        }
    }

    /// Move to `to`, or refuse an illegal transition.
    pub fn go(
        &mut self,
        to: InstanceState,
        now: i64,
        detail: impl Into<String>,
    ) -> Result<(), TransitionError> {
        if !self.state.can_go(to) {
            return Err(TransitionError {
                from: self.state,
                to,
            });
        }
        self.state = to;
        self.since = now;
        self.detail = detail.into();
        if to != InstanceState::Ready {
            self.unhealthy = false;
        }
        Ok(())
    }

    pub fn routable(&self) -> bool {
        self.state.routable() && !self.unhealthy
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceState::*;
    use super::*;

    #[test]
    fn happy_path_and_back() {
        let mut i = Instance::off("s", "A");
        for s in [Preparing, Starting, Warming, Ready, Draining, Stopping, Off] {
            i.go(s, 1, "").unwrap();
        }
        assert_eq!(i.state, Off);
    }

    #[test]
    fn illegal_moves_refused() {
        assert!(!Off.can_go(Ready));
        assert!(!Off.can_go(Starting));
        assert!(!Ready.can_go(Starting));
        assert!(!Stopping.can_go(Ready));
        assert!(!Off.can_go(Failed));
        assert!(Starting.can_go(Failed));
        assert!(Ready.can_go(Failed));
        assert!(Unknown.can_go(Ready));
        assert!(Failed.can_go(Stopping));
        let mut i = Instance::off("s", "A");
        assert_eq!(
            i.go(Ready, 1, "x"),
            Err(TransitionError {
                from: Off,
                to: Ready
            })
        );
    }

    #[test]
    fn only_ready_and_healthy_is_routable() {
        for s in InstanceState::ALL {
            assert_eq!(s.routable(), s == Ready, "{s}");
            assert_eq!(InstanceState::parse(s.as_str()), Some(s));
        }
        let mut i = Instance {
            state: Ready,
            unhealthy: true,
            ..Default::default()
        };
        assert!(!i.routable());
        i.unhealthy = false;
        assert!(i.routable());
    }
}
