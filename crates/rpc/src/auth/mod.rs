//! Authorization token parsing and verification.

pub mod eip712;
mod token;

pub use crate::error::AuthError;
pub use token::{
    AuthContext, AuthorizationToken, DEFAULT_MAX_AUTH_TOKEN_VALIDITY,
    DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS, TOKEN_FIELDS_LEN, X_AUTHORIZATION_TOKEN,
    build_eip712_token_fields, build_token_fields, parse_auth_header,
};
