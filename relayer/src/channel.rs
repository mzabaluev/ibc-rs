#![allow(clippy::borrowed_box)]

use core::marker::PhantomData;
use prost_types::Any;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use ibc::events::IbcEvent;
use ibc::ics04_channel::channel::{ChannelEnd, Counterparty, Order, State};
use ibc::ics04_channel::msgs::chan_close_confirm::MsgChannelCloseConfirm;
use ibc::ics04_channel::msgs::chan_close_init::MsgChannelCloseInit;
use ibc::ics04_channel::msgs::chan_open_ack::MsgChannelOpenAck;
use ibc::ics04_channel::msgs::chan_open_confirm::MsgChannelOpenConfirm;
use ibc::ics04_channel::msgs::chan_open_init::MsgChannelOpenInit;
use ibc::ics04_channel::msgs::chan_open_try::MsgChannelOpenTry;
use ibc::ics24_host::identifier::{ChainId, ChannelId, ClientId, ConnectionId, PortId};
use ibc::tagged::{DualTagged, Tagged};
use ibc::tx_msg::Msg;
use ibc::Height;
use ibc_proto::ibc::core::channel::v1::QueryConnectionChannelsRequest;

use crate::chain::counterparty::{channel_connection_client, channel_state_on_destination};
use crate::chain::handle::ChainHandle;
use crate::connection::Connection;
use crate::foreign_client::ForeignClient;
use crate::object::Channel as WorkerChannelObject;
use crate::supervisor::error::Error as SupervisorError;
use crate::util::retry::retry_with_index;
use crate::util::retry::RetryResult;

pub mod error;
pub use error::ChannelError;

mod retry_strategy {
    use std::time::Duration;

    use retry::delay::Fibonacci;

    use crate::util::retry::clamp_total;

    // Default parameters for the retrying mechanism
    const MAX_DELAY: Duration = Duration::from_secs(60); // 1 minute
    const MAX_TOTAL_DELAY: Duration = Duration::from_secs(10 * 60); // 10 minutes
    const INITIAL_DELAY: Duration = Duration::from_secs(1); // 1 second

    pub fn default() -> impl Iterator<Item = Duration> {
        clamp_total(Fibonacci::from(INITIAL_DELAY), MAX_DELAY, MAX_TOTAL_DELAY)
    }
}

pub fn from_retry_error(e: retry::Error<ChannelError>, description: String) -> ChannelError {
    match e {
        retry::Error::Operation {
            error,
            total_delay,
            tries,
        } => {
            let detail = error::ChannelErrorDetail::MaxRetry(error::MaxRetrySubdetail {
                description,
                tries,
                total_delay,
                source: Box::new(error.0),
            });
            ChannelError(detail, error.1)
        }
        retry::Error::Internal(reason) => ChannelError::retry_internal(reason),
    }
}

#[derive(Clone, Debug)]
pub struct ChannelSide<Chain, CounterpartyChain>
where
    Chain: ChainHandle<CounterpartyChain>,
{
    pub chain: Chain,
    client_id: Tagged<Chain, ClientId>,
    connection_id: Tagged<Chain, ConnectionId>,
    port_id: Tagged<Chain, PortId>,
    channel_id: Option<Tagged<Chain, ChannelId>>,
    phantom: PhantomData<CounterpartyChain>,
}

impl<Chain, CounterpartyChain> ChannelSide<Chain, CounterpartyChain>
where
    Chain: ChainHandle<CounterpartyChain>,
{
    pub fn new(
        chain: Chain,
        client_id: Tagged<Chain, ClientId>,
        connection_id: Tagged<Chain, ConnectionId>,
        port_id: Tagged<Chain, PortId>,
        channel_id: Option<Tagged<Chain, ChannelId>>,
    ) -> ChannelSide<Chain, CounterpartyChain> {
        Self {
            chain,
            client_id,
            connection_id,
            port_id,
            channel_id,
            phantom: PhantomData,
        }
    }

    pub fn chain_id(&self) -> ChainId {
        self.chain.id()
    }

    pub fn client_id(&self) -> Tagged<Chain, ClientId> {
        self.client_id.clone()
    }

    pub fn connection_id(&self) -> Tagged<Chain, ConnectionId> {
        self.connection_id.clone()
    }

    pub fn port_id(&self) -> Tagged<Chain, PortId> {
        self.port_id.clone()
    }

    pub fn channel_id(&self) -> Option<Tagged<Chain, ChannelId>> {
        self.channel_id.clone()
    }
}

#[derive(Clone, Debug)]
pub struct Channel<ChainA, ChainB>
where
    ChainA: ChainHandle<ChainB>,
    ChainB: ChainHandle<ChainA>,
{
    pub ordering: Order,
    pub a_side: ChannelSide<ChainA, ChainB>,
    pub b_side: ChannelSide<ChainB, ChainA>,
    pub connection_delay: Duration,
    pub version: Option<String>,
}

impl<ChainA, ChainB> Channel<ChainA, ChainB>
where
    ChainA: ChainHandle<ChainB>,
    ChainB: ChainHandle<ChainA>,
{
    /// Creates a new channel on top of the existing connection. If the channel is not already
    /// set-up on both sides of the connection, this functions also fulfils the channel handshake.
    pub fn new(
        connection: Connection<ChainA, ChainB>,
        ordering: Order,
        a_port: Tagged<ChainA, PortId>,
        b_port: Tagged<ChainB, PortId>,
        version: Option<String>,
    ) -> Result<Self, ChannelError> {
        let b_side_chain = connection.dst_chain();
        let version = version.unwrap_or(
            b_side_chain
                .module_version(b_port)
                .map_err(|e| ChannelError::query(b_side_chain.id(), e))?,
        );

        let src_connection_id = connection
            .src_connection_id()
            .ok_or_else(|| ChannelError::missing_local_connection(connection.src_chain().id()))?;
        let dst_connection_id = connection
            .dst_connection_id()
            .ok_or_else(|| ChannelError::missing_local_connection(connection.dst_chain().id()))?;

        let mut channel = Self {
            ordering,
            a_side: ChannelSide::new(
                connection.src_chain(),
                connection.src_client_id().clone(),
                src_connection_id.clone(),
                a_port,
                Default::default(),
            ),
            b_side: ChannelSide::new(
                connection.dst_chain(),
                connection.dst_client_id().clone(),
                dst_connection_id.clone(),
                b_port,
                Default::default(),
            ),
            connection_delay: connection.delay_period,
            version: Some(version),
        };

        channel.handshake()?;

        Ok(channel)
    }

    pub fn restore_from_event(
        chain: ChainA,
        counterparty_chain: ChainB,
        channel_open_event: DualTagged<ChainA, ChainB, IbcEvent>,
    ) -> Result<Channel<ChainA, ChainB>, ChannelError> {
        let channel_event_attributes = channel_open_event
            .dual_map(|e| e.channel_attributes().clone())
            .transpose()
            .ok_or_else(|| ChannelError::invalid_event(channel_open_event.untag()))?;

        let port_id = channel_event_attributes.map(|a| a.port_id.clone());
        let channel_id = channel_event_attributes
            .map(|a| a.channel_id.clone())
            .transpose();

        let connection_id = channel_event_attributes.map(|a| a.connection_id.clone());

        let connection = chain
            .query_connection(connection_id, Height::tagged_zero())
            .map_err(ChannelError::relayer)?;

        let connection_counterparty = connection.counterparty();

        let counterparty_connection_id = connection_counterparty
            .connection_id()
            .ok_or_else(ChannelError::missing_counterparty_connection)?;

        let counterparty_port_id = channel_event_attributes.map_flipped(|a| a.counterparty_port_id);

        let counterparty_channel_id = channel_event_attributes
            .map_flipped(|a| a.counterparty_channel_id)
            .transpose();

        let version = counterparty_chain
            .module_version(counterparty_port_id)
            .map_err(|e| ChannelError::query(counterparty_chain.id(), e))?;

        Ok(Channel {
            // The event does not include the channel ordering.
            // The message handlers `build_chan_open..` determine the order included in the handshake
            // message from channel query.
            ordering: Default::default(),
            a_side: ChannelSide::new(
                chain,
                connection.client_id(),
                connection_id,
                port_id,
                channel_id,
            ),
            b_side: ChannelSide::new(
                counterparty_chain,
                connection_counterparty.client_id(),
                counterparty_connection_id.clone(),
                counterparty_port_id,
                counterparty_channel_id,
            ),
            connection_delay: connection.delay_period(),
            // The event does not include the version.
            // The message handlers `build_chan_open..` determine the version from channel query.
            version: Some(version),
        })
    }

    /// Recreates a 'Channel' object from the worker's object built from chain state scanning.
    /// The channel must exist on chain and its connection must be initialized on both chains.
    pub fn restore_from_state(
        chain: ChainA,
        counterparty_chain: ChainB,
        channel: Tagged<ChainA, WorkerChannelObject>,
        height: Tagged<ChainA, Height>,
    ) -> Result<(Channel<ChainA, ChainB>, State), ChannelError> {
        let src_port_id = channel.map(|c| c.src_port_id.clone());
        let src_channel_id = channel.map(|c| c.src_channel_id.clone());

        let a_channel = chain
            .query_channel(src_port_id, src_channel_id, height)
            .map_err(ChannelError::relayer)?;

        let a_connection_id = a_channel
            .map(|c| c.connection_hops().first().map(Clone::clone))
            .transpose()
            .ok_or_else(|| {
                ChannelError::supervisor(SupervisorError::missing_connection_hops(
                    channel.untag().src_channel_id,
                    chain.id(),
                ))
            })?;

        let a_connection = chain
            .query_connection(a_connection_id, Height::tagged_zero())
            .map_err(ChannelError::relayer)?;

        let b_connection = a_connection.counterparty();

        let b_connection_id = b_connection.connection_id().ok_or_else(|| {
            ChannelError::supervisor(SupervisorError::channel_connection_uninitialized(
                src_channel_id.untag(),
                chain.id(),
                b_connection.0.untag(),
            ))
        })?;

        let b_channel = a_channel.map_flipped(|c| c.remote.clone());

        let mut handshake_channel = Channel {
            ordering: *a_channel.value().ordering(),
            a_side: ChannelSide::new(
                chain.clone(),
                a_connection.client_id(),
                a_connection_id.clone(),
                channel.map(|c| c.src_port_id.clone()),
                Some(channel.map(|c| c.src_channel_id.clone())),
            ),
            b_side: ChannelSide::new(
                counterparty_chain.clone(),
                b_connection.client_id(),
                b_connection_id.clone(),
                b_channel.map(|c| c.port_id.clone()),
                b_channel.map(|c| c.channel_id.clone()).transpose(),
            ),
            connection_delay: a_connection.delay_period(),
            version: Some(a_channel.value().version.clone()),
        };

        if a_channel.value().state_matches(&State::Init) && b_channel.value().channel_id.is_none() {
            let req = QueryConnectionChannelsRequest {
                connection: b_connection_id.to_string(),
                pagination: ibc_proto::cosmos::base::query::pagination::all(),
            };

            let b_channels = counterparty_chain
                .query_connection_channels(req)
                .map_err(ChannelError::relayer)?;

            for b_channel in b_channels {
                let a_channel = b_channel.map_flipped(|c| c.channel_end.remote.clone());

                let b_channel_id = b_channel.map(|c| c.channel_id);

                let m_a_channel_id = a_channel.map(|c| c.channel_id).transpose();

                if let Some(a_channel_id) = m_a_channel_id {
                    if a_channel_id == src_channel_id {
                        handshake_channel.b_side.channel_id = Some(b_channel_id);
                        break;
                    }
                }
            }
        }

        Ok((handshake_channel, a_channel.untag().state))
    }

    pub fn src_chain(&self) -> &ChainA {
        &self.a_side.chain
    }

    pub fn dst_chain(&self) -> &ChainB {
        &self.b_side.chain
    }

    pub fn src_client_id(&self) -> Tagged<ChainA, ClientId> {
        self.a_side.client_id.clone()
    }

    pub fn dst_client_id(&self) -> Tagged<ChainB, ClientId> {
        self.b_side.client_id.clone()
    }

    pub fn src_connection_id(&self) -> Tagged<ChainA, ConnectionId> {
        self.a_side.connection_id.clone()
    }

    pub fn dst_connection_id(&self) -> Tagged<ChainB, ConnectionId> {
        self.b_side.connection_id.clone()
    }

    pub fn src_port_id(&self) -> Tagged<ChainA, PortId> {
        self.a_side.port_id.clone()
    }

    pub fn dst_port_id(&self) -> Tagged<ChainB, PortId> {
        self.b_side.port_id.clone()
    }

    pub fn src_channel_id(&self) -> Option<Tagged<ChainA, ChannelId>> {
        self.a_side.channel_id()
    }

    pub fn dst_channel_id(&self) -> Option<Tagged<ChainB, ChannelId>> {
        self.b_side.channel_id()
    }

    pub fn flipped(&self) -> Channel<ChainB, ChainA> {
        Channel {
            ordering: self.ordering,
            a_side: self.b_side.clone(),
            b_side: self.a_side.clone(),
            connection_delay: self.connection_delay,
            version: self.version.clone(),
        }
    }

    fn do_chan_open_init_and_send(&mut self) -> Result<(), ChannelError> {
        let event = self.flipped().build_chan_open_init_and_send()?;

        info!("done {} => {:#?}\n", self.src_chain().id(), event);

        let channel_id = event.map(|e| extract_channel_id(e)).transpose()?;
        self.a_side.channel_id = Some(channel_id);
        info!("successfully opened init channel");

        Ok(())
    }

    // Check that the channel was created on a_chain
    fn do_chan_open_init_and_send_with_retry(&mut self) -> Result<(), ChannelError> {
        retry_with_index(retry_strategy::default(), |_| {
            self.do_chan_open_init_and_send()
        })
        .map_err(|err| {
            error!("failed to open channel after {} retries", err);

            from_retry_error(
                err,
                format!("Failed to finish channel open init for {:?}", self),
            )
        })?;

        Ok(())
    }

    fn do_chan_open_try_and_send(&mut self) -> Result<(), ChannelError> {
        let event = self.build_chan_open_try_and_send().map_err(|e| {
            error!("Failed ChanTry {:?}: {:?}", self.b_side, e);
            e
        })?;

        let channel_id = event.map(|e| extract_channel_id(e)).transpose()?;

        self.b_side.channel_id = Some(channel_id.clone());

        println!("done {} => {:#?}\n", self.dst_chain().id(), event);
        Ok(())
    }

    fn do_chan_open_try_and_send_with_retry(&mut self) -> Result<(), ChannelError> {
        retry_with_index(retry_strategy::default(), |_| {
            self.do_chan_open_try_and_send()
        })
        .map_err(|err| {
            error!("failed to open channel after {} retries", err);

            from_retry_error(
                err,
                format!("Failed to finish channel open try for {:?}", self),
            )
        })?;

        Ok(())
    }

    /// Sends the last two steps, consisting of `Ack` and `Confirm`
    /// messages, for finalizing the channel open handshake.
    ///
    /// Assumes that the channel open handshake was previously
    /// started (with `Init` & `Try` steps).
    ///
    /// Returns `Ok` when both channel ends are in state `Open`.
    /// Also returns `Ok` if the channel is undergoing a closing handshake.
    ///
    /// An `Err` can signal two cases:
    ///     - the common-case flow for the handshake protocol was interrupted,
    ///         e.g., by a competing relayer.
    ///     - Rpc problems (a query or submitting a tx failed).
    /// In both `Err` cases, there should be retry calling this method.
    fn do_chan_open_finalize(&self) -> Result<(), ChannelError> {
        fn query_channel_states<ChainA, ChainB>(
            channel: &Channel<ChainA, ChainB>,
        ) -> Result<(Tagged<ChainA, State>, Tagged<ChainB, State>), ChannelError>
        where
            ChainA: ChainHandle<ChainB>,
            ChainB: ChainHandle<ChainA>,
        {
            let src_channel_id = channel
                .src_channel_id()
                .ok_or_else(ChannelError::missing_local_channel_id)?;

            let dst_channel_id = channel
                .dst_channel_id()
                .ok_or_else(ChannelError::missing_counterparty_connection)?;

            debug!(
                "do_chan_open_finalize for src_channel_id: {}, dst_channel_id: {}",
                src_channel_id, dst_channel_id
            );

            // Continue loop if query error
            let a_channel = channel
                .src_chain()
                .query_channel(channel.src_port_id(), src_channel_id, Height::tagged_zero())
                .map_err(|e| {
                    ChannelError::handshake_finalize(
                        channel.src_port_id().value().clone(),
                        src_channel_id.value().clone(),
                        channel.src_chain().id(),
                        e,
                    )
                })?;

            let b_channel = channel
                .dst_chain()
                .query_channel(channel.dst_port_id(), dst_channel_id, Height::tagged_zero())
                .map_err(|e| {
                    ChannelError::handshake_finalize(
                        channel.dst_port_id().value().clone(),
                        dst_channel_id.value().clone(),
                        channel.dst_chain().id(),
                        e,
                    )
                })?;

            let a_state = a_channel.map(|c| c.state().clone());
            let b_state = b_channel.map(|c| c.state().clone());

            Ok((a_state, b_state))
        }

        fn expect_channel_states<ChainA, ChainB>(
            ctx: &Channel<ChainA, ChainB>,
            a1: State,
            b1: State,
        ) -> Result<(), ChannelError>
        where
            ChainA: ChainHandle<ChainB>,
            ChainB: ChainHandle<ChainA>,
        {
            let (a2, b2) = query_channel_states(ctx)?;

            if (a1, b1) == (a2.untag(), b2.untag()) {
                Ok(())
            } else {
                warn!(
                    "expected channels to progress to states {}, {}), instead got ({}, {})",
                    a1, b1, a2, b2
                );

                debug!("returning PartialOpenHandshake to retry");

                // One more step (confirm) left.
                // Returning error signals that the caller should retry.
                Err(ChannelError::partial_open_handshake(a1, b1))
            }
        }

        let (a_state, b_state) = query_channel_states(self)?;
        debug!(
            "do_chan_open_finalize with channel states: {}, {}",
            a_state, b_state
        );

        match (a_state.untag(), b_state.untag()) {
            // Handle sending the Ack message to the source chain,
            // then the Confirm message to the destination.
            (State::Init, State::TryOpen) | (State::TryOpen, State::TryOpen) => {
                self.flipped().build_chan_open_ack_and_send()?;

                expect_channel_states(self, State::Open, State::TryOpen)?;

                self.build_chan_open_confirm_and_send()?;

                expect_channel_states(self, State::Open, State::Open)?;

                Ok(())
            }

            // Handle sending the Ack message to the destination chain,
            // then the Confirm to the source chain.
            (State::TryOpen, State::Init) => {
                self.flipped().build_chan_open_ack_and_send()?;

                expect_channel_states(self, State::TryOpen, State::Open)?;

                self.flipped().build_chan_open_confirm_and_send()?;

                expect_channel_states(self, State::Open, State::Open)?;

                Ok(())
            }

            // Handle sending the Confirm message to the destination chain.
            (State::Open, State::TryOpen) => {
                self.build_chan_open_confirm_and_send()?;

                expect_channel_states(self, State::Open, State::Open)?;

                Ok(())
            }

            // Send Confirm to the source chain.
            (State::TryOpen, State::Open) => {
                self.flipped().build_chan_open_confirm_and_send()?;

                expect_channel_states(self, State::Open, State::Open)?;

                Ok(())
            }

            (State::Open, State::Open) => {
                info!("channel handshake already finished for {:#?}\n", self);
                Ok(())
            }

            // In all other conditions, return Ok, since the channel open handshake does not apply.
            _ => Ok(()),
        }
    }

    /// Takes a partially open channel and finalizes the open handshake protocol.
    ///
    /// Pre-condition: the channel identifiers are established on both ends
    ///   (i.e., `OpenInit` and `OpenTry` have executed previously for this channel).
    ///
    /// Post-condition: the channel state is `Open` on both ends if successful.
    fn do_chan_open_finalize_with_retry(&self) -> Result<(), ChannelError> {
        retry_with_index(retry_strategy::default(), |_| self.do_chan_open_finalize()).map_err(
            |err| {
                error!("failed to open channel after {} retries", err);
                from_retry_error(
                    err,
                    format!("Failed to finish channel handshake for {:?}", self),
                )
            },
        )?;

        Ok(())
    }

    /// Executes the channel handshake protocol (ICS004)
    fn handshake(&mut self) -> Result<(), ChannelError> {
        self.do_chan_open_init_and_send_with_retry()?;
        self.do_chan_open_try_and_send_with_retry()?;
        self.do_chan_open_finalize_with_retry()
    }

    pub fn counterparty_state(&self) -> Result<Tagged<ChainB, State>, ChannelError> {
        // Source channel ID must be specified
        let channel_id = self
            .src_channel_id()
            .ok_or_else(ChannelError::missing_local_channel_id)?;

        let channel_deps =
            channel_connection_client(self.src_chain(), self.src_port_id(), channel_id)
                .map_err(|e| ChannelError::query_channel(channel_id.untag(), e))?;

        channel_state_on_destination(
            channel_deps.dual_map(|c| c.channel.clone()),
            channel_deps.dual_map(|c| c.connection.clone()),
            self.dst_chain(),
        )
        .map_err(|e| ChannelError::query_channel(channel_id.value().clone(), e))
    }

    pub fn handshake_step(
        &mut self,
        state: State,
    ) -> Result<Vec<Tagged<ChainB, IbcEvent>>, ChannelError> {
        match (state, self.counterparty_state()?.value()) {
            (State::Init, State::Uninitialized) => Ok(vec![self.build_chan_open_try_and_send()?]),
            (State::Init, State::Init) => Ok(vec![self.build_chan_open_try_and_send()?]),
            (State::TryOpen, State::Init) => Ok(vec![self.build_chan_open_ack_and_send()?]),
            (State::TryOpen, State::TryOpen) => Ok(vec![self.build_chan_open_ack_and_send()?]),
            (State::Open, State::TryOpen) => Ok(vec![self.build_chan_open_confirm_and_send()?]),
            _ => Ok(vec![]),
        }
    }

    pub fn step_state(&mut self, state: State, index: u64) -> RetryResult<(), u64> {
        let done = '🥳';

        match self.handshake_step(state) {
            Err(e) => {
                error!("Failed Chan{:?} with error: {}", state, e);
                RetryResult::Retry(index)
            }
            Ok(ev) => {
                debug!("{} => {:#?}\n", done, ev);
                RetryResult::Ok(())
            }
        }
    }

    pub fn step_event(&mut self, event: IbcEvent, index: u64) -> RetryResult<(), u64> {
        let state = match event {
            IbcEvent::OpenInitChannel(_) => State::Init,
            IbcEvent::OpenTryChannel(_) => State::TryOpen,
            IbcEvent::OpenAckChannel(_) => State::Open,
            IbcEvent::OpenConfirmChannel(_) => State::Open,
            _ => State::Uninitialized,
        };

        self.step_state(state, index)
    }

    pub fn build_update_client_on_dst(&self, height: Height) -> Result<Vec<Any>, ChannelError> {
        let client = ForeignClient::restore(
            self.dst_client_id().clone(),
            self.dst_chain().clone(),
            self.src_chain().clone(),
        );

        client.build_update_client(height).map_err(|e| {
            ChannelError::client_operation(
                self.dst_client_id().value().clone(),
                self.dst_chain().id(),
                e,
            )
        })
    }

    /// Returns the channel version if already set, otherwise it queries the destination chain
    /// for the destination port's version.
    /// Note: This query is currently not available and it is hardcoded in the `module_version()`
    /// to be `ics20-1` for `transfer` port.
    pub fn dst_version(&self) -> Result<String, ChannelError> {
        Ok(self.version.clone().unwrap_or(
            self.dst_chain()
                .module_version(self.dst_port_id())
                .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?,
        ))
    }

    /// Returns the channel version if already set, otherwise it queries the source chain
    /// for the source port's version.
    pub fn src_version(&self) -> Result<String, ChannelError> {
        Ok(self.version.clone().unwrap_or(
            self.src_chain()
                .module_version(self.src_port_id())
                .map_err(|e| ChannelError::query(self.src_chain().id(), e))?,
        ))
    }

    pub fn build_chan_open_init(&self) -> Result<Vec<Any>, ChannelError> {
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?;

        let counterparty = Counterparty::new(self.src_port_id().value().clone(), None);

        let channel = ChannelEnd::new(
            State::Init,
            self.ordering,
            counterparty,
            vec![self.dst_connection_id().value().clone()],
            self.dst_version()?,
        );

        // Build the domain type message
        let new_msg = MsgChannelOpenInit {
            port_id: self.dst_port_id().value().clone(),
            channel,
            signer,
        };

        Ok(vec![new_msg.to_any()])
    }

    pub fn build_chan_open_init_and_send(&self) -> Result<Tagged<ChainB, IbcEvent>, ChannelError> {
        let dst_msgs = self.build_chan_open_init()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ChannelError::submit(self.dst_chain().id(), e))?;

        for event in events {
            match event.value() {
                IbcEvent::OpenInitChannel(_) => {
                    return Ok(event);
                }
                IbcEvent::ChainError(e) => {
                    return Err(ChannelError::tx_response(e.clone()));
                }
                _ => {}
            }
        }

        Err(ChannelError::missing_event(
            "no chan init event was in the response".to_string(),
        ))
    }

    /// Retrieves the channel from destination and compares against the expected channel
    /// built from the message type (`msg_type`) and options (`opts`).
    /// If the expected and the destination channels are compatible, it returns the expected channel
    /// Source and destination channel IDs must be specified.
    fn validated_expected_channel(
        &self,
        msg_type: ChannelMsgType,
    ) -> Result<DualTagged<ChainB, ChainA, ChannelEnd>, ChannelError> {
        // Destination channel ID must be specified
        let dst_channel_id = self
            .dst_channel_id()
            .ok_or_else(ChannelError::missing_counterparty_channel_id)?;

        // If there is a channel present on the destination chain, it should look like this:
        let counterparty = Counterparty::new(
            self.src_port_id().value().clone(),
            self.src_channel_id().map(|id| id.value().clone()),
        );

        // The highest expected state, depends on the message type:
        let highest_state = match msg_type {
            ChannelMsgType::OpenAck => State::TryOpen,
            ChannelMsgType::OpenConfirm => State::TryOpen,
            ChannelMsgType::CloseConfirm => State::Open,
            _ => State::Uninitialized,
        };

        let dst_expected_channel = <DualTagged<ChainB, ChainA, _>>::new(ChannelEnd::new(
            highest_state,
            self.ordering,
            counterparty,
            vec![self.dst_connection_id().value().clone()],
            self.dst_version()?,
        ));

        // Retrieve existing channel
        let dst_channel = self
            .dst_chain()
            .query_channel(self.dst_port_id(), dst_channel_id, Height::tagged_zero())
            .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?;

        // Check if a channel is expected to exist on destination chain
        // A channel must exist on destination chain for Ack and Confirm Tx-es to succeed
        if dst_channel.value().state_matches(&State::Uninitialized) {
            return Err(ChannelError::missing_channel_on_destination());
        }

        check_destination_channel_state(
            dst_channel_id.clone(),
            dst_channel,
            dst_expected_channel.clone(),
        )?;

        Ok(dst_expected_channel)
    }

    pub fn build_chan_open_try(&self) -> Result<Vec<Any>, ChannelError> {
        // Source channel ID must be specified
        let src_channel_id = self
            .src_channel_id()
            .ok_or_else(ChannelError::missing_local_channel_id)?;

        // Channel must exist on source
        let src_channel = self
            .src_chain()
            .query_channel(self.src_port_id(), src_channel_id, Height::tagged_zero())
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        let dst_channel = src_channel.map_flipped(|c| c.counterparty().clone());
        let dst_port_id = dst_channel.map(|c| c.port_id().clone());

        if dst_port_id != self.dst_port_id() {
            return Err(ChannelError::mismatch_port(
                self.dst_chain().id(),
                self.dst_port_id().value().clone(),
                self.src_chain().id(),
                dst_port_id.untag(),
                src_channel_id.value().clone(),
            ));
        }

        // Connection must exist on destination
        self.dst_chain()
            .query_connection(self.dst_connection_id(), Height::zero())
            .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?;

        let query_height = self
            .src_chain()
            .query_latest_height()
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        let proofs = self
            .src_chain()
            .build_channel_proofs(self.src_port_id(), src_channel_id, query_height)
            .map_err(ChannelError::channel_proof)?;

        // Build message(s) to update client on destination
        let mut msgs = self.build_update_client_on_dst(proofs.height())?;

        let counterparty = Counterparty::new(
            self.src_port_id().value().clone(),
            self.src_channel_id().map(|id| id.value().clone()),
        );

        let channel = ChannelEnd::new(
            State::TryOpen,
            *src_channel.ordering(),
            counterparty,
            vec![self.dst_connection_id().clone()],
            self.dst_version()?,
        );

        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ChannelError::fetch_signer(self.dst_chain().id(), e))?;

        let previous_channel_id = if src_channel.counterparty().channel_id.is_none() {
            self.b_side.channel_id.clone()
        } else {
            src_channel.counterparty().channel_id.clone()
        };

        // Build the domain type message
        let new_msg = MsgChannelOpenTry {
            port_id: self.dst_port_id().clone(),
            previous_channel_id,
            counterparty_version: self.src_version()?,
            channel,
            proofs,
            signer,
        };

        msgs.push(new_msg.to_any());
        Ok(msgs)
    }

    pub fn build_chan_open_try_and_send(&self) -> Result<Tagged<ChainB, IbcEvent>, ChannelError> {
        let dst_msgs = self.build_chan_open_try()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ChannelError::submit(self.dst_chain().id(), e))?;

        for event in events {
            match event.value() {
                IbcEvent::OpenTryChannel(_) => {
                    return Ok(event);
                }
                IbcEvent::ChainError(e) => {
                    return Err(ChannelError::tx_response(e));
                }
                _ => {}
            }
        }

        Err(ChannelError::missing_event(
            "no chan try event was in the response".to_string(),
        ))
    }

    pub fn build_chan_open_ack(&self) -> Result<Tagged<ChainB, Vec<Any>>, ChannelError> {
        // Source and destination channel IDs must be specified
        let src_channel_id = self
            .src_channel_id()
            .ok_or_else(ChannelError::missing_local_channel_id)?;
        let dst_channel_id = self
            .dst_channel_id()
            .ok_or_else(ChannelError::missing_counterparty_channel_id)?;

        // Check that the destination chain will accept the message
        self.validated_expected_channel(ChannelMsgType::OpenAck)?;

        // Channel must exist on source
        self.src_chain()
            .query_channel(self.src_port_id(), src_channel_id, Height::zero())
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        // Connection must exist on destination
        self.dst_chain()
            .query_connection(self.dst_connection_id(), Height::zero())
            .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?;

        let query_height = self
            .src_chain()
            .query_latest_height()
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        let proofs = self
            .src_chain()
            .build_channel_proofs(self.src_port_id(), src_channel_id, query_height)
            .map_err(ChannelError::channel_proof)?;

        // Build message(s) to update client on destination
        let mut msgs = self.build_update_client_on_dst(proofs.height())?;

        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ChannelError::fetch_signer(self.dst_chain().id(), e))?;

        // Build the domain type message
        let new_msg = MsgChannelOpenAck {
            port_id: self.dst_port_id().value().clone(),
            channel_id: dst_channel_id.value().clone(),
            counterparty_channel_id: src_channel_id.value().clone(),
            counterparty_version: self.src_version()?,
            proofs,
            signer,
        };

        msgs.push(new_msg.to_any());
        Ok(msgs)
    }

    pub fn build_chan_open_ack_and_send(&self) -> Result<Tagged<ChainB, IbcEvent>, ChannelError> {
        fn do_build_chan_open_ack_and_send<ChainA, ChainB>(
            channel: &Channel<ChainA, ChainB>,
        ) -> Result<Tagged<ChainB, IbcEvent>, ChannelError>
        where
            ChainA: ChainHandle<ChainB>,
            ChainB: ChainHandle<ChainA>,
        {
            let dst_msgs = channel.build_chan_open_ack()?;

            let events = channel
                .dst_chain()
                .send_msgs(dst_msgs)
                .map_err(|e| ChannelError::submit(channel.dst_chain().id(), e))?;

            // Find the relevant event for channel open ack
            let event = events
                .into_iter()
                .find(|event| {
                    matches!(event, IbcEvent::OpenAckChannel(_))
                        || matches!(event, IbcEvent::ChainError(_))
                })
                .ok_or_else(|| {
                    ChannelError::missing_event("no chan ack event was in the response".to_string())
                })?;

            match event {
                IbcEvent::OpenAckChannel(_) => {
                    info!(
                        "done with ChanAck step {} => {:#?}\n",
                        channel.dst_chain().id(),
                        event
                    );

                    Ok(event)
                }
                IbcEvent::ChainError(e) => Err(ChannelError::tx_response(e)),
                _ => Err(ChannelError::invalid_event(event)),
            }
        }

        do_build_chan_open_ack_and_send(self).map_err(|e| {
            error!("failed ChanAck {:?}: {}", self.b_side, e);
            e
        })
    }

    pub fn build_chan_open_confirm(&self) -> Result<Vec<Any>, ChannelError> {
        // Source and destination channel IDs must be specified
        let src_channel_id = self
            .src_channel_id()
            .ok_or_else(ChannelError::missing_local_channel_id)?;
        let dst_channel_id = self
            .dst_channel_id()
            .ok_or_else(ChannelError::missing_counterparty_channel_id)?;

        // Check that the destination chain will accept the message
        self.validated_expected_channel(ChannelMsgType::OpenConfirm)?;

        // Channel must exist on source
        self.src_chain()
            .query_channel(self.src_port_id(), src_channel_id, Height::zero())
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        // Connection must exist on destination
        self.dst_chain()
            .query_connection(self.dst_connection_id(), Height::zero())
            .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?;

        let query_height = self
            .src_chain()
            .query_latest_height()
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        let proofs = self
            .src_chain()
            .build_channel_proofs(self.src_port_id(), src_channel_id, query_height)
            .map_err(ChannelError::channel_proof)?;

        // Build message(s) to update client on destination
        let mut msgs = self.build_update_client_on_dst(proofs.height())?;

        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ChannelError::fetch_signer(self.dst_chain().id(), e))?;

        // Build the domain type message
        let new_msg = MsgChannelOpenConfirm {
            port_id: self.dst_port_id().clone(),
            channel_id: dst_channel_id.clone(),
            proofs,
            signer,
        };

        msgs.push(new_msg.to_any());
        Ok(msgs)
    }

    pub fn build_chan_open_confirm_and_send(
        &self,
    ) -> Result<Tagged<ChainB, IbcEvent>, ChannelError> {
        fn do_build_chan_open_confirm_and_send<ChainA, ChainB>(
            channel: &Channel<ChainA, ChainB>,
        ) -> Result<Tagged<ChainB, IbcEvent>, ChannelError>
        where
            ChainA: ChainHandle<ChainB>,
            ChainB: ChainHandle<ChainA>,
        {
            let dst_msgs = channel.build_chan_open_confirm()?;

            let events = channel
                .dst_chain()
                .send_msgs(dst_msgs)
                .map_err(|e| ChannelError::submit(channel.dst_chain().id(), e))?;

            for event in events {
                match event.value() {
                    IbcEvent::OpenConfirmChannel(_) => {
                        info!("done {} => {:#?}\n", channel.dst_chain().id(), event);
                        return Ok(());
                    }
                    IbcEvent::ChainError(_) => {
                        return Err(ChannelError::invalid_event(event.untag()))
                    }
                    _ => {}
                }
            }

            return Err(ChannelError::missing_event(
                "no chan confirm event was in the response".to_string(),
            ));
        }

        do_build_chan_open_confirm_and_send(self).map_err(|e| {
            error!("failed ChanConfirm {:?}: {}", self.b_side, e);
            e
        })
    }

    pub fn build_chan_close_init(&self) -> Result<Vec<Any>, ChannelError> {
        // Destination channel ID must be specified
        let dst_channel_id = self
            .dst_channel_id()
            .ok_or_else(ChannelError::missing_counterparty_channel_id)?;

        // Channel must exist on destination
        self.dst_chain()
            .query_channel(self.dst_port_id(), dst_channel_id, Height::zero())
            .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?;

        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ChannelError::fetch_signer(self.dst_chain().id(), e))?;

        // Build the domain type message
        let new_msg = MsgChannelCloseInit {
            port_id: self.dst_port_id().clone(),
            channel_id: dst_channel_id.clone(),
            signer,
        };

        Ok(vec![new_msg.to_any()])
    }

    pub fn build_chan_close_init_and_send(&self) -> Result<IbcEvent, ChannelError> {
        let dst_msgs = self.build_chan_close_init()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ChannelError::submit(self.dst_chain().id(), e))?;

        // Find the relevant event for channel close init
        let result = events
            .into_iter()
            .find(|event| {
                matches!(event, IbcEvent::CloseInitChannel(_))
                    || matches!(event, IbcEvent::ChainError(_))
            })
            .ok_or_else(|| {
                ChannelError::missing_event("no chan init event was in the response".to_string())
            })?;

        match result {
            IbcEvent::CloseInitChannel(_) => Ok(result),
            IbcEvent::ChainError(e) => Err(ChannelError::tx_response(e)),
            _ => Err(ChannelError::invalid_event(result)),
        }
    }

    pub fn build_chan_close_confirm(&self) -> Result<Vec<Any>, ChannelError> {
        // Source and destination channel IDs must be specified
        let src_channel_id = self
            .src_channel_id()
            .ok_or_else(ChannelError::missing_local_channel_id)?;
        let dst_channel_id = self
            .dst_channel_id()
            .ok_or_else(ChannelError::missing_counterparty_channel_id)?;

        // Check that the destination chain will accept the message
        self.validated_expected_channel(ChannelMsgType::CloseConfirm)?;

        // Channel must exist on source
        self.src_chain()
            .query_channel(self.src_port_id(), src_channel_id, Height::zero())
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        // Connection must exist on destination
        self.dst_chain()
            .query_connection(self.dst_connection_id(), Height::zero())
            .map_err(|e| ChannelError::query(self.dst_chain().id(), e))?;

        let query_height = self
            .src_chain()
            .query_latest_height()
            .map_err(|e| ChannelError::query(self.src_chain().id(), e))?;

        let proofs = self
            .src_chain()
            .build_channel_proofs(self.src_port_id(), src_channel_id, query_height)
            .map_err(ChannelError::channel_proof)?;

        // Build message(s) to update client on destination
        let mut msgs = self.build_update_client_on_dst(proofs.height())?;

        // Get signer
        let signer = self
            .dst_chain()
            .get_signer()
            .map_err(|e| ChannelError::fetch_signer(self.dst_chain().id(), e))?;

        // Build the domain type message
        let new_msg = MsgChannelCloseConfirm {
            port_id: self.dst_port_id().clone(),
            channel_id: dst_channel_id.clone(),
            proofs,
            signer,
        };

        msgs.push(new_msg.to_any());
        Ok(msgs)
    }

    pub fn build_chan_close_confirm_and_send(&self) -> Result<IbcEvent, ChannelError> {
        let dst_msgs = self.build_chan_close_confirm()?;

        let events = self
            .dst_chain()
            .send_msgs(dst_msgs)
            .map_err(|e| ChannelError::submit(self.dst_chain().id(), e))?;

        // Find the relevant event for channel close confirm
        let result = events
            .into_iter()
            .find(|event| {
                matches!(event, IbcEvent::CloseConfirmChannel(_))
                    || matches!(event, IbcEvent::ChainError(_))
            })
            .ok_or_else(|| {
                ChannelError::missing_event("no chan confirm event was in the response".to_string())
            })?;

        match result {
            IbcEvent::CloseConfirmChannel(_) => Ok(result),
            IbcEvent::ChainError(e) => Err(ChannelError::tx_response(e)),
            _ => Err(ChannelError::invalid_event(result)),
        }
    }
}

pub fn extract_channel_id(event: &IbcEvent) -> Result<ChannelId, ChannelError> {
    match event {
        IbcEvent::OpenInitChannel(ev) => ev.channel_id(),
        IbcEvent::OpenTryChannel(ev) => ev.channel_id(),
        IbcEvent::OpenAckChannel(ev) => ev.channel_id(),
        IbcEvent::OpenConfirmChannel(ev) => ev.channel_id(),
        _ => None,
    }
    .ok_or_else(|| ChannelError::missing_event("cannot extract channel_id from result".to_string()))
}

/// Enumeration of proof carrying ICS4 message, helper for relayer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelMsgType {
    OpenTry,
    OpenAck,
    OpenConfirm,
    CloseConfirm,
}

fn check_destination_channel_state<Chain, Counterparty>(
    channel_id: Tagged<Chain, ChannelId>,
    existing_channel: DualTagged<Chain, Counterparty, ChannelEnd>,
    expected_channel: DualTagged<Chain, Counterparty, ChannelEnd>,
) -> Result<(), ChannelError>
where
    Chain: ChainHandle<Counterparty>,
{
    let good_connection_hops =
        existing_channel.value().connection_hops() == expected_channel.value().connection_hops();

    // TODO: Refactor into a method
    let good_state =
        *existing_channel.value().state() as u32 <= *expected_channel.value().state() as u32;

    let good_channel_port_ids = existing_channel
        .value()
        .counterparty()
        .channel_id()
        .is_none()
        || existing_channel.value().counterparty().channel_id()
            == expected_channel.value().counterparty().channel_id()
            && existing_channel.value().counterparty().port_id()
                == expected_channel.value().counterparty().port_id();

    // TODO: Check versions

    if good_state && good_connection_hops && good_channel_port_ids {
        Ok(())
    } else {
        Err(ChannelError::channel_already_exist(channel_id.untag()))
    }
}
