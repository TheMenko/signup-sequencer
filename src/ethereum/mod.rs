mod contract;

use self::contract::{LeafInsertionFilter, Semaphore};
use crate::{app::JsonCommitment, hash::Hash, mimc_tree::MimcTree};
use ethers::{
    core::k256::ecdsa::SigningKey,
    prelude::{
        builders::Event, Address, Http, LocalWallet, Middleware, Provider, Signer,
        SignerMiddleware, Wallet, H160,
    },
};
use eyre::{eyre, Result as EyreResult};
use hex_literal::hex;
use serde_json::Error as SerdeError;
use std::{fs::File, path::Path, sync::Arc};
use structopt::StructOpt;
use tracing::info;
use url::Url;

const SEMAPHORE_ADDRESS: Address = H160(hex!("266FB396B626621898C87a92efFBA109dE4685F6"));
const SIGNING_KEY: [u8; 32] =
    hex!("ee79b5f6e221356af78cf4c36f4f7885a11b67dfcc81c34d80249947330c0f82");

pub type ContractSigner = SignerMiddleware<Provider<Http>, Wallet<SigningKey>>;
pub type SemaphoreContract = contract::Semaphore<ContractSigner>;

#[derive(Debug, PartialEq, StructOpt)]
pub struct Options {
    /// Ethereum API Provider
    #[structopt(long, env, default_value = "http://localhost:8545")]
    pub ethereum_provider: Url,

    /// Semaphore contract address.
    #[structopt(long, env, default_value = "266FB396B626621898C87a92efFBA109dE4685F6")]
    pub semaphore_address: Address,

    /// Private key used for transaction signing
    #[structopt(
        long,
        env,
        default_value = "ee79b5f6e221356af78cf4c36f4f7885a11b67dfcc81c34d80249947330c0f82"
    )]
    // NOTE: We abuse `Hash` here because it has the right `FromStr` implementation.
    pub signing_key: Hash,
}

pub struct Ethereum {
    provider:  Provider<Http>,
    wallet:    Wallet<SigningKey>,
    semaphore: Semaphore<ContractSigner>,
}

impl Ethereum {
    pub async fn new(options: Options) -> EyreResult<Self> {
        // Connect to the Ethereum provider
        // TODO: Support WebSocket and Https
        info!(
            provider = %&options.ethereum_provider,
            "Connecting to Ethereum"
        );
        let http = Http::new(options.ethereum_provider);
        let provider = Provider::new(http);
        let chain_id = provider.get_chainid().await?;
        let latest_block = provider.get_block_number().await?;
        info!(%chain_id, %latest_block, "Connected to Ethereum");

        // Construct wallet
        let chain_id: u64 = chain_id.try_into().map_err(|e| eyre!("{}", e))?;
        let signing_key = SigningKey::from_bytes(options.signing_key.as_bytes_be())?;
        let wallet = LocalWallet::from(signing_key).with_chain_id(chain_id);
        let address = wallet.address();
        info!(?address, "Constructed wallet");

        // Construct middleware stack
        // TODO: See <https://docs.rs/ethers-middleware/0.5.4/ethers_middleware/index.html> for useful middlewares.
        let client = SignerMiddleware::new(provider.clone(), wallet.clone());

        // Connect to Contract
        let client = Arc::new(client);
        let semaphore = Semaphore::new(options.semaphore_address, client);

        Ok(Self {
            provider,
            wallet,
            semaphore,
        })
    }
}

pub async fn initialize_semaphore() -> Result<(ContractSigner, SemaphoreContract), eyre::Error> {
    let provider = Provider::<Http>::try_from("http://localhost:8545")
        .expect("could not instantiate HTTP Provider");
    let chain_id: u64 = provider
        .get_chainid()
        .await?
        .try_into()
        .map_err(|e| eyre!("{}", e))?;

    let wallet = LocalWallet::from(SigningKey::from_bytes(&SIGNING_KEY)?).with_chain_id(chain_id);
    let signer = SignerMiddleware::new(provider, wallet);
    let contract = Semaphore::new(SEMAPHORE_ADDRESS, Arc::new(signer.clone()));

    Ok((signer, contract))
}

pub async fn parse_identity_commitments(
    json_file_path: &Path,
    tree: &mut MimcTree,
    semaphore_contract: SemaphoreContract,
) -> EyreResult<usize> {
    let mut last_index = 0;
    let starting_block = match File::open(json_file_path) {
        Ok(file) => {
            let json_commitments: Result<JsonCommitment, SerdeError> =
                serde_json::from_reader(file);
            match json_commitments {
                Ok(json_commitments) => {
                    for &commitment in &json_commitments.commitments {
                        tree.set(last_index, commitment);
                        last_index += 1;
                    }
                    json_commitments.last_block
                }
                Err(_) => 0,
            }
        }
        Err(_) => 0,
    };

    let filter: Event<SignerMiddleware<Provider<Http>, Wallet<SigningKey>>, LeafInsertionFilter> =
        semaphore_contract
            .leaf_insertion_filter()
            .from_block(starting_block);
    let logs = filter.query().await?;
    for event in &logs {
        let index: usize = event.leaf_index.as_u32().try_into()?;
        tree.set(index, event.leaf.into());
        last_index = index;
    }
    Ok(last_index)
}
