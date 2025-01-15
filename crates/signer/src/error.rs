use alloy::hex;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SignerModuleError {
    #[error("unauthorized")]
    Unauthorized,

    #[error("unknown consensus signer: 0x{}", hex::encode(.0))]
    UnknownConsensusSigner(Vec<u8>),

    #[error("unknown proxy signer: 0x{}", hex::encode(.0))]
    UnknownProxySigner(Vec<u8>),

    #[error("Dirk communication error: {0}")]
    DirkCommunicationError(String),

    #[error("Dirk signer does not support this operation")]
    DirkNotSupported,

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for SignerModuleError {
    fn into_response(self) -> Response {
        match self {
            SignerModuleError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            SignerModuleError::UnknownConsensusSigner(_) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            SignerModuleError::UnknownProxySigner(_) => (StatusCode::NOT_FOUND, self.to_string()),
            SignerModuleError::DirkCommunicationError(_) => {
                (StatusCode::BAD_GATEWAY, "Dirk communication error".to_string())
            }
            SignerModuleError::DirkNotSupported => (StatusCode::BAD_REQUEST, self.to_string()),
            SignerModuleError::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        }
        .into_response()
    }
}
