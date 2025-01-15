use std::{fs, path::PathBuf};

use alloy::{hex, primitives::FixedBytes};
use cb_common::{
    commit::request::{ConsensusProxyMap, ProxyDelegation, SignedProxyDelegation},
    config::DirkConfig,
    constants::COMMIT_BOOST_DOMAIN,
    signature::compute_domain,
    signer::{BlsPublicKey, BlsSignature, ProxyStore},
    types::{Chain, ModuleId},
};
use rand::Rng;
use tonic::transport::{Channel, ClientTlsConfig};
use tracing::{info, trace};
use tree_hash::TreeHash;

use crate::{
    error::SignerModuleError::{self, DirkCommunicationError},
    proto::v1::{
        account_manager_client::AccountManagerClient, lister_client::ListerClient,
        sign_request::Id as SignerId, signer_client::SignerClient, Account as DirkAccount,
        GenerateRequest, ListAccountsRequest, ResponseState, SignRequest, UnlockAccountRequest,
    },
};

#[derive(Clone, Debug)]
struct Account {
    wallet: String,
    name: String,
    public_key: Option<BlsPublicKey>,
}

impl Account {
    pub fn complete_name(&self) -> String {
        format!("{}/{}", self.wallet, self.name)
    }
}

#[derive(Clone, Debug)]
pub struct DirkManager {
    chain: Chain,
    channel: Channel,
    accounts: Vec<Account>,
    unlock: bool,
    secrets_path: PathBuf,
    proxy_store: Option<ProxyStore>,
}

impl DirkManager {
    pub async fn new_from_config(chain: Chain, config: DirkConfig) -> eyre::Result<Self> {
        let mut tls_config = ClientTlsConfig::new().identity(config.client_cert);

        if let Some(ca) = config.cert_auth {
            tls_config = tls_config.ca_certificate(ca);
        }

        if let Some(server_domain) = config.server_domain {
            tls_config = tls_config.domain_name(server_domain);
        }

        trace!(url=%config.url, "Stablishing connection with Dirk");

        let channel = Channel::from_shared(config.url.to_string())
            .map_err(|_| eyre::eyre!("Invalid Dirk URL"))?
            .tls_config(tls_config)
            .map_err(|_| eyre::eyre!("Invalid Dirk TLS config"))?
            .connect()
            .await
            .map_err(|e| eyre::eyre!("Couldn't connect to Dirk: {e}"))?;

        let dirk_accounts = get_accounts_in_wallets(
            channel.clone(),
            config
                .accounts
                .iter()
                .filter_map(|account| Some(account.split_once("/")?.0.to_string()))
                .collect(),
        )
        .await?;

        let mut accounts = Vec::with_capacity(config.accounts.len());
        for account in config.accounts {
            let (wallet, name) = account.split_once("/").ok_or(eyre::eyre!(
                "Invalid account name: {account}. It must be in format wallet/account"
            ))?;
            let public_key = dirk_accounts.iter().find_map(|a| {
                if a.name == account {
                    BlsPublicKey::try_from(a.public_key.as_slice()).ok()
                } else {
                    None
                }
            });

            accounts.push(Account {
                wallet: wallet.to_string(),
                name: name.to_string(),
                public_key,
            });
        }
        let wallets =
            accounts.iter().map(|account| account.wallet.clone()).collect::<Vec<String>>();
        let dirk_accounts = get_accounts_in_wallets(channel.clone(), wallets).await?;
        for account in accounts.iter_mut() {
            if let Some(dirk_account) =
                dirk_accounts.iter().find(|a| a.name == account.complete_name())
            {
                account.public_key =
                    Some(BlsPublicKey::try_from(dirk_account.public_key.as_slice())?);
            }
        }

        Ok(Self {
            chain,
            channel,
            accounts,
            unlock: config.unlock,
            secrets_path: config.secrets_path,
            proxy_store: None,
        })
    }

    pub fn with_proxy_store(self, proxy_store: ProxyStore) -> eyre::Result<Self> {
        Ok(Self { proxy_store: Some(proxy_store), ..self })
    }

    /// Get all available accounts in the `self.accounts` wallets
    async fn get_all_accounts(&self) -> Result<Vec<DirkAccount>, SignerModuleError> {
        get_accounts_in_wallets(
            self.channel.clone(),
            self.accounts.iter().map(|account| account.wallet.clone()).collect::<Vec<String>>(),
        )
        .await
    }

    /// Get the complete account name (`wallet/account`) for a public key.
    /// Returns `Ok(None)` if the account was not found.
    /// Returns `Err` if there was a communication error with Dirk.
    async fn get_pubkey_account(
        &self,
        pubkey: BlsPublicKey,
    ) -> Result<Option<String>, SignerModuleError> {
        match self
            .accounts
            .iter()
            .find(|account| account.public_key.is_some_and(|account_pk| account_pk == pubkey))
        {
            Some(account) => Ok(Some(account.complete_name())),
            None => {
                let accounts = self.get_all_accounts().await?;

                for account in accounts {
                    if account.public_key == pubkey.to_vec() {
                        return Ok(Some(account.name));
                    }
                }

                Ok(None)
            }
        }
    }

    /// Returns the public keys of the config-registered accounts
    pub async fn consensus_pubkeys(&self) -> eyre::Result<Vec<BlsPublicKey>> {
        let registered_pubkeys = self
            .accounts
            .iter()
            .filter_map(|account| account.public_key)
            .collect::<Vec<BlsPublicKey>>();

        if registered_pubkeys.len() == self.accounts.len() {
            Ok(registered_pubkeys)
        } else {
            let accounts = self.get_all_accounts().await?;

            let expected_accounts: Vec<String> =
                self.accounts.iter().map(|account| account.complete_name()).collect();

            Ok(accounts
                .iter()
                .filter_map(|account| {
                    if expected_accounts.contains(&account.name) {
                        BlsPublicKey::try_from(account.public_key.as_slice()).ok()
                    } else {
                        None
                    }
                })
                .collect())
        }
    }

    /// Returns the public keys of all the proxy accounts found in Dirk.
    /// An account is considered a proxy if its name has the format
    /// `consensus_account/module_id/uuid`, where `consensus_account` is the
    /// name of a config-registered account.
    pub async fn proxies(&self) -> eyre::Result<Vec<BlsPublicKey>> {
        let accounts = self.get_all_accounts().await?;

        Ok(accounts
            .iter()
            .filter_map(|account| {
                if self.accounts.iter().any(|consensus_account| {
                    account.name.starts_with(&format!("{}/", consensus_account.complete_name()))
                }) {
                    BlsPublicKey::try_from(account.public_key.as_slice()).ok()
                } else {
                    None
                }
            })
            .collect())
    }

    /// Returns a mapping of the proxy accounts' pubkeys by consensus account,
    /// for a given module.
    /// An account is considered a proxy if its name has the format
    /// `consensus_account/module_id/uuid`, where `consensus_account` is the
    /// name of a config-registered account.
    pub async fn get_consensus_proxy_maps(
        &self,
        module_id: &ModuleId,
    ) -> Result<Vec<ConsensusProxyMap>, SignerModuleError> {
        let accounts = self.get_all_accounts().await?;

        let mut proxy_maps = Vec::new();

        for consensus_account in self.accounts.iter() {
            let Some(consensus_key) = consensus_account.public_key else {
                continue;
            };

            let proxy_keys = accounts
                .iter()
                .filter_map(|account| {
                    if account
                        .name
                        .starts_with(&format!("{}/{module_id}/", consensus_account.complete_name()))
                    {
                        BlsPublicKey::try_from(account.public_key.as_slice()).ok()
                    } else {
                        None
                    }
                })
                .collect::<Vec<BlsPublicKey>>();
            proxy_maps.push(ConsensusProxyMap {
                consensus: consensus_key,
                proxy_bls: proxy_keys,
                proxy_ecdsa: vec![],
            });
        }

        Ok(proxy_maps)
    }

    /// Generate a random password of 64 hex-characters
    fn random_password() -> String {
        let password_bytes: [u8; 32] = rand::thread_rng().gen();
        hex::encode(password_bytes)
    }

    /// Read the password for an account from a file
    fn read_password(&self, account: String) -> Result<String, SignerModuleError> {
        fs::read_to_string(self.secrets_path.join(account.clone())).map_err(|err| {
            SignerModuleError::Internal(format!(
                "error reading password for account '{account}': {err}"
            ))
        })
    }

    /// Store the password for an account in a file
    fn store_password(&self, account: String, password: String) -> Result<(), SignerModuleError> {
        let account_dir = self
            .secrets_path
            .join(
                account
                    .rsplit_once("/")
                    .ok_or(SignerModuleError::Internal(format!(
                        "account name '{account}' is invalid"
                    )))?
                    .0,
            )
            .to_string_lossy()
            .to_string();

        fs::create_dir_all(account_dir.clone()).map_err(|err| {
            SignerModuleError::Internal(format!("error creating dir '{account_dir}': {err}"))
        })?;
        fs::write(self.secrets_path.join(account.clone()), password).map_err(|err| {
            SignerModuleError::Internal(format!(
                "error writing password for account '{account}': {err}"
            ))
        })
    }

    async fn unlock_account(
        &self,
        account: String,
        password: String,
    ) -> Result<(), SignerModuleError> {
        trace!(account, "Sending AccountManager/Unlock request to Dirk");

        let mut client = AccountManagerClient::new(self.channel.clone());
        let unlock_request = tonic::Request::new(UnlockAccountRequest {
            account: account.clone(),
            passphrase: password.as_bytes().to_vec(),
        });

        let unlock_response = client.unlock(unlock_request).await.map_err(|err| {
            DirkCommunicationError(format!("error unlocking account '{account}': {err}"))
        })?;
        if unlock_response.get_ref().state() != ResponseState::Succeeded {
            return Err(DirkCommunicationError(format!(
                "unlock request for '{account}' returned error"
            )));
        }

        Ok(())
    }

    pub async fn generate_proxy_key(
        &self,
        module_id: ModuleId,
        consensus_pubkey: BlsPublicKey,
    ) -> Result<SignedProxyDelegation<BlsPublicKey>, SignerModuleError> {
        let uuid = uuid::Uuid::new_v4();

        let consensus_account = self
            .get_pubkey_account(consensus_pubkey)
            .await?
            .ok_or(SignerModuleError::UnknownConsensusSigner(consensus_pubkey.to_vec()))?;

        if !self
            .accounts
            .iter()
            .map(|account| account.complete_name())
            .collect::<Vec<String>>()
            .contains(&consensus_account)
        {
            return Err(SignerModuleError::UnknownConsensusSigner(consensus_pubkey.to_vec()))?;
        }

        let account_name = format!("{consensus_account}/{module_id}/{uuid}");
        let new_password = Self::random_password();

        trace!(account = account_name, "Sending AccountManager/Generate request to Dirk");

        let mut client = AccountManagerClient::new(self.channel.clone());
        let generate_request = tonic::Request::new(GenerateRequest {
            account: account_name.clone(),
            passphrase: new_password.as_bytes().to_vec(),
            participants: 1,
            signing_threshold: 1,
        });

        let generate_response = client
            .generate(generate_request)
            .await
            .map_err(|err| DirkCommunicationError(format!("error on generate request: {err}")))?;

        if generate_response.get_ref().state() != ResponseState::Succeeded {
            return Err(DirkCommunicationError("generate request returned error".to_string()));
        }

        self.store_password(account_name.clone(), new_password.clone())?;

        let proxy_key =
            BlsPublicKey::try_from(generate_response.into_inner().public_key.as_slice()).map_err(
                |_| DirkCommunicationError("return value is not a valid public key".to_string()),
            )?;

        self.unlock_account(account_name, new_password).await?;

        let message = ProxyDelegation { delegator: consensus_pubkey, proxy: proxy_key };
        let signature =
            self.request_signature(consensus_pubkey, message.tree_hash_root().0).await?;
        let delegation = SignedProxyDelegation { message, signature };

        if let Some(store) = &self.proxy_store {
            store.store_proxy_bls_delegation(&module_id, &delegation).map_err(|err| {
                SignerModuleError::Internal(format!("error storing delegation signature: {err}"))
            })?;
        }

        Ok(delegation)
    }

    pub async fn request_signature(
        &self,
        pubkey: BlsPublicKey,
        object_root: [u8; 32],
    ) -> Result<BlsSignature, SignerModuleError> {
        let domain = compute_domain(self.chain, COMMIT_BOOST_DOMAIN);

        trace!(
            %pubkey,
            object_root = hex::encode(object_root),
            domain = hex::encode(domain),
            "Sending Signer/Sign request to Dirk"
        );

        let mut signer_client = SignerClient::new(self.channel.clone());
        let sign_request = tonic::Request::new(SignRequest {
            id: Some(SignerId::PublicKey(pubkey.to_vec())),
            domain: domain.to_vec(),
            data: object_root.to_vec(),
        });

        let sign_response = signer_client
            .sign(sign_request)
            .await
            .map_err(|err| DirkCommunicationError(format!("error on sign request: {err}")))?;

        // Retry if unlock config is set
        let sign_response = match sign_response.get_ref().state() {
            ResponseState::Denied if self.unlock => {
                info!("Failed to sign message, account {pubkey:#} may be locked. Unlocking and retrying.");

                let account_name = self
                    .get_pubkey_account(pubkey)
                    .await?
                    .ok_or(SignerModuleError::UnknownConsensusSigner(pubkey.to_vec()))?;
                self.unlock_account(
                    account_name.clone(),
                    self.read_password(account_name.clone())?,
                )
                .await?;

                trace!(
                    %pubkey,
                    object_root = hex::encode(object_root),
                    domain = hex::encode(domain),
                    "Sending Signer/Sign request to Dirk"
                );

                let sign_request = tonic::Request::new(SignRequest {
                    id: Some(SignerId::PublicKey(pubkey.to_vec())),
                    domain: domain.to_vec(),
                    data: object_root.to_vec(),
                });
                signer_client.sign(sign_request).await.map_err(|err| {
                    DirkCommunicationError(format!("error on sign request: {err}"))
                })?
            }
            _ => sign_response,
        };

        if sign_response.get_ref().state() != ResponseState::Succeeded {
            return Err(DirkCommunicationError("sign request returned error".to_string()));
        }

        Ok(BlsSignature::from(
            FixedBytes::try_from(sign_response.into_inner().signature.as_slice()).map_err(
                |_| DirkCommunicationError("return value is not a valid signature".to_string()),
            )?,
        ))
    }
}

/// Get the accounts for the wallets passed as argument
async fn get_accounts_in_wallets(
    channel: Channel,
    wallets: Vec<String>,
) -> Result<Vec<DirkAccount>, SignerModuleError> {
    trace!(?wallets, "Sending Lister/ListAccounts request to Dirk");

    let mut client = ListerClient::new(channel);
    let pubkeys_request = tonic::Request::new(ListAccountsRequest { paths: wallets });
    let pubkeys_response = client
        .list_accounts(pubkeys_request)
        .await
        .map_err(|err| DirkCommunicationError(format!("error listing accounts: {err}")))?;

    if pubkeys_response.get_ref().state() != ResponseState::Succeeded {
        return Err(DirkCommunicationError("list accounts request returned error".to_string()));
    }

    Ok(pubkeys_response.into_inner().accounts)
}
