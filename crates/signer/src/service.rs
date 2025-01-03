use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json,
};
use axum_extra::TypedHeader;
use bimap::BiHashMap;
use cb_common::{
    commit::{
        constants::{
            GENERATE_PROXY_KEY_PATH, GET_PUBKEYS_PATH, REQUEST_SIGNATURE_PATH, STATUS_PATH,
        },
        request::{
            ConsensusProxyMap, EncryptionScheme, GenerateProxyRequest, GetPubkeysResponse,
            SignConsensusRequest, SignProxyRequest, SignRequest,
        },
    },
    config::StartSignerConfig,
    constants::COMMIT_BOOST_VERSION,
    types::{Jwt, ModuleId},
};
use cb_metrics::provider::MetricsProvider;
use eyre::{Context, Result};
use headers::{authorization::Bearer, Authorization};
use tokio::{net::TcpListener, sync::RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    dirk::DirkClient,
    error::SignerModuleError,
    manager::LocalSigningManager,
    metrics::{uri_to_tag, SIGNER_METRICS_REGISTRY, SIGNER_STATUS},
};

/// Implements the Signer API and provides a service for signing requests
pub struct SigningService;

#[derive(Clone)]
pub enum SigningManager {
    Local(Arc<RwLock<LocalSigningManager>>),
    Dirk(DirkClient),
}

impl SigningManager {
    /// Amount of consensus signers available
    pub async fn available_consensus_signers(&self) -> eyre::Result<usize> {
        match self {
            SigningManager::Local(manager) => Ok(manager.read().await.consensus_pubkeys().len()),
            SigningManager::Dirk(dirk) => Ok(dirk.get_pubkeys().await?.len()),
        }
    }

    /// Amount of proxy signers available
    pub async fn available_proxy_signers(&self) -> eyre::Result<usize> {
        match self {
            SigningManager::Local(manager) => {
                let proxies = manager.read().await.proxies().clone();
                Ok(proxies.bls_signers.len() + proxies.ecdsa_signers.len())
            }
            SigningManager::Dirk(dirk) => Ok(dirk.get_proxy_pubkeys().await?.len()),
        }
    }

    pub async fn get_consensus_proxy_maps(
        &self,
        module_id: &ModuleId,
    ) -> eyre::Result<Vec<ConsensusProxyMap>> {
        match self {
            SigningManager::Local(local_manager) => {
                local_manager.read().await.get_consensus_proxy_maps(module_id)
            }
            SigningManager::Dirk(dirk_manager) => {
                dirk_manager.get_consensus_proxy_maps(module_id).await
            }
        }
    }
}

#[derive(Clone)]
struct SigningState {
    /// Manager handling different signing methods
    manager: SigningManager,
    /// Map of JWTs to module ids. This also acts as registry of all modules
    /// running
    jwts: Arc<BiHashMap<ModuleId, Jwt>>,
}

impl SigningService {
    pub async fn run(config: StartSignerConfig) -> eyre::Result<()> {
        if config.jwts.is_empty() {
            warn!("Signing service was started but no module is registered. Exiting");
            return Ok(());
        }

        let module_ids: Vec<String> = config.jwts.left_values().cloned().map(Into::into).collect();

        let state = match &config.dirk {
            Some(dirk) => SigningState {
                manager: SigningManager::Dirk(
                    DirkClient::new_from_config(config.chain, dirk.clone()).await?,
                ),
                jwts: config.jwts.into(),
            },
            None => {
                let proxy_store = if let Some(store) = config.store {
                    Some(store.init_from_env()?)
                } else {
                    warn!("Proxy store not configured. Proxies keys and delegations will not be persisted");
                    None
                };

                let mut local_manager = LocalSigningManager::new(config.chain, proxy_store)?;

                if let Some(loader) = config.loader {
                    for signer in loader.load_keys()? {
                        local_manager.add_consensus_signer(signer);
                    }
                }

                SigningState {
                    manager: SigningManager::Local(Arc::new(RwLock::new(local_manager))),
                    jwts: config.jwts.into(),
                }
            }
        };

        let loaded_consensus = state.manager.available_consensus_signers().await?;
        let loaded_proxies = state.manager.available_proxy_signers().await?;

        info!(version = COMMIT_BOOST_VERSION, modules =? module_ids, port =? config.server_port, loaded_consensus, loaded_proxies, "Starting signing service");

        SigningService::init_metrics()?;

        let app = axum::Router::new()
            .route(REQUEST_SIGNATURE_PATH, post(handle_request_signature))
            .route(GET_PUBKEYS_PATH, get(handle_get_pubkeys))
            .route(GENERATE_PROXY_KEY_PATH, post(handle_generate_proxy))
            .with_state(state.clone())
            .route_layer(middleware::from_fn_with_state(state.clone(), jwt_auth))
            .route_layer(middleware::from_fn(log_request));
        let status_router = axum::Router::new().route(STATUS_PATH, get(handle_status));

        let address = SocketAddr::from(([0, 0, 0, 0], config.server_port));
        let listener = TcpListener::bind(address).await?;

        axum::serve(listener, axum::Router::new().merge(app).merge(status_router))
            .await
            .wrap_err("signer server exited")
    }

    fn init_metrics() -> Result<()> {
        MetricsProvider::load_and_run(SIGNER_METRICS_REGISTRY.clone())
    }
}

/// Authentication middleware layer
async fn jwt_auth(
    State(state): State<SigningState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    mut req: Request,
    next: Next,
) -> Result<Response, SignerModuleError> {
    let jwt: Jwt = auth.token().to_string().into();

    let module_id = state.jwts.get_by_right(&jwt).ok_or_else(|| {
        error!("Unauthorized request. Was the module started correctly?");
        SignerModuleError::Unauthorized
    })?;

    req.extensions_mut().insert(module_id.clone());

    Ok(next.run(req).await)
}

/// Requests logging middleware layer
async fn log_request(req: Request, next: Next) -> Result<Response, SignerModuleError> {
    let url = &req.uri().clone();
    let response = next.run(req).await;
    SIGNER_STATUS.with_label_values(&[response.status().as_str(), uri_to_tag(url)]).inc();
    Ok(response)
}

/// Status endpoint for the Signer API
async fn handle_status() -> Result<impl IntoResponse, SignerModuleError> {
    Ok((StatusCode::OK, "OK"))
}

/// Implements get_pubkeys from the Signer API
async fn handle_get_pubkeys(
    Extension(module_id): Extension<ModuleId>,
    State(state): State<SigningState>,
) -> Result<impl IntoResponse, SignerModuleError> {
    let req_id = Uuid::new_v4();

    debug!(event = "get_pubkeys", ?req_id, "New request");

    let keys = state
        .manager
        .get_consensus_proxy_maps(&module_id)
        .await
        .map_err(|err| SignerModuleError::Internal(err.to_string()))?;

    let res = GetPubkeysResponse { keys };

    Ok((StatusCode::OK, Json(res)).into_response())
}

/// Implements request_signature from the Signer API
async fn handle_request_signature(
    Extension(module_id): Extension<ModuleId>,
    State(state): State<SigningState>,
    Json(request): Json<SignRequest>,
) -> Result<impl IntoResponse, SignerModuleError> {
    let req_id = Uuid::new_v4();

    debug!(event = "request_signature", ?module_id, ?req_id, "New request");

    let response = match state.manager {
        SigningManager::Local(local_manager) => match request {
            SignRequest::Consensus(SignConsensusRequest { object_root, pubkey }) => local_manager
                .read()
                .await
                .sign_consensus(&pubkey, &object_root)
                .await
                .map(|sig| Json(sig).into_response())
                .map_err(|err| SignerModuleError::Internal(err.to_string())),
            SignRequest::ProxyBls(SignProxyRequest { object_root, pubkey: bls_key }) => {
                local_manager
                    .read()
                    .await
                    .sign_proxy_bls(&bls_key, &object_root)
                    .await
                    .map(|sig| Json(sig).into_response())
                    .map_err(|err| SignerModuleError::Internal(err.to_string()))
            }
            SignRequest::ProxyEcdsa(SignProxyRequest { object_root, pubkey: ecdsa_key }) => {
                local_manager
                    .read()
                    .await
                    .sign_proxy_ecdsa(&ecdsa_key, &object_root)
                    .await
                    .map(|sig| Json(sig).into_response())
                    .map_err(|err| SignerModuleError::Internal(err.to_string()))
            }
        },
        SigningManager::Dirk(dirk_manager) => match request {
            SignRequest::Consensus(SignConsensusRequest { object_root, pubkey }) => dirk_manager
                .request_signature(pubkey, object_root)
                .await
                .map(|sig| Json(sig).into_response())
                .map_err(|err| SignerModuleError::Internal(err.to_string())),
            SignRequest::ProxyBls(SignProxyRequest { object_root, pubkey: bls_key }) => {
                dirk_manager
                    .request_signature(bls_key, object_root)
                    .await
                    .map(|sig| Json(sig).into_response())
                    .map_err(|err| SignerModuleError::Internal(err.to_string()))
            }
            SignRequest::ProxyEcdsa(_) => {
                error!("ECDSA proxy sign request not supported with Dirk");
                Err(SignerModuleError::DirkNotSupported)
            }
        },
    };

    response
}

async fn handle_generate_proxy(
    Extension(module_id): Extension<ModuleId>,
    State(state): State<SigningState>,
    Json(request): Json<GenerateProxyRequest>,
) -> Result<impl IntoResponse, SignerModuleError> {
    let req_id = Uuid::new_v4();

    debug!(event = "generate_proxy", module_id=?module_id, ?req_id, "New request");

    let response = match state.manager {
        SigningManager::Local(local_manager) => match request.scheme {
            EncryptionScheme::Bls => local_manager
                .write()
                .await
                .create_proxy_bls(module_id, request.consensus_pubkey)
                .await
                .map(|proxy_delegation| Json(proxy_delegation).into_response())
                .map_err(|err| SignerModuleError::Internal(err.to_string())),
            EncryptionScheme::Ecdsa => local_manager
                .write()
                .await
                .create_proxy_ecdsa(module_id, request.consensus_pubkey)
                .await
                .map(|proxy_delegation| Json(proxy_delegation).into_response())
                .map_err(|err| SignerModuleError::Internal(err.to_string())),
        },
        SigningManager::Dirk(dirk_manager) => match request.scheme {
            EncryptionScheme::Bls => dirk_manager
                .generate_proxy_key(module_id, request.consensus_pubkey)
                .await
                .map(|proxy_delegation| Json(proxy_delegation).into_response())
                .map_err(|err| SignerModuleError::Internal(err.to_string())),
            EncryptionScheme::Ecdsa => {
                error!("ECDSA proxy generation not supported with Dirk");
                Err(SignerModuleError::DirkNotSupported)
            }
        },
    };

    response
}
