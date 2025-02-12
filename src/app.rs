use crate::{
    contracts,
    contracts::{
        batching::Contract as BatchingContract, legacy::Contract as LegacyContract,
        IdentityManager, SharedIdentityManager,
    },
    database::{self, Database},
    ethereum::{self, Ethereum},
    ethereum_subscriber::{Error as SubscriberError, EthereumSubscriber},
    identity_committer::IdentityCommitter,
    identity_tree::{Hash, SharedTreeState, TreeState},
    prover,
    server::{Error as ServerError, ToResponseCode},
    timed_rw_lock::TimedRwLock,
};
use anyhow::{anyhow, Result as AnyhowResult};
use clap::Parser;
use cli_batteries::await_shutdown;
use ethers::types::U256;
use futures::TryFutureExt;
use hyper::StatusCode;
use semaphore::{poseidon_tree::Proof, Field};
use serde::{ser::SerializeStruct, Serialize, Serializer};
use std::{sync::Arc, time::Duration};
use tokio::{select, try_join};
use tracing::{error, info, instrument, warn};

pub enum InclusionProofResponse {
    Proof { root: Field, proof: Proof },
    Pending,
}

impl ToResponseCode for InclusionProofResponse {
    fn to_response_code(&self) -> StatusCode {
        match self {
            Self::Proof { .. } => StatusCode::OK,
            Self::Pending => StatusCode::ACCEPTED,
        }
    }
}

impl Serialize for InclusionProofResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Proof { root, proof } => {
                let mut state = serializer.serialize_struct("InclusionProof", 2)?;
                state.serialize_field("root", root)?;
                state.serialize_field("proof", proof)?;
                state.end()
            }
            Self::Pending => serializer.serialize_str("pending"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Parser)]
#[group(skip)]
pub struct Options {
    #[clap(flatten)]
    pub ethereum: ethereum::Options,

    #[clap(flatten)]
    pub contracts: contracts::Options,

    #[clap(flatten)]
    pub database: database::Options,

    #[clap(flatten)]
    pub prover: prover::Options,

    /// Block number to start syncing from
    #[clap(long, env, default_value = "0")]
    pub starting_block: u64,

    /// Timeout for the tree lock (seconds).
    #[clap(long, env, default_value = "120")]
    pub lock_timeout: u64,
}

pub struct App {
    database:           Arc<Database>,
    #[allow(dead_code)]
    ethereum:           Ethereum,
    identity_manager:   SharedIdentityManager,
    identity_committer: Arc<IdentityCommitter>,
    #[allow(dead_code)]
    chain_subscriber:   EthereumSubscriber,
    tree_state:         SharedTreeState,
    snark_scalar_field: Hash,
}

impl App {
    /// # Errors
    ///
    /// Will return `Err` if the internal Ethereum handler errors or if the
    /// `options.storage_file` is not accessible.
    #[allow(clippy::missing_panics_doc)] // TODO
    #[instrument(name = "App::new", level = "debug")]
    pub async fn new(options: Options) -> AnyhowResult<Self> {
        let refresh_rate = options.ethereum.refresh_rate;
        let cache_recovery_step_size = options.ethereum.cache_recovery_step_size;

        // Connect to Ethereum and Database
        let (database, (ethereum, identity_manager)) = {
            let db = Database::new(options.database);

            let eth = Ethereum::new(options.ethereum).and_then(|ethereum| async move {
                let identity_manager = if cfg!(feature = "batching-contract") {
                    BatchingContract::new(options.contracts, ethereum.clone()).await?;
                    panic!("The batching contract does not yet exist but was requested.");
                } else {
                    LegacyContract::new(options.contracts, ethereum.clone()).await?
                };
                Ok((ethereum, Arc::new(identity_manager)))
            });

            // Connect to both in parallel
            try_join!(db, eth)?
        };
        let database = Arc::new(database);

        // Poseidon tree depth is one more than the contract's tree depth
        let tree_state = Arc::new(TimedRwLock::new(
            Duration::from_secs(options.lock_timeout),
            TreeState::new(
                identity_manager.tree_depth() + 1,
                identity_manager.initial_leaf_value(),
            ),
        ));

        let identity_committer = Arc::new(IdentityCommitter::new(
            database.clone(),
            identity_manager.clone(),
            tree_state.clone(),
        ));
        let chain_subscriber = EthereumSubscriber::new(
            options.starting_block,
            database.clone(),
            identity_manager.clone(),
            tree_state.clone(),
            identity_committer.clone(),
        );

        let snark_scalar_field = Hash::from_str_radix(
            "21888242871839275222246405745257275088548364400416034343698204186575808495617",
            10,
        )
        .expect("This should just parse.");

        // Sync with chain on start up
        let mut app = Self {
            database,
            ethereum,
            identity_manager,
            identity_committer,
            chain_subscriber,
            tree_state,
            snark_scalar_field,
        };

        select! {
            _ = app.load_initial_events(options.lock_timeout, options.starting_block, cache_recovery_step_size) => {},
            _ = await_shutdown() => return Err(anyhow!("Interrupted"))
        }

        // Basic sanity checks on the merkle tree
        app.chain_subscriber.check_health().await;

        // Listen to Ethereum events
        app.chain_subscriber.start(refresh_rate).await;

        // Process to push new identities to Ethereum
        app.identity_committer.start().await;

        Ok(app)
    }

    async fn load_initial_events(
        &mut self,
        lock_timeout: u64,
        starting_block: u64,
        cache_recovery_step_size: usize,
    ) -> AnyhowResult<()> {
        let mut root_mismatch_count = 0;
        loop {
            if root_mismatch_count == 1 {
                error!(cache_recovery_step_size, "Removing most recent cache.");
                self.database
                    .delete_most_recent_cached_events(cache_recovery_step_size as i64)
                    .await?;
            } else if root_mismatch_count == 2 {
                error!("Wiping out the entire cache.");
                self.database.wipe_cache().await?;
            } else if root_mismatch_count >= 3 {
                return Err(SubscriberError::RootMismatch.into());
            }

            match self.chain_subscriber.process_initial_events().await {
                Err(SubscriberError::RootMismatch) => {
                    error!("Error when rebuilding tree from cache.");
                    root_mismatch_count += 1;

                    // Create a new empty MerkleTree
                    self.tree_state = Arc::new(TimedRwLock::new(
                        Duration::from_secs(lock_timeout),
                        TreeState::new(
                            self.identity_manager.tree_depth() + 1,
                            self.identity_manager.initial_leaf_value(),
                        ),
                    ));

                    // Retry
                    self.chain_subscriber = EthereumSubscriber::new(
                        starting_block,
                        self.database.clone(),
                        self.identity_manager.clone(),
                        self.tree_state.clone(),
                        self.identity_committer.clone(),
                    );
                }
                Err(e) => return Err(e.into()),
                Ok(_) => return Ok(()),
            }
        }
    }

    fn identity_is_reduced(&self, commitment: Hash) -> bool {
        commitment.lt(&self.snark_scalar_field)
    }

    /// Queues an insert into the merkle tree.
    ///
    /// # Errors
    ///
    /// Will return `Err` if identity is already queued, or in the tree, or the
    /// queue malfunctions.
    #[instrument(level = "debug", skip_all)]
    pub async fn insert_identity(
        &self,
        group_id: usize,
        commitment: Hash,
    ) -> Result<(), ServerError> {
        if U256::from(group_id) != self.identity_manager.group_id() {
            return Err(ServerError::InvalidGroupId);
        }

        if commitment == self.identity_manager.initial_leaf_value() {
            warn!(?commitment, "Attempt to insert initial leaf.");
            return Err(ServerError::InvalidCommitment);
        }

        if !self.identity_is_reduced(commitment) {
            warn!(
                ?commitment,
                "The provided commitment is not an element of the field."
            );
            return Err(ServerError::UnreducedCommitment);
        }

        // Note the ordering of duplicate checks: since we never want to lose data,
        // pending identities are removed from the DB _after_ they are inserted into the
        // tree. Therefore this order of checks guarantees we will not insert a
        // duplicate.
        if self
            .database
            .pending_identity_exists(group_id, &commitment)
            .await?
        {
            warn!(?commitment, "Pending identity already exists.");
            return Err(ServerError::DuplicateCommitment);
        }

        {
            let tree = self.tree_state.read().await?;
            if let Some(existing) = tree
                .merkle_tree
                .leaves()
                .iter()
                .position(|&x| x == commitment)
            {
                warn!(?existing, ?commitment, next = %tree.next_leaf, "Commitment already exists in tree.");
                return Err(ServerError::DuplicateCommitment);
            }
        }

        self.database
            .insert_pending_identity(group_id, &commitment)
            .await?;

        self.identity_committer.notify_queued().await;

        Ok(())
    }

    /// # Errors
    ///
    /// Will return `Err` if the provided index is out of bounds.
    #[instrument(level = "debug", skip_all)]
    pub async fn inclusion_proof(
        &self,
        group_id: usize,
        commitment: &Hash,
    ) -> Result<InclusionProofResponse, ServerError> {
        if U256::from(group_id) != self.identity_manager.group_id() {
            return Err(ServerError::InvalidGroupId);
        }

        if commitment == &self.identity_manager.initial_leaf_value() {
            return Err(ServerError::InvalidCommitment);
        }

        {
            let tree = self.tree_state.read().await.map_err(|e| {
                error!(?e, "Failed to obtain tree lock in inclusion_proof.");
                panic!("Sequencer potentially deadlocked, terminating.");
                #[allow(unreachable_code)]
                e
            })?;

            if let Some(identity_index) = tree
                .merkle_tree
                .leaves()
                .iter()
                .position(|&x| x == *commitment)
            {
                let proof = tree
                    .merkle_tree
                    .proof(identity_index)
                    .ok_or(ServerError::IndexOutOfBounds)?;
                let root = tree.merkle_tree.root();

                // Locally check the proof
                // TODO: Check the leaf index / path
                if !tree.merkle_tree.verify(*commitment, &proof) {
                    error!(
                        ?commitment,
                        ?identity_index,
                        ?root,
                        "Proof does not verify locally."
                    );
                    panic!("Proof does not verify locally.");
                }

                drop(tree);

                // Verify the root on chain
                if let Err(error) = self.identity_manager.assert_valid_root(root).await {
                    error!(
                        computed_root = ?root,
                        ?error,
                        "Root mismatch between tree and contract."
                    );
                    return Err(ServerError::RootMismatch);
                }
                return Ok(InclusionProofResponse::Proof { root, proof });
            }
        }

        if self
            .database
            .pending_identity_exists(group_id, commitment)
            .await?
        {
            Ok(InclusionProofResponse::Pending)
        } else {
            Err(ServerError::IdentityCommitmentNotFound)
        }
    }

    /// # Errors
    ///
    /// Will return an Error if any of the components cannot be shut down
    /// gracefully.
    pub async fn shutdown(&self) -> AnyhowResult<()> {
        info!("Shutting down identity committer and chain subscriber.");
        self.chain_subscriber.shutdown().await;
        self.identity_committer.shutdown().await
    }
}
