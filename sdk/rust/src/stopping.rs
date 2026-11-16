//! A stopped plugin says so before it goes.
//!
//! SIGTERM is how homeCore and a plugin runtime stop a plugin. Rust's default
//! ends the process on the spot, so every device kept reading online with its
//! last state until something noticed. The run loop now answers SIGTERM and
//! Ctrl-C by running the plugin's stop handler, if it set one with
//! [`ManagementHandle::with_stop_handler`](crate::ManagementHandle::with_stop_handler),
//! marking every registered device offline, reporting the plugin offline, and
//! disconnecting. Then the process exits, since most plugins run the SDK loop
//! on a task of its own and have nothing else that would end them.

use crate::DeviceTracker;
use rumqttc::{AsyncClient, QoS};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// What a stop handler returns.
pub type StopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Set with [`ManagementHandle::with_stop_handler`](crate::ManagementHandle::with_stop_handler).
pub type StopHandler = Arc<dyn Fn() -> StopFuture + Send + Sync>;

/// How long the plugin's own handler may take. homeCore kills a plugin 2
/// seconds after asking it to stop, and the goodbye still has to go out.
pub(crate) const HANDLER_LIMIT: Duration = Duration::from_millis(1000);

/// From the signal to exiting, whatever happens: inside homeCore's 2 seconds.
pub(crate) const DEADLINE: Duration = Duration::from_millis(1800);

/// SIGTERM or Ctrl-C.
pub(crate) async fn signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                return;
            }
            Err(e) => warn!(error = %e, "Can't watch for SIGTERM; Ctrl-C only"),
        }
    }
    if tokio::signal::ctrl_c().await.is_err() {
        // Nothing to watch: never stop from here.
        std::future::pending::<()>().await;
    }
}

/// The plugin's handler, then every device offline, the plugin offline, and a
/// disconnect. Everything goes through the client's queue, in that order, so
/// the run loop sends it all before the DISCONNECT.
pub(crate) async fn goodbye(
    client: AsyncClient,
    plugin_id: String,
    devices: DeviceTracker,
    handler: Option<StopHandler>,
) {
    if let Some(handler) = handler {
        if tokio::time::timeout(HANDLER_LIMIT, handler())
            .await
            .is_err()
        {
            warn!(
                plugin_id = %plugin_id,
                "The plugin's stop handler took over a second; stopping anyway"
            );
        }
    }
    let mut ids: Vec<String> = devices.lock().unwrap().snapshot().into_iter().collect();
    ids.sort();
    for id in &ids {
        let topic = format!("homecore/devices/{id}/availability");
        let _ = client
            .publish(&topic, QoS::AtLeastOnce, true, "offline".as_bytes())
            .await;
    }
    let _ = client
        .publish(
            format!("homecore/plugins/{plugin_id}/status"),
            QoS::AtLeastOnce,
            true,
            "offline".as_bytes(),
        )
        .await;
    info!(plugin_id = %plugin_id, devices = ids.len(), "Plugin stopping; its devices are offline");
    let _ = client.disconnect().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceTrackerInner;
    use rumqttc::Request;
    use std::sync::Mutex;

    fn tracker(ids: &[&str]) -> DeviceTracker {
        let mut inner = DeviceTrackerInner::default();
        for id in ids {
            inner.set.insert((*id).to_string());
        }
        Arc::new(Mutex::new(inner))
    }

    fn said(rx: &flume::Receiver<Request>) -> Vec<String> {
        rx.drain()
            .map(|r| match r {
                Request::Publish(p) => format!(
                    "{} {} retained={}",
                    p.topic,
                    String::from_utf8_lossy(&p.payload),
                    p.retain
                ),
                Request::Disconnect(_) => "DISCONNECT".to_string(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn the_handler_runs_then_every_device_and_the_plugin_go_offline() {
        let (tx, rx) = flume::unbounded();
        let client = AsyncClient::from_senders(tx);
        let ran = Arc::new(Mutex::new(false));
        let ran2 = Arc::clone(&ran);
        let c2 = client.clone();
        let handler: StopHandler = Arc::new(move || {
            let ran = Arc::clone(&ran2);
            let c = c2.clone();
            Box::pin(async move {
                // What a plugin leaves safe goes out before the goodbye.
                let _ = c
                    .publish(
                        "homecore/devices/heat.a/cmd",
                        QoS::AtLeastOnce,
                        false,
                        "off",
                    )
                    .await;
                *ran.lock().unwrap() = true;
            })
        });

        goodbye(
            client,
            "plugin.test".into(),
            tracker(&["light.b", "light.a"]),
            Some(handler),
        )
        .await;

        assert!(*ran.lock().unwrap());
        assert_eq!(
            said(&rx),
            vec![
                "homecore/devices/heat.a/cmd off retained=false",
                "homecore/devices/light.a/availability offline retained=true",
                "homecore/devices/light.b/availability offline retained=true",
                "homecore/plugins/plugin.test/status offline retained=true",
                "DISCONNECT",
            ]
        );
    }

    #[tokio::test]
    async fn a_handler_that_hangs_does_not_stop_the_goodbye() {
        let (tx, rx) = flume::unbounded();
        let client = AsyncClient::from_senders(tx);
        let handler: StopHandler = Arc::new(|| Box::pin(std::future::pending()));

        goodbye(
            client,
            "plugin.test".into(),
            tracker(&["light.a"]),
            Some(handler),
        )
        .await;

        let said = said(&rx);
        assert_eq!(said.len(), 3, "{said:?}");
        assert_eq!(said.last().map(String::as_str), Some("DISCONNECT"));
    }
}
