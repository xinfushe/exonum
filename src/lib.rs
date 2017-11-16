//! Testkit for Exonum blockchain framework, allowing to test service APIs synchronously
//! and in the same process as the testkit.

#![deny(missing_docs)]

extern crate exonum;
extern crate futures;
extern crate iron;
extern crate iron_test;
extern crate mount;
extern crate router;
extern crate serde;
extern crate serde_json;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use exonum::blockchain::{Blockchain, ConsensusConfig, GenesisConfig,
                         Schema as CoreSchema, Service, StoredConfiguration,
                         Transaction, ValidatorKeys};
use exonum::crypto;
use exonum::helpers::{Height, Round, ValidatorId};
use exonum::messages::{Message, Precommit, Propose};
use exonum::node::{ApiSender, ExternalMessage, State as NodeState, TransactionSend, TxPool};
use exonum::storage::{MemoryDB, Snapshot, Database};

use futures::Stream;
use futures::executor::{self, Spawn};
use futures::sync::mpsc;
use iron::IronError;
use iron::headers::{ContentType, Headers};
use iron::status::StatusClass;
use iron_test::{request, response};
use mount::Mount;
use router::Router;
use serde::{Deserialize, Serialize};

#[macro_use]
mod macros;
pub mod compare;
mod greedy_fold;

#[doc(hidden)]
pub use greedy_fold::GreedilyFoldable;
pub use compare::ComparableSnapshot;

/// Emulated test network.
pub struct TestNetwork {
    us: TestNode,
    validators: Vec<TestNode>,
}

impl TestNetwork {
    /// Creates a new emulated network.
    pub fn new(validator_count: u16) -> Self {
        let validators = (0..validator_count)
            .map(ValidatorId)
            .map(TestNode::new_validator)
            .collect::<Vec<_>>();

        let us = validators[0].clone();
        TestNetwork { validators, us }
    }

    /// Returns the node in the emulated network, from whose perspective the testkit operates.
    pub fn us(&self) -> &TestNode {
        &self.us
    }

    /// Returns a slice of all validators in the network.
    pub fn validators(&self) -> &[TestNode] {
        &self.validators
    }

    /// Returns config encoding the network structure usable for creating the genesis block of
    /// a blockchain.
    pub fn genesis_config(&self) -> GenesisConfig {
        GenesisConfig::new(self.validators.iter().map(TestNode::public_keys))
    }

    /// Updates the test network by the new set of nodes.
    pub fn update<I: IntoIterator<Item = TestNode>>(&mut self, mut us: TestNode, validators: I) {
        let validators = validators
            .into_iter()
            .enumerate()
            .map(|(id, mut validator)| {
                let validator_id = ValidatorId(id as u16);
                validator.change_role(Some(validator_id));
                if us.public_keys().consensus_key == validator.public_keys().consensus_key {
                    us.change_role(Some(validator_id));
                }
                validator
            })
            .collect::<Vec<_>>();
        self.validators = validators;
        self.us.clone_from(&us);
    }

    /// Returns service public key of the validator with given id.
    pub fn service_public_key_of(&self, id: ValidatorId) -> Option<&crypto::PublicKey> {
        self.validators().get(id.0 as usize).map(|x| {
            &x.service_public_key
        })
    }

    /// Returns consensus public key of the validator with given id.
    pub fn consensus_public_key_of(&self, id: ValidatorId) -> Option<&crypto::PublicKey> {
        self.validators().get(id.0 as usize).map(|x| {
            &x.consensus_public_key
        })
    }
}

/// An emulated node in the test network.
#[derive(Debug, Clone, PartialEq)]
pub struct TestNode {
    consensus_secret_key: crypto::SecretKey,
    consensus_public_key: crypto::PublicKey,
    service_secret_key: crypto::SecretKey,
    service_public_key: crypto::PublicKey,
    validator_id: Option<ValidatorId>,
}

impl TestNode {
    /// Creates a new auditor.
    pub fn new_auditor() -> Self {
        let (consensus_public_key, consensus_secret_key) = crypto::gen_keypair();
        let (service_public_key, service_secret_key) = crypto::gen_keypair();

        TestNode {
            consensus_secret_key,
            consensus_public_key,
            service_secret_key,
            service_public_key,
            validator_id: None,
        }
    }

    /// Creates a new validator with the given id.
    pub fn new_validator(validator_id: ValidatorId) -> Self {
        let (consensus_public_key, consensus_secret_key) = crypto::gen_keypair();
        let (service_public_key, service_secret_key) = crypto::gen_keypair();

        TestNode {
            consensus_secret_key,
            consensus_public_key,
            service_secret_key,
            service_public_key,
            validator_id: Some(validator_id),
        }
    }

    /// Creates a `Propose` message signed by this validator.
    pub fn create_propose(
        &self,
        height: Height,
        last_hash: &crypto::Hash,
        tx_hashes: &[crypto::Hash],
    ) -> Propose {
        Propose::new(
            self.validator_id.expect(
                "An attempt to create propose from a non-validator node.",
            ),
            height,
            Round::first(),
            last_hash,
            tx_hashes,
            &self.consensus_secret_key,
        )
    }

    /// Creates a `Precommit` message signed by this validator.
    pub fn create_precommit(&self, propose: &Propose, block_hash: &crypto::Hash) -> Precommit {
        use std::time::SystemTime;

        Precommit::new(
            self.validator_id.expect(
                "An attempt to create propose from a non-validator node.",
            ),
            propose.height(),
            propose.round(),
            &propose.hash(),
            block_hash,
            SystemTime::now(),
            &self.consensus_secret_key,
        )
    }

    /// Returns public keys of the node.
    pub fn public_keys(&self) -> ValidatorKeys {
        ValidatorKeys {
            consensus_key: self.consensus_public_key,
            service_key: self.service_public_key,
        }
    }

    /// Returns the current validator id of node if it is validator of the test network.
    pub fn validator_id(&self) -> Option<ValidatorId> {
        self.validator_id
    }

    /// Change node role.
    pub fn change_role(&mut self, role: Option<ValidatorId>) {
        self.validator_id = role;
    }

    /// Returns the service keypar.
    pub fn service_keypair(&self) -> (&crypto::PublicKey, &crypto::SecretKey) {
        (&self.service_public_key, &self.service_secret_key)
    }
}

impl From<TestNode> for ValidatorKeys {
    fn from(node: TestNode) -> Self {
        node.public_keys()
    }
}

/// Builder for `TestKit`.
pub struct TestKitBuilder {
    us: TestNode,
    validators: Vec<TestNode>,
    services: Vec<Box<Service>>,
}

impl TestKitBuilder {
    /// Creates testkit for the validator node.
    pub fn validator() -> Self {
        let us = TestNode::new_validator(ValidatorId(0));
        TestKitBuilder {
            validators: vec![us.clone()],
            services: Vec::new(),
            us,
        }
    }

    /// Creates testkit for the auditor node.
    pub fn auditor() -> Self {
        let us = TestNode::new_auditor();
        TestKitBuilder {
            validators: vec![TestNode::new_validator(ValidatorId(0))],
            services: Vec::new(),
            us,
        }
    }

    /// Adds an additional validators.
    pub fn with_validators(mut self, validators_count: u16) -> Self {
        assert!(validators_count > 0, "At least one validator must be.");
        let additional_validators = (1..validators_count).map(ValidatorId).map(
            TestNode::new_validator,
        );
        self.validators.extend(additional_validators);
        self
    }

    /// Adds a service to the testkit.
    pub fn with_service<S>(mut self, service: S) -> Self
    where
        S: Into<Box<Service>>,
    {
        self.services.push(service.into());
        self
    }

    /// Creates the testkit.
    pub fn create(self) -> TestKit {
        crypto::init();
        let db = MemoryDB::new();
        TestKit::assemble(
            Box::new(db),
            self.services,
            TestNetwork {
                us: self.us,
                validators: self.validators,
            },
        )
    }
}

/// Testkit for testing blockchain services. It offers simple network configuration emulation
/// (with no real network setup).
pub struct TestKit {
    blockchain: Blockchain,
    events_stream: Spawn<Box<Stream<Item = (), Error = ()>>>,
    network: TestNetwork,
    api_sender: ApiSender,
    mempool: TxPool,
    cfg_proposal: Option<ConfigurationProposalState>,
}

impl TestKit {
    fn assemble(db: Box<Database>, services: Vec<Box<Service>>, network: TestNetwork) -> Self {
        let api_channel = mpsc::channel(1_000);
        let api_sender = ApiSender::new(api_channel.0.clone());

        let mut blockchain = Blockchain::new(db, services, *network.us().service_keypair().0,
        network.us().service_keypair().1.clone(), api_sender.clone());

        let genesis = network.genesis_config();
        blockchain.create_genesis_block(genesis.clone()).unwrap();

        let mempool = Arc::new(RwLock::new(BTreeMap::new()));
        let event_stream: Box<Stream<Item = (), Error = ()>> = {
            let blockchain = blockchain.clone();
            let mempool = Arc::clone(&mempool);
            Box::new(api_channel.1.greedy_fold((), move |_, event| {
                let snapshot = blockchain.snapshot();
                let schema = CoreSchema::new(&snapshot);
                match event {
                    ExternalMessage::Transaction(tx) => {
                        let hash = tx.hash();
                        if !schema.transactions().contains(&hash) {
                            mempool
                                .write()
                                .expect("Cannot write transactions to mempool")
                                .insert(tx.hash(), tx);
                        }
                    }
                    ExternalMessage::PeerAdd(_) => { /* Ignored */ }
                }
            }))
        };
        let events_stream = executor::spawn(event_stream);

        TestKit {
            blockchain,
            api_sender,
            events_stream,
            network,
            mempool: Arc::clone(&mempool),
            cfg_proposal: None,
        }
    }

    /// Creates a mounting point for public APIs used by the blockchain.
    fn public_api_mount(&self) -> Mount {
        self.blockchain.mount_public_api()
    }

    /// Creates a mounting point for public APIs used by the blockchain.
    fn private_api_mount(&self) -> Mount {
        self.blockchain.mount_private_api()
    }

    /// Creates an instance of `TestKitApi` to test the API provided by services.
    pub fn api(&self) -> TestKitApi {
        TestKitApi::new(self)
    }

    /// Polls the *existing* events from the event loop until exhaustion. Does not wait
    /// until new events arrive.
    pub fn poll_events(&mut self) -> Option<Result<(), ()>> {
        self.events_stream.wait_stream()
    }

    /// Returns a snapshot of the current blockchain state.
    pub fn snapshot(&self) -> Box<Snapshot> {
        self.blockchain.snapshot()
    }

    /// Executes a list of transactions given the current state of the blockchain, but does not
    /// commit execution results to the blockchain. The execution result is the same
    /// as if transactions were included into a new block; for example,
    /// transactions included into one of previous blocks do not lead to any state changes.
    ///
    /// # Panics
    ///
    /// If there are duplicate transactions.
    pub fn probe_all(&self, transactions: Vec<Box<Transaction>>) -> Box<Snapshot> {
        let validator_id = self.network().us().validator_id().expect(
            "Tested node is not a validator",
        );
        let height = self.current_height();

        let (transaction_map, hashes) = {
            let mut transaction_map = BTreeMap::new();
            let mut hashes = Vec::with_capacity(transactions.len());

            let core_schema = CoreSchema::new(self.snapshot());
            let committed_txs = core_schema.transactions();

            for tx in transactions {
                let hash = tx.hash();
                if committed_txs.contains(&hash) {
                    continue;
                }

                hashes.push(hash);
                transaction_map.insert(hash, tx);
            }

            assert_eq!(
                hashes.len(),
                transaction_map.len(),
                "Duplicate transactions in probe"
            );

            (transaction_map, hashes)
        };

        let (_, patch) = self.blockchain.create_patch(
            validator_id,
            height,
            &hashes,
            &transaction_map,
        );

        let mut fork = self.blockchain.fork();
        fork.merge(patch);
        Box::new(fork)
    }

    /// Executes a transaction given the current state of the blockchain but does not
    /// commit execution results to the blockchain. The execution result is the same
    /// as if a transaction was included into a new block; for example,
    /// a transaction included into one of previous blocks does not lead to any state changes.
    pub fn probe<T: Transaction>(&self, transaction: T) -> Box<Snapshot> {
        self.probe_all(vec![Box::new(transaction)])
    }

    fn do_create_block(&mut self, tx_hashes: &[crypto::Hash]) {
        let height = self.current_height();
        let last_hash = self.last_hash();

        self.update_configuration();

        let (block_hash, patch) = {
            let validator_id = self.leader().validator_id().expect(
                "Tested node is not a validator",
            );
            let transactions = self.mempool();
            self.blockchain.create_patch(
                validator_id,
                height,
                tx_hashes,
                &transactions,
            )
        };

        // Remove txs from mempool
        {
            let mut transactions = self.mempool.write().expect(
                "Cannot modify transactions in mempool",
            );
            for hash in tx_hashes {
                transactions.remove(hash);
            }
        }

        let propose = self.leader().create_propose(height, &last_hash, tx_hashes);
        let precommits: Vec<_> = self.network()
            .validators()
            .iter()
            .map(|v| v.create_precommit(&propose, &block_hash))
            .collect();

        self.blockchain
            .commit(&patch, block_hash, precommits.iter())
            .unwrap();

        self.poll_events();
    }

    /// Update test network configuration if such an update has been scheduled
    /// with `commit_configuration_change`.
    fn update_configuration(&mut self) {
        use ConfigurationProposalState::*;

        let height = self.current_height();
        if let Some(cfg_proposal) = self.cfg_proposal.take() {
            match cfg_proposal {
                Uncommitted(cfg_proposal) => {
                    // Commit configuration proposal
                    let stored = cfg_proposal.stored_configuration().clone();
                    let mut fork = self.blockchain.fork();
                    CoreSchema::new(&mut fork).commit_configuration(stored);
                    let changes = fork.into_patch();
                    self.blockchain.merge(changes).unwrap();
                    self.cfg_proposal = Some(Committed(cfg_proposal));
                }
                Committed(ref cfg_proposal) if cfg_proposal.actual_from() == height => {
                    // Modify the self configuration
                    self.network_mut().update(
                        cfg_proposal.us.clone(),
                        cfg_proposal.validators.clone(),
                    );
                }
                Committed(cfg_proposal) => {
                    self.cfg_proposal = Some(Committed(cfg_proposal));
                }
            }
        }
    }

    /// Creates block with the given transactions.
    /// Transactions that are in mempool will be ignored.
    ///
    /// # Panics
    ///
    /// If the one of transactions has been already committed to the blockchain.
    pub fn create_block_with_transactions(&mut self, txs: Vec<Box<Transaction>>) {
        let tx_hashes = {
            let mut mempool = self.mempool.write().expect(
                "Cannot write transactions to mempool",
            );

            let mut tx_hashes = Vec::with_capacity(txs.len());
            let snapshot = self.snapshot();
            let schema = CoreSchema::new(&snapshot);
            for tx in txs {
                let txid = tx.hash();
                assert!(
                    !schema.transactions().contains(&txid),
                    "Given transaction is already committed: {:?}",
                    tx
                );
                tx_hashes.push(txid);
                mempool.insert(txid, tx);
            }
            tx_hashes
        };
        self.create_block_with_tx_hashes(&tx_hashes);
    }

    /// Creates block with the specified transactions. The transactions must be previously
    /// sent to the node via API or directly put into the `channel()`.
    ///
    /// # Panics
    ///
    /// In the case any of transaction hashes are not in the mempool.
    pub fn create_block_with_tx_hashes(&mut self, tx_hashes: &[crypto::Hash]) {
        self.poll_events();

        {
            let txs = self.mempool();
            for hash in tx_hashes {
                assert!(txs.contains_key(hash));
            }
        }

        self.do_create_block(tx_hashes);
    }

    /// Creates block with all transactions in the mempool.
    pub fn create_block(&mut self) {
        self.poll_events();

        let tx_hashes: Vec<_> = self.mempool().keys().cloned().collect();

        self.do_create_block(&tx_hashes);
    }

    /// Creates a chain of blocks until a given height.
    pub fn create_blocks_until(&mut self, height: Height) {
        while self.current_height() <= height {
            self.create_block();
        }
    }

    /// Returns the current height of the blockchain. Its value is equal to `last_height + 1`.
    pub fn current_height(&self) -> Height {
        CoreSchema::new(&self.snapshot()).current_height()
    }

    /// Returns the hash of latest committed block.
    pub fn last_hash(&self) -> crypto::Hash {
        self.blockchain.last_hash()
    }

    /// Returns sufficient number of validators for the Byzantine Fault Toulerance consensus.
    pub fn majority_count(&self) -> usize {
        NodeState::byzantine_majority_count(self.network().validators().len())
    }

    /// Returns the test node memory pool handle.
    pub fn mempool(&self) -> RwLockReadGuard<BTreeMap<crypto::Hash, Box<Transaction>>> {
        self.mempool.read().expect(
            "Can't read transactions from the mempool.",
        )
    }

    /// Returns the leader on the current height. At the moment first validator.
    pub fn leader(&self) -> &TestNode {
        &self.network().validators[0]
    }

    /// Returns the reference to test network.
    pub fn network(&self) -> &TestNetwork {
        &self.network
    }

    /// Returns the mutable reference to test network for manual modifications.
    pub fn network_mut(&mut self) -> &mut TestNetwork {
        &mut self.network
    }

    /// Returns a copy of the actual configuration of the testkit.
    /// The returned configuration could be modified for use with
    /// `commit_configuration_change` method.
    pub fn configuration_change_proposal(&self) -> TestNetworkConfiguration {
        let stored_configuration = CoreSchema::new(&self.snapshot()).actual_configuration();
        TestNetworkConfiguration::from_parts(
            self.network().us().clone(),
            self.network().validators().into(),
            stored_configuration,
        )
    }

    /// Adds a new configuration proposal.
    ///
    /// # Panics
    ///
    /// - If `actual_from` is less than current height or equals.
    /// - If configuration change has been already proposed but not executed.
    pub fn commit_configuration_change(&mut self, proposal: TestNetworkConfiguration) {
        use self::ConfigurationProposalState::*;
        assert!(self.current_height() < proposal.actual_from());
        assert!(self.cfg_proposal.is_none());
        self.cfg_proposal = Some(Uncommitted(proposal));
    }
}

/// A configuration of the test network.
#[derive(Debug)]
pub struct TestNetworkConfiguration {
    us: TestNode,
    validators: Vec<TestNode>,
    stored_configuration: StoredConfiguration,
}

// A new configuration proposal state
#[derive(Debug)]
enum ConfigurationProposalState {
    Uncommitted(TestNetworkConfiguration),
    Committed(TestNetworkConfiguration),
}

impl TestNetworkConfiguration {
    fn from_parts(
        us: TestNode,
        validators: Vec<TestNode>,
        mut stored_configuration: StoredConfiguration,
    ) -> Self {
        let prev_hash = exonum::storage::StorageValue::hash(&stored_configuration);
        stored_configuration.previous_cfg_hash = prev_hash;
        TestNetworkConfiguration {
            us,
            validators,
            stored_configuration,
        }
    }

    /// Returns the testkit node.
    pub fn us(&self) -> &TestNode {
        &self.us
    }

    /// Modifies the testkit node.
    pub fn set_us(&mut self, us: TestNode) {
        self.us = us;
        self.update_our_role();
    }

    /// Returns the test network validators.
    pub fn validators(&self) -> &[TestNode] {
        self.validators.as_ref()
    }

    /// Returns the current consensus configuration.
    pub fn consensus_configuration(&self) -> &ConsensusConfig {
        &self.stored_configuration.consensus
    }

    /// Return the height, starting from which this configuration becomes actual.
    pub fn actual_from(&self) -> Height {
        self.stored_configuration.actual_from
    }

    /// Modifies the height, starting from which this configuration becomes actual.
    pub fn set_actual_from(&mut self, actual_from: Height) {
        self.stored_configuration.actual_from = actual_from;
    }

    /// Modifies the current consensus configuration.
    pub fn set_consensus_configuration(&mut self, consensus: ConsensusConfig) {
        self.stored_configuration.consensus = consensus;
    }

    /// Modifies the validators list.
    pub fn set_validators<I>(&mut self, validators: I)
    where
        I: IntoIterator<Item = TestNode>,
    {
        self.validators = validators
            .into_iter()
            .enumerate()
            .map(|(idx, mut node)| {
                node.change_role(Some(ValidatorId(idx as u16)));
                node
            })
            .collect();
        self.stored_configuration.validator_keys = self.validators
            .iter()
            .cloned()
            .map(ValidatorKeys::from)
            .collect();
        self.update_our_role();
    }

    /// Returns the configuration for service with the given identifier.
    pub fn service_config<D>(&self, id: &str) -> D
    where
        for<'de> D: Deserialize<'de>,
    {
        let value = self.stored_configuration.services.get(id).expect(
            "Unable to find configuration for service",
        );
        serde_json::from_value(value.clone()).unwrap()
    }

    /// Modifies the configuration of the service with the given identifier.
    pub fn set_service_config<D>(&mut self, id: &str, config: D)
    where
        D: Serialize,
    {
        let value = serde_json::to_value(config).unwrap();
        self.stored_configuration.services.insert(id.into(), value);
    }

    /// Returns the resulting exonum blockchain configuration.
    pub fn stored_configuration(&self) -> &StoredConfiguration {
        &self.stored_configuration
    }

    fn update_our_role(&mut self) {
        let validator_id = self.validators
            .iter()
            .position(|x| {
                x.public_keys().service_key == self.us.service_public_key
            })
            .map(|x| ValidatorId(x as u16));
        self.us.validator_id = validator_id;
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub enum ApiKind {
    System,
    Explorer,
    Service(&'static str),
}

impl ApiKind {
    fn into_prefix(self) -> String {
        match self {
            ApiKind::System => "api/system".to_string(),
            ApiKind::Explorer => "api/explorer".to_string(),
            ApiKind::Service(name) => format!("api/services/{}", name),
        }
    }
}

/// API encapsulation for the testkit. Allows to execute and synchronously retrieve results
/// for REST-ful endpoints of services.
pub struct TestKitApi {
    public_mount: Mount,
    private_mount: Mount,
    api_sender: ApiSender,
}

impl TestKitApi {
    /// Creates a new instance of Api.
    fn new(testkit: &TestKit) -> Self {
        use std::sync::Arc;
        use exonum::api::{public, Api};

        let blockchain = &testkit.blockchain;

        TestKitApi {
            public_mount: {
                let mut mount = Mount::new();

                let service_mount = testkit.public_api_mount();
                mount.mount("api/services", service_mount);

                let mut router = Router::new();
                let pool = Arc::clone(&testkit.mempool);
                let system_api = public::SystemApi::new(pool, blockchain.clone());
                system_api.wire(&mut router);
                mount.mount("api/system", router);

                let mut router = Router::new();
                let explorer_api = public::ExplorerApi::new(blockchain.clone());
                explorer_api.wire(&mut router);
                mount.mount("api/explorer", router);

                mount
            },

            private_mount: {
                let mut mount = Mount::new();

                let service_mount = testkit.private_api_mount();
                mount.mount("api/services", service_mount);

                mount
            },

            api_sender: testkit.api_sender.clone(),
        }
    }

    /// Returns the mounting point for public APIs. Useful for intricate testing not covered
    /// by `get*` and `post*` functions.
    pub fn public_mount(&self) -> &Mount {
        &self.public_mount
    }

    /// Returns the mounting point for private APIs. Useful for intricate testing not covered
    /// by `get*` and `post*` functions.
    pub fn private_mount(&self) -> &Mount {
        &self.private_mount
    }

    /// Sends a transaction to the node via `ApiSender`.
    pub fn send<T: Transaction>(&self, transaction: T) {
        self.api_sender.send(Box::new(transaction)).expect(
            "Cannot send transaction",
        );
    }

    fn get_internal<D>(mount: &Mount, url: &str, expect_error: bool) -> D
    where
        for<'de> D: Deserialize<'de>,
    {
        let status_class = if expect_error {
            StatusClass::ClientError
        } else {
            StatusClass::Success
        };

        let url = format!("http://localhost:3000/{}", url);
        let resp = request::get(&url, Headers::new(), mount);
        let resp = if expect_error {
            // Support either "normal" or erroneous responses.
            // For example, `Api.not_found_response()` returns the response as `Ok(..)`.
            match resp {
                Ok(resp) => resp,
                Err(IronError { response, .. }) => response,
            }
        } else {
            resp.expect("Got unexpected `Err(..)` response")
        };

        if let Some(ref status) = resp.status {
            if status.class() != status_class {
                panic!("Unexpected response status: {:?}", status);
            }
        } else {
            panic!("Response status not set");
        }

        let resp = response::extract_body_to_string(resp);
        serde_json::from_str(&resp).unwrap()
    }

    /// Gets information from a public endpoint of the node.
    pub fn get<D>(&self, kind: ApiKind, endpoint: &str) -> D
    where
        for<'de> D: Deserialize<'de>,
    {
        TestKitApi::get_internal(
            &self.public_mount,
            &format!("{}/{}", kind.into_prefix(), endpoint),
            false,
        )
    }

    /// Gets information from a private endpoint of the node.
    pub fn get_private<D>(&self, kind: ApiKind, endpoint: &str) -> D
    where
        for<'de> D: Deserialize<'de>,
    {
        TestKitApi::get_internal(
            &self.public_mount,
            &format!("{}/{}", kind.into_prefix(), endpoint),
            false,
        )
    }

    /// Gets an error from a public endpoint of the node.
    pub fn get_err<D>(&self, kind: ApiKind, endpoint: &str) -> D
    where
        for<'de> D: Deserialize<'de>,
    {
        TestKitApi::get_internal(
            &self.public_mount,
            &format!("{}/{}", kind.into_prefix(), endpoint),
            true,
        )
    }

    fn post_internal<T, D>(mount: &Mount, endpoint: &str, data: &T) -> D
    where
        T: Serialize,
        for<'de> D: Deserialize<'de>,
    {
        let url = format!("http://localhost:3000/{}", endpoint);
        let resp = request::post(
            &url,
            {
                let mut headers = Headers::new();
                headers.set(ContentType::json());
                headers
            },
            &serde_json::to_string(&data).expect("Cannot serialize data to JSON"),
            mount,
        ).expect("Cannot send data");

        let resp = response::extract_body_to_string(resp);
        serde_json::from_str(&resp).expect("Cannot parse result")
    }

    /// Posts a transaction to the service using the public API. The returned value is the result
    /// of synchronous transaction processing, which includes running the API shim
    /// and `Transaction.verify()`. `Transaction.execute()` is not run until the transaction
    /// gets to a block via one of `create_block*()` methods.
    pub fn post<T, D>(&self, kind: ApiKind, endpoint: &str, transaction: &T) -> D
    where
        T: Serialize,
        for<'de> D: Deserialize<'de>,
    {
        TestKitApi::post_internal(
            &self.public_mount,
            &format!("{}/{}", kind.into_prefix(), endpoint),
            transaction,
        )
    }

    /// Posts a transaction to the service using the private API. The returned value is the result
    /// of synchronous transaction processing, which includes running the API shim
    /// and `Transaction.verify()`. `Transaction.execute()` is not run until the transaction
    /// gets to a block via one of `create_block*()` methods.
    pub fn post_private<T, D>(&self, kind: ApiKind, endpoint: &str, transaction: &T) -> D
    where
        T: Serialize,
        for<'de> D: Deserialize<'de>,
    {
        TestKitApi::post_internal(
            &self.private_mount,
            &format!("{}/{}", kind.into_prefix(), endpoint),
            transaction,
        )
    }
}

#[test]
fn test_create_block_heights() {
    let mut testkit = TestKitBuilder::validator().create();
    assert_eq!(Height(1), testkit.current_height());
    testkit.create_block();
    assert_eq!(Height(2), testkit.current_height());
    testkit.create_blocks_until(Height(6));
    assert_eq!(Height(7), testkit.current_height());
}
