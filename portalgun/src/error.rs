// SPDX-FileCopyrightText: 2023 perillamint <perillamint@silicon.moe>
// SPDX-FileCopyrightText: 2020-2022 Alex Grinman <me@alexgr.in>
//
// SPDX-License-Identifier: MIT

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to connect to control server: {0}.")]
    WebSocketError(#[from] tokio_tungstenite::tungstenite::error::Error),

    #[error("Server denied the connection.")]
    AuthenticationFailed,

    #[error("Server sent a malformed message.")]
    MalformedMessageFromServer,

    #[error("Invalid sub-domain specified.")]
    InvalidSubDomain,

    #[error("Cannot use this sub-domain, it is already taken.")]
    SubDomainInUse,

    #[error("{0}")]
    ServerError(String),

    #[error("The server responded with an invalid response.")]
    ServerReplyInvalid,

    #[error("The server did not respond to our client_hello.")]
    NoResponseFromServer,

    #[error("The server timed out sending us something.")]
    Timeout,

    #[error("Reqwest error: {0}")]
    ReqwestError(#[from] reqwest::Error),

    #[error("OAuth2 Authentication error: {0}")]
    OAuth2(String),
}
