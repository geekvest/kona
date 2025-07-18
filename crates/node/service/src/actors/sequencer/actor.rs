//! The [`SequencerActor`].

use super::{L1OriginSelector, L1OriginSelectorError};
use crate::{CancellableContext, NodeActor};
use alloy_provider::RootProvider;
use async_trait::async_trait;
use kona_derive::{AttributesBuilder, PipelineErrorKind, StatefulAttributesBuilder};
use kona_genesis::RollupConfig;
use kona_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};
use kona_providers_alloy::{AlloyChainProvider, AlloyL2ChainProvider};
use op_alloy_network::Optimism;
use op_alloy_rpc_types_engine::OpExecutionPayloadEnvelope;
use std::{sync::Arc, time::Duration};
use tokio::{
    select,
    sync::{mpsc, watch},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

/// The [`SequencerActor`] is responsible for building L2 blocks on top of the current unsafe head
/// and scheduling them to be signed and gossipped by the P2P layer, extending the L2 chain with new
/// blocks.
#[derive(Debug)]
pub struct SequencerActor<AB: AttributesBuilderConfig> {
    /// The [`SequencerActorState`].
    builder: AB,
    /// Watch channel to observe the unsafe head of the engine.
    pub unsafe_head_rx: watch::Receiver<L2BlockInfo>,
}

/// The state of the [`SequencerActor`].
#[derive(Debug)]
struct SequencerActorState<AB: AttributesBuilder> {
    /// The [`RollupConfig`] for the chain being sequenced.
    pub cfg: Arc<RollupConfig>,
    /// The [`AttributesBuilder`].
    pub builder: AB,
    /// The [`L1OriginSelector`].
    pub origin_selector: L1OriginSelector<RootProvider>,
}

/// A trait for building [`AttributesBuilder`]s.
pub trait AttributesBuilderConfig {
    /// The type of [`AttributesBuilder`] to build.
    type AB: AttributesBuilder;

    /// Builds the [`AttributesBuilder`].
    fn build(self) -> Self::AB;
}

impl From<SequencerBuilder>
    for SequencerActorState<StatefulAttributesBuilder<AlloyChainProvider, AlloyL2ChainProvider>>
{
    fn from(seq_builder: SequencerBuilder) -> Self {
        let cfg = seq_builder.cfg.clone();
        let l1_provider = seq_builder.l1_provider.clone();

        let builder = seq_builder.build();

        let origin_selector = L1OriginSelector::new(cfg.clone(), l1_provider);

        Self { cfg, builder, origin_selector }
    }
}

const DERIVATION_PROVIDER_CACHE_SIZE: usize = 1024;

/// The builder for the [`SequencerActor`].
#[derive(Debug)]
pub struct SequencerBuilder {
    /// The [`RollupConfig`] for the chain being sequenced.
    pub cfg: Arc<RollupConfig>,
    /// The L1 provider.
    pub l1_provider: RootProvider,
    /// The L2 provider.
    pub l2_provider: RootProvider<Optimism>,
}

impl AttributesBuilderConfig for SequencerBuilder {
    type AB = StatefulAttributesBuilder<AlloyChainProvider, AlloyL2ChainProvider>;

    fn build(self) -> Self::AB {
        let l1_derivation_provider =
            AlloyChainProvider::new(self.l1_provider.clone(), DERIVATION_PROVIDER_CACHE_SIZE);
        let l2_derivation_provider = AlloyL2ChainProvider::new(
            self.l2_provider.clone(),
            self.cfg.clone(),
            DERIVATION_PROVIDER_CACHE_SIZE,
        );
        StatefulAttributesBuilder::new(self.cfg, l2_derivation_provider, l1_derivation_provider)
    }
}

/// The inbound channels for the [`SequencerActor`].
/// These channels are used by external actors to send messages to the sequencer actor.
#[derive(Debug)]
pub struct SequencerInboundData {
    /// Watch channel to observe the unsafe head of the engine.
    pub unsafe_head_tx: watch::Sender<L2BlockInfo>,
}

/// The communication context used by the [`SequencerActor`].
#[derive(Debug)]
pub struct SequencerContext {
    /// The cancellation token, shared between all tasks.
    pub cancellation: CancellationToken,
    /// Sender to request the engine to reset.
    pub reset_request_tx: mpsc::Sender<()>,
    /// Sender to request the execution layer to build a payload attributes on top of the
    /// current unsafe head.
    pub build_request_tx:
        mpsc::Sender<(OpAttributesWithParent, mpsc::Sender<OpExecutionPayloadEnvelope>)>,
    /// A sender to asynchronously sign and gossip built [`OpExecutionPayloadEnvelope`]s to the
    /// network actor.
    pub gossip_payload_tx: mpsc::Sender<OpExecutionPayloadEnvelope>,
}

impl CancellableContext for SequencerContext {
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation.cancelled()
    }
}

/// An error produced by the [`SequencerActor`].
#[derive(Debug, thiserror::Error)]
pub enum SequencerActorError {
    /// An error occurred while building payload attributes.
    #[error(transparent)]
    AttributesBuilder(#[from] PipelineErrorKind),
    /// An error occurred while selecting the next L1 origin.
    #[error(transparent)]
    L1OriginSelector(#[from] L1OriginSelectorError),
    /// A channel was unexpectedly closed.
    #[error("Channel closed unexpectedly")]
    ChannelClosed,
}

impl<AB: AttributesBuilderConfig> SequencerActor<AB> {
    /// Creates a new instance of the [`SequencerActor`].
    pub fn new(state: AB) -> (SequencerInboundData, Self) {
        let (unsafe_head_tx, unsafe_head_rx) = watch::channel(L2BlockInfo::default());
        let actor = Self { builder: state, unsafe_head_rx };

        (SequencerInboundData { unsafe_head_tx }, actor)
    }
}

impl<AB: AttributesBuilder> SequencerActorState<AB> {
    /// Starts the build job for the next L2 block, on top of the current unsafe head.
    ///
    /// Notes: TODO
    async fn start_build(
        &mut self,
        ctx: &mut SequencerContext,
        latest_payload_rx: &mut Option<mpsc::Receiver<OpExecutionPayloadEnvelope>>,
        unsafe_head_rx: &mut watch::Receiver<L2BlockInfo>,
    ) -> Result<(), SequencerActorError> {
        // If there is currently a block building job in-progress, do not start a new one.
        if latest_payload_rx.is_some() {
            return Ok(());
        }

        let unsafe_head = *unsafe_head_rx.borrow();
        let l1_origin = match self.origin_selector.next_l1_origin(unsafe_head).await {
            Ok(l1_origin) => l1_origin,
            Err(err) => {
                warn!(
                    target: "sequencer",
                    ?err,
                    "Temporary error occurred while selecting next L1 origin. Re-attempting on next tick."
                );
                return Ok(())
            }
        };

        if unsafe_head.l1_origin.hash != l1_origin.parent_hash &&
            unsafe_head.l1_origin.hash != l1_origin.hash
        {
            warn!(
                target: "sequencer",
                l1_origin = ?l1_origin,
                unsafe_head_hash = %unsafe_head.l1_origin.hash,
                unsafe_head_l1_origin = ?unsafe_head.l1_origin,
                "Cannot build new L2 block on inconsistent L1 origin, resetting engine"
            );
            if let Err(err) = ctx.reset_request_tx.send(()).await {
                error!(target: "sequencer", ?err, "Failed to reset engine");
                ctx.cancellation.cancel();
                return Err(SequencerActorError::ChannelClosed);
            }
            return Ok(());
        }

        info!(
            target: "sequencer",
            parent_num = unsafe_head.block_info.number,
            l1_origin_num = l1_origin.number,
            "Started sequencing new block"
        );

        // Build the payload attributes for the next block.
        let mut attributes =
            match self.builder.prepare_payload_attributes(unsafe_head, l1_origin.id()).await {
                Ok(attrs) => attrs,
                Err(PipelineErrorKind::Temporary(_)) => {
                    return Ok(());
                    // Do nothing and allow a retry.
                }
                Err(PipelineErrorKind::Reset(_)) => {
                    if let Err(err) = ctx.reset_request_tx.send(()).await {
                        error!(target: "sequencer", ?err, "Failed to reset engine");
                        ctx.cancellation.cancel();
                        return Err(SequencerActorError::ChannelClosed);
                    }

                    warn!(
                        target: "sequencer",
                        "Resetting engine due to pipeline error while preparing payload attributes"
                    );
                    return Ok(());
                }
                Err(err @ PipelineErrorKind::Critical(_)) => {
                    error!(target: "sequencer", ?err, "Failed to prepare payload attributes");
                    ctx.cancellation.cancel();
                    return Err(err.into());
                }
            };

        // If the next L2 block is beyond the sequencer drift threshold, we must produce an empty
        // block.
        attributes.no_tx_pool = (attributes.payload_attributes.timestamp >
            l1_origin.timestamp + self.cfg.max_sequencer_drift(l1_origin.timestamp))
        .then_some(true);

        // Do not include transactions in the first Ecotone block.
        if self.cfg.is_first_ecotone_block(attributes.payload_attributes.timestamp) {
            info!(target: "sequencer", "Sequencing ecotone upgrade block");
            attributes.no_tx_pool = Some(true);
        }

        // Do not include transactions in the first Fjord block.
        if self.cfg.is_first_fjord_block(attributes.payload_attributes.timestamp) {
            info!(target: "sequencer", "Sequencing fjord upgrade block");
            attributes.no_tx_pool = Some(true);
        }

        // Do not include transactions in the first Granite block.
        if self.cfg.is_first_granite_block(attributes.payload_attributes.timestamp) {
            info!(target: "sequencer", "Sequencing granite upgrade block");
            attributes.no_tx_pool = Some(true);
        }

        // Do not include transactions in the first Holocene block.
        if self.cfg.is_first_holocene_block(attributes.payload_attributes.timestamp) {
            info!(target: "sequencer", "Sequencing holocene upgrade block");
            attributes.no_tx_pool = Some(true);
        }

        // Do not include transactions in the first Isthmus block.
        if self.cfg.is_first_isthmus_block(attributes.payload_attributes.timestamp) {
            info!(target: "sequencer", "Sequencing isthmus upgrade block");
            attributes.no_tx_pool = Some(true);
        }

        // Do not include transactions in the first Interop block.
        if self.cfg.is_first_interop_block(attributes.payload_attributes.timestamp) {
            info!(target: "sequencer", "Sequencing interop upgrade block");
            attributes.no_tx_pool = Some(true);
        }

        // TODO: L1 origin in this type must be optional, to account for attributes that weren't
        // derived.
        let attrs_with_parent =
            OpAttributesWithParent::new(attributes, unsafe_head, BlockInfo::default(), false);

        // Create a new channel to receive the built payload.
        let (payload_tx, payload_rx) = mpsc::channel(1);
        *latest_payload_rx = Some(payload_rx);

        // Send the built attributes to the engine to be built.
        if let Err(err) = ctx.build_request_tx.send((attrs_with_parent, payload_tx)).await {
            error!(target: "sequencer", ?err, "Failed to send built attributes to engine");
            ctx.cancellation.cancel();
            return Err(SequencerActorError::ChannelClosed);
        }

        Ok(())
    }

    /// Waits for the next payload to be built and returns it, if there is a payload receiver
    /// present.
    async fn try_wait_for_payload(
        &mut self,
        ctx: &mut SequencerContext,
        latest_payload_rx: &mut Option<mpsc::Receiver<OpExecutionPayloadEnvelope>>,
    ) -> Result<Option<OpExecutionPayloadEnvelope>, SequencerActorError> {
        if let Some(mut payload_rx) = latest_payload_rx.take() {
            payload_rx.recv().await.map_or_else(
                || {
                    error!(target: "sequencer", "Failed to receive built payload");
                    ctx.cancellation.cancel();
                    Err(SequencerActorError::ChannelClosed)
                },
                |payload| Ok(Some(payload)),
            )
        } else {
            Ok(None)
        }
    }

    /// Schedules a built [`OpExecutionPayloadEnvelope`] to be signed and gossipped.
    async fn schedule_gossip(
        &mut self,
        ctx: &mut SequencerContext,
        payload: OpExecutionPayloadEnvelope,
    ) -> Result<(), SequencerActorError> {
        // Send the payload to the P2P layer to be signed and gossipped.
        if let Err(err) = ctx.gossip_payload_tx.send(payload).await {
            error!(target: "sequencer", ?err, "Failed to send payload to be signed and gossipped");
            ctx.cancellation.cancel();
            return Err(SequencerActorError::ChannelClosed);
        }

        Ok(())
    }
}

#[async_trait]
impl NodeActor for SequencerActor<SequencerBuilder> {
    type Error = SequencerActorError;
    type OutboundData = SequencerContext;
    type Builder = SequencerBuilder;
    type InboundData = SequencerInboundData;

    fn build(config: Self::Builder) -> (Self::InboundData, Self) {
        Self::new(config)
    }

    async fn start(mut self, mut ctx: Self::OutboundData) -> Result<(), Self::Error> {
        let mut build_ticker =
            tokio::time::interval(Duration::from_secs(self.builder.cfg.block_time));

        let mut state = SequencerActorState::from(self.builder);
        // A channel to receive the latest built payload from the engine.
        let mut latest_payload_rx = None;

        loop {
            // Check if we are waiting on a block to be built. If so, we must wait for the response
            // before continuing.
            if let Some(payload) =
                state.try_wait_for_payload(&mut ctx, &mut latest_payload_rx).await?
            {
                state.schedule_gossip(&mut ctx, payload).await?;
            }

            select! {
                _ = ctx.cancellation.cancelled() => {
                    info!(
                        target: "sequencer",
                        "Received shutdown signal. Exiting sequencer task."
                    );
                    return Ok(());
                }
                _ = build_ticker.tick() => {
                    state.start_build(&mut ctx, &mut latest_payload_rx, &mut self.unsafe_head_rx).await?;
                }
            }
        }
    }
}
