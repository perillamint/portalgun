// SPDX-FileCopyrightText: 2023 perillamint <perillamint@silicon.moe>
// SPDX-FileCopyrightText: 2020-2022 Alex Grinman <me@alexgr.in>
//
// SPDX-License-Identifier: MIT

use crate::auth::{SigKey, Signature};
use base64::Engine;
use chrono::{DateTime, Utc};
use portalgun_lib::{ClientId, ReconnectToken};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid base64: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("invalid reconnect token (signature)")]
    InvalidSignature,

    #[error("reconnect token expired")]
    Expired,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReconnectTokenPayload {
    pub sub_domain: String,
    pub client_id: ClientId,
    pub expires: DateTime<Utc>,
}
impl ReconnectTokenPayload {
    pub fn into_token(self, key: &SigKey) -> Result<ReconnectToken, Error> {
        let payload = serde_json::to_string(&self)?;
        let sig = key.sign(payload.as_bytes());
        let tok = ReconnectTokenInner { payload, sig };
        let tok = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&tok)?);
        Ok(ReconnectToken(tok))
    }

    pub fn verify(tok: ReconnectToken, key: &SigKey) -> Result<ReconnectTokenPayload, Error> {
        let tok = base64::engine::general_purpose::STANDARD.decode(tok.0.as_str())?;
        let tok: ReconnectTokenInner = serde_json::from_slice(&tok)?;

        if !key.verify(tok.payload.as_bytes(), &tok.sig) {
            return Err(Error::InvalidSignature);
        }

        let payload: ReconnectTokenPayload = serde_json::from_str(&tok.payload)?;

        if Utc::now() > payload.expires {
            return Err(Error::Expired);
        }

        Ok(payload)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ReconnectTokenInner {
    payload: String,
    sig: Signature,
}
