// Copyright 2022 Cartesi Pte. Ltd.
//
// SPDX-License-Identifier: Apache-2.0
// Licensed under the Apache License, Version 2.0 (the "License"); you may not use
// this file except in compliance with the License. You may obtain a copy of the
// License at http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

use anyhow::Result;
use async_trait::async_trait;
use backoff::ExponentialBackoffBuilder;
use snafu::{ResultExt, Snafu};
use tokio::sync::{self, Mutex};

use rollups_events::{
    Broker, BrokerError, Event, InputMetadata, RollupsAdvanceStateInput,
    RollupsClaim, RollupsClaimsStream, RollupsData, RollupsInput,
    RollupsInputsStream, INITIAL_ID,
};
use types::foldables::input_box::Input;

use super::{
    config::BrokerConfig, BrokerReceive, BrokerSend, BrokerStatus, RollupStatus,
};

#[derive(Debug, Snafu)]
pub enum BrokerFacadeError {
    #[snafu(display("error connecting to the broker"))]
    BrokerConnectionError { source: BrokerError },

    #[snafu(display("error peeking at the end of the stream"))]
    PeekInputError { source: BrokerError },

    #[snafu(display("error producing input event"))]
    ProduceInputError { source: BrokerError },

    #[snafu(display("error producing finish-epoch event"))]
    ProduceFinishError { source: BrokerError },

    #[snafu(display("error consuming claim event"))]
    ConsumeClaimError { source: BrokerError },
}

#[derive(Debug)]
pub struct BrokerFacade {
    broker: Mutex<Broker>,
    inputs_stream: RollupsInputsStream,
    claims_stream: RollupsClaimsStream,
    last_claim_id: Mutex<String>,
}

struct BrokerStreamStatus {
    id: String,
    epoch_number: u64,
    status: RollupStatus,
}

impl BrokerFacade {
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn new(config: BrokerConfig) -> Result<Self> {
        tracing::trace!(?config, "connection to the broker");

        let backoff = ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(config.backoff_max_elapsed_duration))
            .build();
        let broker_config = rollups_events::BrokerConfig {
            redis_endpoint: config.redis_endpoint,
            backoff,
            consume_timeout: config.claims_consume_timeout,
        };
        let broker = Mutex::new(
            Broker::new(broker_config)
                .await
                .context(BrokerConnectionSnafu)?,
        );

        tracing::trace!("connected to the broker successfully");

        let dapp_metadata = rollups_events::DAppMetadata {
            chain_id: config.chain_id,
            dapp_address: config.dapp_contract_address.into(),
        };

        let inputs_stream = RollupsInputsStream::new(&dapp_metadata);

        let claims_stream = RollupsClaimsStream::new(&dapp_metadata);

        Ok(Self {
            broker,
            inputs_stream,
            claims_stream,
            last_claim_id: Mutex::new(INITIAL_ID.to_owned()),
        })
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn broker_status(
        &self,
        broker: &mut sync::MutexGuard<'_, Broker>,
    ) -> Result<BrokerStreamStatus> {
        let event = self.peek(broker).await?;
        Ok(event.into())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn peek(
        &self,
        broker: &mut sync::MutexGuard<'_, Broker>,
    ) -> Result<Option<Event<RollupsInput>>> {
        tracing::trace!("peeking last produced event");
        let response = broker
            .peek_latest(&self.inputs_stream)
            .await
            .context(PeekInputSnafu)?;
        tracing::trace!(?response, "got response");

        Ok(response)
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn claim(&self, id: &String) -> Result<Option<Event<RollupsClaim>>> {
        let mut broker = self.broker.lock().await;
        let event = broker
            .consume_nonblocking(&self.claims_stream, id)
            .await
            .context(ConsumeClaimSnafu)?;

        tracing::trace!(?event, "consumed event");

        Ok(event)
    }
}

#[async_trait]
impl BrokerStatus for BrokerFacade {
    #[tracing::instrument(level = "trace", skip_all)]
    async fn status(&self) -> Result<RollupStatus> {
        tracing::trace!("querying broker status");
        let mut broker = self.broker.lock().await;
        let status = self.broker_status(&mut broker).await?.status;
        tracing::trace!(?status, "returning rollup status");
        Ok(status)
    }
}

macro_rules! input_sanity_check {
    ($event:expr, $input_index:expr) => {
        assert_eq!($event.inputs_sent_count, $input_index + 1);
        assert!(matches!(
            $event.data,
            RollupsData::AdvanceStateInput(RollupsAdvanceStateInput {
                metadata: InputMetadata {
                    epoch_index,
                    ..
                },
                ..
            }) if epoch_index == 0
        ));
        assert!(matches!(
            $event.data,
            RollupsData::AdvanceStateInput(RollupsAdvanceStateInput {
                metadata: InputMetadata {
                    input_index,
                    ..
                },
                ..
            }) if input_index == $input_index
        ));
    };
}

macro_rules! epoch_sanity_check {
    ($event:expr, $inputs_sent_count:expr) => {
        assert_eq!($event.inputs_sent_count, $inputs_sent_count);
        assert!(matches!($event.data, RollupsData::FinishEpoch { .. }));
    };
}

#[async_trait]
impl BrokerSend for BrokerFacade {
    #[tracing::instrument(level = "trace", skip_all)]
    async fn enqueue_input(
        &self,
        input_index: u64,
        input: &Input,
    ) -> Result<()> {
        tracing::trace!(?input_index, ?input, "enqueueing input");

        let mut broker = self.broker.lock().await;
        let status = self.broker_status(&mut broker).await?;

        let event = build_next_input(input, &status);
        tracing::trace!(?event, "producing input event");

        input_sanity_check!(event, input_index);

        let id = broker
            .produce(&self.inputs_stream, event)
            .await
            .context(ProduceInputSnafu)?;
        tracing::trace!(id, "produced event with id");

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn finish_epoch(&self, inputs_sent_count: u64) -> Result<()> {
        tracing::trace!(?inputs_sent_count, "finishing epoch");

        let mut broker = self.broker.lock().await;
        let status = self.broker_status(&mut broker).await?;

        let event = build_next_finish_epoch(&status);
        tracing::trace!(?event, "producing finish epoch event");

        epoch_sanity_check!(event, inputs_sent_count);

        let id = broker
            .produce(&self.inputs_stream, event)
            .await
            .context(ProduceFinishSnafu)?;

        tracing::trace!(id, "produce event with id");

        Ok(())
    }
}

#[async_trait]
impl BrokerReceive for BrokerFacade {
    #[tracing::instrument(level = "trace", skip_all)]
    async fn next_claim(&self) -> Result<Option<super::RollupClaim>> {
        let mut last_id = self.last_claim_id.lock().await;
        tracing::trace!(?last_id, "getting next epoch claim");

        match self.claim(&last_id).await? {
            Some(event) => {
                *last_id = event.id.clone();
                Ok(Some(event.into()))
            }
            None => Ok(None),
        }
    }
}

impl From<RollupsInput> for RollupStatus {
    fn from(payload: RollupsInput) -> Self {
        let inputs_sent_count = payload.inputs_sent_count;

        match payload.data {
            RollupsData::AdvanceStateInput { .. } => RollupStatus {
                inputs_sent_count,
                last_event_is_finish_epoch: false,
            },

            RollupsData::FinishEpoch { .. } => RollupStatus {
                inputs_sent_count,
                last_event_is_finish_epoch: true,
            },
        }
    }
}

impl From<Event<RollupsInput>> for BrokerStreamStatus {
    fn from(event: Event<RollupsInput>) -> Self {
        let id = event.id;
        let payload = event.payload;
        let epoch_index = payload.epoch_index;

        match payload.data {
            RollupsData::AdvanceStateInput { .. } => Self {
                id,
                epoch_number: epoch_index,
                status: payload.into(),
            },

            RollupsData::FinishEpoch { .. } => Self {
                id,
                epoch_number: epoch_index + 1,
                status: payload.into(),
            },
        }
    }
}

impl From<Option<Event<RollupsInput>>> for BrokerStreamStatus {
    fn from(event: Option<Event<RollupsInput>>) -> Self {
        match event {
            Some(e) => e.into(),

            None => Self {
                id: INITIAL_ID.to_owned(),
                epoch_number: 0,
                status: RollupStatus::default(),
            },
        }
    }
}

fn build_next_input(
    input: &Input,
    status: &BrokerStreamStatus,
) -> RollupsInput {
    let metadata = InputMetadata {
        msg_sender: input.sender.to_fixed_bytes().into(),
        block_number: input.block_added.number.as_u64(),
        timestamp: input.block_added.timestamp.as_u64(),
        epoch_index: 0,
        input_index: status.status.inputs_sent_count,
    };

    let data = RollupsData::AdvanceStateInput(RollupsAdvanceStateInput {
        metadata,
        payload: input.payload.clone().into(),
        tx_hash: (*input.tx_hash).0.into(),
    });

    RollupsInput {
        parent_id: status.id.clone(),
        epoch_index: status.epoch_number,
        inputs_sent_count: status.status.inputs_sent_count + 1,
        data,
    }
}

fn build_next_finish_epoch(status: &BrokerStreamStatus) -> RollupsInput {
    RollupsInput {
        parent_id: status.id.clone(),
        epoch_index: status.epoch_number,
        inputs_sent_count: status.status.inputs_sent_count,
        data: RollupsData::FinishEpoch {},
    }
}

impl From<Event<RollupsClaim>> for super::RollupClaim {
    fn from(event: Event<RollupsClaim>) -> Self {
        super::RollupClaim {
            hash: event.payload.claim.into_inner(),
            number: event.payload.epoch_index,
        }
    }
}

#[cfg(test)]
mod broker_facade_tests {
    use std::{sync::Arc, time::Duration};

    use rollups_events::{
        Hash, InputMetadata, Payload, RedactedUrl, RollupsAdvanceStateInput,
        RollupsData, Url, HASH_SIZE,
    };
    use state_fold_types::{
        ethereum_types::{Bloom, H160, H256, U256, U64},
        Block,
    };
    use test_fixtures::broker::BrokerFixture;
    use testcontainers::clients::Cli;
    use types::foldables::input_box::Input;

    use crate::machine::{
        config::BrokerConfig, BrokerReceive, BrokerSend, BrokerStatus,
    };

    use super::BrokerFacade;

    // --------------------------------------------------------------------------------------------
    // new
    // --------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn new_ok() {
        let docker = Cli::default();
        let (_fixture, _broker) = setup(&docker).await;
    }

    #[tokio::test]
    async fn new_error() {
        let result = BrokerFacade::new(BrokerConfig {
            redis_endpoint: Url::parse("redis://invalid")
                .map(RedactedUrl::new)
                .expect("failed to parse Redis Url"),
            chain_id: 1,
            dapp_contract_address: [0; 20],
            claims_consume_timeout: 300000,
            backoff_max_elapsed_duration: Duration::from_millis(1000),
        })
        .await;
        let error = result
            .err()
            .expect("'status' function has not failed")
            .to_string();
        // BrokerFacadeError::BrokerConnectionError
        assert_eq!(error, "error connecting to the broker");
    }

    // --------------------------------------------------------------------------------------------
    // status
    // --------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn status_inputs_sent_count_equals_0() {
        let docker = Cli::default();
        let (_fixture, broker) = setup(&docker).await;
        let status = broker.status().await.expect("'status' function failed");
        assert_eq!(status.inputs_sent_count, 0);
        assert!(!status.last_event_is_finish_epoch);
    }

    #[tokio::test]
    async fn status_inputs_sent_count_equals_1() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;
        produce_advance_state_inputs(&fixture, 1).await;
        let status = broker.status().await.expect("'status' function failed");
        assert_eq!(status.inputs_sent_count, 1);
        assert!(!status.last_event_is_finish_epoch);
    }

    #[tokio::test]
    async fn status_inputs_sent_count_equals_10() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;
        produce_advance_state_inputs(&fixture, 10).await;
        let status = broker.status().await.expect("'status' function failed");
        assert_eq!(status.inputs_sent_count, 10);
        assert!(!status.last_event_is_finish_epoch);
    }

    #[tokio::test]
    async fn status_is_finish_epoch() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;
        produce_finish_epoch_input(&fixture).await;
        let status = broker.status().await.expect("'status' function failed");
        assert_eq!(status.inputs_sent_count, 0);
        assert!(status.last_event_is_finish_epoch);
    }

    #[tokio::test]
    async fn status_inputs_with_finish_epoch() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;
        produce_advance_state_inputs(&fixture, 5).await;
        produce_finish_epoch_input(&fixture).await;
        let status = broker.status().await.expect("'status' function failed");
        assert_eq!(status.inputs_sent_count, 5);
        assert!(status.last_event_is_finish_epoch);
    }

    // --------------------------------------------------------------------------------------------
    // enqueue_input
    // --------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn enqueue_input_ok() {
        let docker = Cli::default();
        let (_fixture, broker) = setup(&docker).await;
        for i in 0..3 {
            assert!(broker
                .enqueue_input(i, &new_enqueue_input())
                .await
                .is_ok());
        }
    }

    #[tokio::test]
    #[should_panic(expected = "left: `1`,\n right: `6`")]
    async fn enqueue_input_assertion_error_1() {
        let docker = Cli::default();
        let (_fixture, broker) = setup(&docker).await;
        let _ = broker.enqueue_input(5, &new_enqueue_input()).await;
    }

    #[tokio::test]
    #[should_panic(expected = "left: `5`,\n right: `6`")]
    async fn enqueue_input_assertion_error_2() {
        let docker = Cli::default();
        let (_fixture, broker) = setup(&docker).await;
        for i in 0..4 {
            assert!(broker
                .enqueue_input(i, &new_enqueue_input())
                .await
                .is_ok());
        }
        let _ = broker.enqueue_input(5, &new_enqueue_input()).await;
    }

    // NOTE: cannot test result error because the dependency is not injectable.

    // --------------------------------------------------------------------------------------------
    // finish_epoch
    // --------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn finish_epoch_ok_1() {
        let docker = Cli::default();
        let (_fixture, broker) = setup(&docker).await;
        assert!(broker.finish_epoch(0).await.is_ok());
        // BONUS TEST: testing for a finished epoch with no inputs
        assert!(broker.finish_epoch(0).await.is_ok());
    }

    #[tokio::test]
    async fn finish_epoch_ok_2() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;
        produce_advance_state_inputs(&fixture, 3).await;
        produce_finish_epoch_input(&fixture).await;
        let n = 7;
        produce_advance_state_inputs(&fixture, n).await;
        assert!(broker.finish_epoch(n as u64).await.is_ok());
    }

    #[tokio::test]
    #[should_panic(expected = "left: `0`,\n right: `1`")]
    async fn finish_epoch_assertion_error() {
        let docker = Cli::default();
        let (_fixture, broker) = setup(&docker).await;
        let _ = broker.finish_epoch(1).await;
    }

    // NOTE: cannot test result error because the dependency is not injectable.

    // --------------------------------------------------------------------------------------------
    // next_claim
    // --------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn next_claim_is_none() {
        let docker = Cli::default();
        let (_fixture, broker) = setup(&docker).await;
        let option = broker
            .next_claim()
            .await
            .expect("'next_claim' function failed");
        assert!(option.is_none());
    }

    #[tokio::test]
    async fn next_claim_is_some() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;

        let hashes = produce_claims(&fixture, 1).await;
        let claim = broker
            .next_claim()
            .await
            .expect("'next_claim' function failed")
            .expect("no claims retrieved");

        assert_eq!(hashes[0].inner().to_owned(), claim.hash);
        assert_eq!(0, claim.number);
    }

    #[tokio::test]
    async fn next_claim_is_some_sequential() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;

        let n = 3;
        let hashes = produce_claims(&fixture, n).await;
        for i in 0..n {
            let claim = broker
                .next_claim()
                .await
                .expect("'next_claim' function failed")
                .expect("no claims retrieved");
            assert_eq!(hashes[i as usize].inner().to_owned(), claim.hash);
            assert_eq!(i as u64, claim.number);
        }
    }

    #[tokio::test]
    async fn next_claim_is_some_interleaved() {
        let docker = Cli::default();
        let (fixture, broker) = setup(&docker).await;

        for i in 0..5 {
            let hash = Hash::new([i; HASH_SIZE]);
            fixture.produce_claim(hash.clone()).await;
            let claim = broker
                .next_claim()
                .await
                .expect("'next_claim' function failed")
                .expect("no claims retrieved");
            assert_eq!(hash.inner().to_owned(), claim.hash);
            assert_eq!(i as u64, claim.number);
        }
    }

    // --------------------------------------------------------------------------------------------
    // auxiliary
    // --------------------------------------------------------------------------------------------

    async fn setup(docker: &Cli) -> (BrokerFixture, BrokerFacade) {
        let fixture = BrokerFixture::setup(docker).await;
        let config = BrokerConfig {
            redis_endpoint: fixture.redis_endpoint().to_owned(),
            chain_id: fixture.chain_id(),
            dapp_contract_address: fixture.dapp_address().inner().to_owned(),
            claims_consume_timeout: 300000,
            backoff_max_elapsed_duration: Duration::from_millis(1000),
        };
        let broker = BrokerFacade::new(config).await.unwrap();
        (fixture, broker)
    }

    fn new_enqueue_input() -> Input {
        Input {
            sender: Arc::new(H160::random()),
            payload: vec![],
            block_added: Arc::new(Block {
                hash: H256::random(),
                number: U64::zero(),
                parent_hash: H256::random(),
                timestamp: U256::zero(),
                logs_bloom: Bloom::default(),
            }),
            dapp: Arc::new(H160::random()),
            tx_hash: Arc::new(H256::random()),
        }
    }

    async fn produce_advance_state_inputs(fixture: &BrokerFixture<'_>, n: u32) {
        for _ in 0..n {
            let _ = fixture
                .produce_input_event(RollupsData::AdvanceStateInput(
                    RollupsAdvanceStateInput {
                        metadata: InputMetadata::default(),
                        payload: Payload::default(),
                        tx_hash: Hash::default(),
                    },
                ))
                .await;
        }
    }

    async fn produce_finish_epoch_input(fixture: &BrokerFixture<'_>) {
        let _ = fixture
            .produce_input_event(RollupsData::FinishEpoch {})
            .await;
    }

    async fn produce_claims(fixture: &BrokerFixture<'_>, n: u32) -> Vec<Hash> {
        let mut hashes = Vec::new();
        for i in 0..n {
            let hash = Hash::new([i as u8; HASH_SIZE]);
            fixture.produce_claim(hash.clone()).await;
            hashes.push(hash);
        }
        hashes
    }
}
