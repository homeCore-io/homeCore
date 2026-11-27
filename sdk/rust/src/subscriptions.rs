use rumqttc::{
    Event, EventLoop, Outgoing, Packet, Request, Subscribe, SubscribeFilter, SubscribeReasonCode,
};
use std::time::Duration;
use tokio::sync::watch;

/// Broker acceptance of the subscriptions for the current connection.
#[derive(Clone, Debug, Default)]
pub enum SubscriptionState {
    #[default]
    Disconnected,
    AwaitingAcknowledgement,
    Acknowledged(Vec<bool>),
}

impl SubscriptionState {
    pub fn connected(&self) -> bool {
        !matches!(self, Self::Disconnected)
    }

    /// Check acceptance in the same order as the requested filters.
    pub fn accepted(&self, index: usize) -> bool {
        matches!(self, Self::Acknowledged(results) if results.get(index) == Some(&true))
    }
}

#[derive(Clone)]
pub struct SubscriptionUpdates(pub(crate) watch::Receiver<SubscriptionState>);

impl SubscriptionUpdates {
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.0.changed().await
    }

    pub fn accepted(&self, index: usize) -> bool {
        self.0.borrow().accepted(index)
    }

    /// Read the current connection state, giving a pending acknowledgement
    /// up to `deadline` to arrive. A pending subscription is unusable.
    pub async fn wait_for_acknowledgement(
        &mut self,
        deadline: Duration,
    ) -> Result<SubscriptionState, watch::error::RecvError> {
        let wait = async {
            self.0
                .wait_for(|state| !matches!(state, SubscriptionState::AwaitingAcknowledgement))
                .await
                .map(|state| state.clone())
        };
        match tokio::time::timeout(deadline, wait).await {
            Ok(result) => result,
            Err(_) => Ok(self.0.borrow_and_update().clone()),
        }
    }
}

pub(crate) struct Subscriptions {
    pub(crate) filters: Vec<SubscribeFilter>,
    pub(crate) state: watch::Sender<SubscriptionState>,
    pub(crate) packet_id: Option<u16>,
}

impl Subscriptions {
    pub(crate) fn observe(
        &mut self,
        event: &Result<Event, rumqttc::ConnectionError>,
        driver: &mut EventLoop,
    ) {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                self.packet_id = None;
                if self.filters.is_empty() {
                    self.state
                        .send_replace(SubscriptionState::Acknowledged(Vec::new()));
                } else {
                    self.state
                        .send_replace(SubscriptionState::AwaitingAcknowledgement);
                    // The driver owns this subscription and sends it first on
                    // each connection without waiting on its own request queue.
                    driver
                        .pending
                        .push_front(Request::Subscribe(Subscribe::new_many(
                            self.filters.clone(),
                        )));
                }
            }
            Ok(Event::Outgoing(Outgoing::Subscribe(id)))
                if matches!(
                    *self.state.borrow(),
                    SubscriptionState::AwaitingAcknowledgement
                ) && self.packet_id.is_none() =>
            {
                self.packet_id = Some(*id);
            }
            Ok(Event::Incoming(Packet::SubAck(ack))) if self.packet_id == Some(ack.pkid) => {
                self.packet_id = None;
                let accepted = if ack.return_codes.len() == self.filters.len() {
                    ack.return_codes
                        .iter()
                        .map(|code| matches!(code, SubscribeReasonCode::Success(_)))
                        .collect()
                } else {
                    vec![false; self.filters.len()]
                };
                self.state
                    .send_replace(SubscriptionState::Acknowledged(accepted));
            }
            Err(_) => {
                self.packet_id = None;
                self.state.send_if_modified(|state| {
                    if state.connected() {
                        *state = SubscriptionState::Disconnected;
                        true
                    } else {
                        false
                    }
                });
            }
            _ => {}
        }
    }
}
