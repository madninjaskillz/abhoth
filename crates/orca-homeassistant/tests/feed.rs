//! `feed::follow` against a fake hub: snapshot on subscribe, merge patches, lag recovery.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use orca_homeassistant::feed::{CardConfig, follow};
use orca_homeassistant::model::{HaSnapshot, ServerInfo};
use orca_sensors::feed::{FeedMessage, Resume, Snapshot, Subscription, TelemetryFeed};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

struct FakeHub {
    state: Mutex<Value>,
    tx: broadcast::Sender<FeedMessage>,
}

impl TelemetryFeed for FakeHub {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            seq: 1,
            ts: 0,
            state: self.state.lock().unwrap().clone(),
        }
    }
    fn subscribe(&self, _last: Option<u64>) -> Subscription {
        Subscription {
            start: Resume::Snapshot(self.snapshot()),
            rx: self.tx.subscribe(),
        }
    }
}

async fn next(
    rx: &mut watch::Receiver<HaSnapshot>,
    pred: impl Fn(&HaSnapshot) -> bool,
) -> HaSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let s = rx.borrow_and_update();
                if pred(&s) {
                    return s.clone();
                }
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("snapshot never matched")
}

#[tokio::test]
async fn follows_snapshot_and_patches() {
    let (tx, _) = broadcast::channel(4);
    let hub = Arc::new(FakeHub {
        state: Mutex::new(json!({"engines": {"layout": "flash-x2", "layouts": ["flash-x2"]}})),
        tx: tx.clone(),
    });
    let server = ServerInfo {
        id: "deepthought".into(),
        name: "DeepThought".into(),
        ..Default::default()
    };
    let (stx, mut srx) = watch::channel(HaSnapshot::default());
    let task = tokio::spawn(follow(
        hub.clone(),
        server,
        vec![CardConfig {
            id: "A".into(),
            allow_cap: false,
        }],
        stx,
    ));
    let s = next(&mut srx, |s| {
        s.control.layout.as_deref() == Some("flash-x2")
    })
    .await;
    assert_eq!(s.server.id, "deepthought");
    assert!(!s.cards[0].cap_offered && !s.cards[0].present);

    tx.send(FeedMessage::Patch {
        seq: 2,
        ts: 0,
        patch: json!({"engines": {"layout": "dense-x2"}}),
    })
    .unwrap();
    next(&mut srx, |s| {
        s.control.layout.as_deref() == Some("dense-x2")
    })
    .await;

    // A lagging subscriber resubscribes from a fresh snapshot.
    *hub.state.lock().unwrap() = json!({"engines": {"layout": "merged-x2"}});
    for i in 0..10 {
        tx.send(FeedMessage::Patch {
            seq: 3 + i,
            ts: 0,
            patch: json!({"engines": {"slots_busy": i}}),
        })
        .unwrap();
    }
    next(&mut srx, |s| {
        s.control.layout.as_deref() == Some("merged-x2")
    })
    .await;
    task.abort();
}
