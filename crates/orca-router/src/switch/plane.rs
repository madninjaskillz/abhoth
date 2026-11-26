//! The orchestrator's [`Plane`] over the running router: slot views and leases through the
//! slots registry, guards through the [`GuardBoard`] (guard leases are one more guard), and
//! route changes through a [`RouteSink`] (the lifecycle's routing table plugs in there).

use std::sync::{Arc, Mutex};

use super::{Plane, Spec};
use crate::guards::{Action, Block, Caller, Guard, GuardBoard, Reservation, lease_reservations};
use crate::leases::SlotView;
use crate::proxy::now_epoch;
use crate::state::AppState;

/// Where routing changes go.
pub trait RouteSink: Send + Sync {
    /// These cards take nothing new (requests in flight finish).
    fn withdraw(&self, cards: &[String]);
    /// What runs now: `ready` cards are routable for `spec`, the rest are not.
    fn commit(&self, layout: Option<&str>, spec: &Spec, ready: &[String]);
}

/// One recorded commit: layout, spec, ready cards.
pub type Commit = (Option<String>, Spec, Vec<String>);

/// Records route changes (tests, and the observe-only mode).
#[derive(Default)]
pub struct RecordedRoutes {
    pub withdrawn: Mutex<Vec<Vec<String>>>,
    pub committed: Mutex<Vec<Commit>>,
}

impl RouteSink for RecordedRoutes {
    fn withdraw(&self, cards: &[String]) {
        self.withdrawn.lock().unwrap().push(cards.to_vec());
    }
    fn commit(&self, layout: Option<&str>, spec: &Spec, ready: &[String]) {
        self.committed.lock().unwrap().push((
            layout.map(String::from),
            spec.clone(),
            ready.to_vec(),
        ));
    }
}

/// Guard leases (pig-slots `guard` claims: benches, pipelines) as reservations.
pub struct LeaseGuard {
    st: Arc<AppState>,
}

impl Guard for LeaseGuard {
    fn id(&self) -> &str {
        "guard-leases"
    }
    fn reservations(&self, _now: f64) -> Vec<Reservation> {
        let snap = self.st.snapshot();
        lease_reservations(&self.st.ext.registry.live_guards(&snap))
    }
}

pub struct RouterPlane {
    st: Arc<AppState>,
    board: Arc<GuardBoard>,
    routes: Arc<dyn RouteSink>,
}

impl RouterPlane {
    /// Adds the guard-lease guard to `board`.
    pub fn new(st: Arc<AppState>, board: Arc<GuardBoard>, routes: Arc<dyn RouteSink>) -> Self {
        board.add_guard(Arc::new(LeaseGuard { st: st.clone() }));
        if let Some(cal) = crate::calendar::installed() {
            board.add_guard(cal.clone());
        }
        RouterPlane { st, board, routes }
    }

    pub fn board(&self) -> &Arc<GuardBoard> {
        &self.board
    }
}

impl Plane for RouterPlane {
    fn set_slots(&self, view: SlotView) {
        self.st.ext.registry.set_control_view(Some(view));
    }

    fn held_on(&self, cards: &[String]) -> Vec<(String, String)> {
        let snap = self.st.snapshot();
        self.st.ext.registry.held_on(&snap, cards)
    }

    fn blocks(
        &self,
        action: Action,
        cards: &[String],
        caller: &Caller,
        layout: &str,
    ) -> Vec<Block> {
        let layout = (!layout.is_empty()).then_some(layout);
        self.board.check(action, cards, caller, layout, now_epoch())
    }

    fn withdraw_routes(&self, cards: &[String]) {
        self.routes.withdraw(cards);
    }

    fn commit_routes(&self, layout: Option<&str>, spec: &Spec, ready: &[String]) {
        self.routes.commit(layout, spec, ready);
    }
}

#[cfg(test)]
mod plane_tests {
    use super::*;
    use crate::config::{RouterConfig, Snapshot};

    fn state() -> Arc<AppState> {
        let cfg = RouterConfig::default();
        AppState::new(Snapshot::build(cfg, None).unwrap(), None)
    }

    #[test]
    fn guard_leases_block_and_routes_are_passed_on() {
        let st = state();
        let board = Arc::new(GuardBoard::default());
        let routes = Arc::new(RecordedRoutes::default());
        let p = RouterPlane::new(st.clone(), board, routes.clone());
        let anyone = Caller::default();
        assert!(p.blocks(Action::Switch, &[], &anyone, "x").is_empty());
        st.ext
            .registry
            .claim_guard(&st.snapshot(), "bench-1", "bench running", "bench", None);
        let b = p.blocks(Action::OnDemand, &["B".into()], &anyone, "");
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!(b[0].source, "lease:bench-1");
        p.set_slots(SlotView {
            frozen: true,
            ..SlotView::default()
        });
        assert!(st.ext.registry.control_view().unwrap().frozen);
        p.withdraw_routes(&["B".into()]);
        p.commit_routes(Some("L"), &Spec::new(), &["A".into()]);
        assert_eq!(routes.withdrawn.lock().unwrap().len(), 1);
        assert_eq!(routes.committed.lock().unwrap()[0].2, ["A"]);
    }
}
