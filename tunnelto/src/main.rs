use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::{SinkExt, StreamExt};

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use human_panic::setup_panic;
pub use log::{debug, error, info, warn};

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, RwLock};

mod config;
mod error;
mod introspect;
mod local;
mod spinner;
pub use self::error::*;

pub use config::*;
pub use tunnelto_lib::*;

use crate::introspect::IntrospectionAddrs;
use colored::Colorize;
use futures::future::Either;
use std::time::Duration;
use tokio::sync::Mutex;

pub type ActiveStreams = Arc<RwLock<HashMap<StreamId, UnboundedSender<StreamMessage>>>>;

lazy_static::lazy_static! {
    pub static ref ACTIVE_STREAMS:ActiveStreams = Arc::new(RwLock::new(HashMap::new()));
    pub static ref RECONNECT_TOKEN: Arc<Mutex<Option<ReconnectToken>>> = Arc::new(Mutex::new(None));
}

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Data(Vec<u8>),
    Close,
}

#[tokio::main]
async fn main() {
    setup_panic!();

    let mut config = match Config::get() {
        Ok(config) => config,
        Err(_) => return,
    };

    let introspect_addrs = introspect::start_introspection_server(config.clone());

    loop {
        let (restart_tx, mut restart_rx) = unbounded();
        let wormhole = run_wormhole(config.clone(), introspect_addrs.clone(), restart_tx);
        let result = futures::future::select(Box::pin(wormhole), restart_rx.next()).await;
        config.first_run = false;

        match result {
            Either::Left((Err(e), _)) => match e {
                Error::WebSocketError(_) | Error::NoResponseFromServer | Error::Timeout => {
                    error!("Control error: {:?}. Retrying in 5 seconds.", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                _ => {
                    eprintln!("Error: {}", format!("{}", e).red());
                    return;
                }
            },
            Either::Right((Some(e), _)) => {
                warn!("restarting in 3 seconds...from error: {:?}", e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            _ => {}
        };

        info!("restarting wormhole");
    }
}

/// Setup the tunnel to our control server
async fn run_wormhole(
    config: Config,
    introspect: IntrospectionAddrs,
    mut restart_tx: UnboundedSender<Option<Error>>,
) -> Result<(), Error> {
    let websocket = connect_to_wormhole(&config).await?;

    if config.first_run {
        eprintln!(
            "Local Inspect Dashboard: {}{}",
            "http://localhost:".yellow(),
            introspect.web_explorer_address.port()
        );
    }

    // split reading and writing
    let (mut ws_sink, mut ws_stream) = websocket.split();

    // tunnel channel
    let (tunnel_tx, mut tunnel_rx) = unbounded::<ControlPacket>();

    // continuously write to websocket tunnel
    let mut restart = restart_tx.clone();
    tokio::spawn(async move {
        loop {
            let packet = match tunnel_rx.next().await {
                Some(data) => data,
                None => {
                    warn!("control flow didn't send anything!");
                    let _ = restart.send(Some(Error::Timeout)).await;
                    return;
                }
            };

            if let Err(e) = ws_sink.send(Message::binary(packet.serialize())).await {
                warn!("failed to write message to tunnel websocket: {:?}", e);
                let _ = restart.send(Some(Error::WebSocketError(e))).await;
                return;
            }
        }
    });

    // continuously read from websocket tunnel

    loop {
        match ws_stream.next().await {
            Some(Ok(message)) if message.is_close() => {
                debug!("got close message");
                let _ = restart_tx.send(None).await;
                return Ok(());
            }
            Some(Ok(message)) => {
                let packet = process_control_flow_message(
                    &introspect,
                    tunnel_tx.clone(),
                    message.into_data(),
                )
                .await
                .map_err(|e| {
                    error!("Malformed protocol control packet: {:?}", e);
                    Error::MalformedMessageFromServer
                })?;
                debug!("Processed packet: {:?}", packet.packet_type());
            }
            Some(Err(e)) => {
                warn!("websocket read error: {:?}", e);
                return Err(Error::Timeout);
            }
            None => {
                warn!("websocket sent none");
                return Err(Error::Timeout);
            }
        }
    }
}

async fn connect_to_wormhole(
    config: &Config,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, Error> {
    let spinner = if config.first_run {
        eprintln!(
            "{}\n\n",
            format!("{}", include_str!("../static/img/wormhole_ascii.txt")).green()
        );
        Some(spinner::new_spinner(
            "initializing remote tunnel, please stand by",
        ))
    } else {
        None
    };

    let (mut websocket, _) = tokio_tungstenite::connect_async(&config.control_url).await?;

    // send our Client Hello message
    let client_hello = match config.secret_key.clone() {
        Some(secret_key) => ClientHello::generate(
            config.sub_domain.clone(),
            ClientType::Auth { key: secret_key },
        ),
        None => {
            // if we have a reconnect token, use it.
            if let Some(reconnect) = RECONNECT_TOKEN.lock().await.clone() {
                ClientHello::reconnect(reconnect)
            } else {
                ClientHello::generate(config.sub_domain.clone(), ClientType::Anonymous)
            }
        }
    };

    info!("connecting to wormhole...");

    let hello = serde_json::to_vec(&client_hello).unwrap();
    websocket
        .send(Message::binary(hello))
        .await
        .expect("Failed to send client hello to wormhole server.");

    // wait for Server hello
    let server_hello_data = websocket
        .next()
        .await
        .ok_or(Error::NoResponseFromServer)??
        .into_data();
    let server_hello = serde_json::from_slice::<ServerHello>(&server_hello_data).map_err(|e| {
        error!("Couldn't parse server_hello from {:?}", e);
        Error::ServerReplyInvalid
    })?;

    let sub_domain = match server_hello {
        ServerHello::Success {
            sub_domain,
            client_id,
            ..
        } => {
            info!("Server accepted our connection. I am client_{}", client_id);
            sub_domain
        }
        ServerHello::AuthFailed => {
            return Err(Error::AuthenticationFailed);
        }
        ServerHello::InvalidSubDomain => {
            return Err(Error::InvalidSubDomain);
        }
        ServerHello::SubDomainInUse => {
            return Err(Error::SubDomainInUse);
        }
        ServerHello::Error(error) => return Err(Error::ServerError(error)),
    };

    // either first run or the tunnel changed domains
    // Note: the latter should rarely occur.
    if config.first_run || config.sub_domain.as_ref() != Some(&sub_domain) {
        if let Some(pb) = spinner {
            pb.finish_with_message(&format!(
                "Success! Remote tunnel created on: {}",
                &config.activation_url(&sub_domain).bold().green()
            ));
        }

        if config.sub_domain.is_some() && (config.sub_domain.as_ref() != Some(&sub_domain)) {
            if config.secret_key.is_some() {
                eprintln!("{}",
                          ">>> Notice: to use custom sub-domains feature, please upgrade your billing plan at https://dashboard.tunnelto.dev.".yellow());
            } else {
                eprintln!("{}",
                          ">>> Notice: to access the sub-domain feature, get your authentication key at https://dashboard.tunnelto.dev.".yellow());
            }
        }

        let p = match (config.scheme.as_str(), config.local_port.as_ref()) {
            (_, Some(p)) => format!(":{}", p),
            ("http", None) => ":8000".to_string(),
            (_, _) => "".to_string(),
        };

        eprintln!(
            "{} Forwarding to {}://{}{}\n",
            "=>".green(),
            config.scheme,
            config.local_host,
            p.yellow()
        );
    }

    Ok(websocket)
}

async fn process_control_flow_message(
    introspect: &IntrospectionAddrs,
    mut tunnel_tx: UnboundedSender<ControlPacket>,
    payload: Vec<u8>,
) -> Result<ControlPacket, Box<dyn std::error::Error>> {
    let control_packet = ControlPacket::deserialize(&payload)?;

    match &control_packet {
        ControlPacket::Init(stream_id) => {
            info!("stream[{:?}] -> init", stream_id.to_string());
        }
        ControlPacket::Ping(reconnect_token) => {
            log::info!("got ping. reconnect_token={}", reconnect_token.is_some());

            if let Some(reconnect) = reconnect_token {
                let _ = RECONNECT_TOKEN.lock().await.replace(reconnect.clone());
            }
            let _ = tunnel_tx.send(ControlPacket::Ping(None)).await;
        }
        ControlPacket::Refused(_) => return Err("unexpected control packet".into()),
        ControlPacket::End(stream_id) => {
            // find the stream
            let stream_id = stream_id.clone();

            info!("got end stream [{:?}]", &stream_id);

            tokio::spawn(async move {
                let stream = ACTIVE_STREAMS.read().unwrap().get(&stream_id).cloned();
                if let Some(mut tx) = stream {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = tx.send(StreamMessage::Close).await.map_err(|e| {
                        error!("failed to send stream close: {:?}", e);
                    });
                    ACTIVE_STREAMS.write().unwrap().remove(&stream_id);
                }
            });
        }
        ControlPacket::Data(stream_id, data) => {
            info!(
                "stream[{:?}] -> new data: {:?}",
                stream_id.to_string(),
                data.len()
            );

            if !ACTIVE_STREAMS.read().unwrap().contains_key(&stream_id) {
                local::setup_new_stream(
                    introspect.forward_address.port(),
                    tunnel_tx.clone(),
                    stream_id.clone(),
                )
                .await;
            }

            // find the right stream
            let active_stream = ACTIVE_STREAMS.read().unwrap().get(&stream_id).cloned();

            // forward data to it
            if let Some(mut tx) = active_stream {
                tx.send(StreamMessage::Data(data.clone())).await?;
                info!("forwarded to local tcp ({})", stream_id.to_string());
            } else {
                error!("got data but no stream to send it to.");
                let _ = tunnel_tx
                    .send(ControlPacket::Refused(stream_id.clone()))
                    .await?;
            }
        }
    };

    Ok(control_packet.clone())
}
