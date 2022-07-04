use std::collections::HashSet;
use std::error::Error;
use std::iter::FusedIterator;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use flume::{Receiver, Sender};
use num::BigInt;
use rand::rngs::OsRng;
use rayon::iter::ParallelIterator;
use reqwest::Client as HttpClient;
use rsa::{PaddingScheme, PublicKeyParts, RsaPrivateKey};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::digest::Update;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{oneshot, Semaphore};
use uuid::Uuid;

use crate::config::{Config, ServerListPing};
use crate::player_textures::SignedPlayerTextures;
use crate::protocol::codec::{Decoder, Encoder};
use crate::protocol::packets::handshake::{Handshake, HandshakeNextState};
use crate::protocol::packets::login::c2s::{EncryptionResponse, LoginStart, VerifyTokenOrMsgSig};
use crate::protocol::packets::login::s2c::{EncryptionRequest, LoginSuccess, SetCompression};
use crate::protocol::packets::play::c2s::C2sPlayPacket;
use crate::protocol::packets::play::s2c::S2cPlayPacket;
use crate::protocol::packets::status::c2s::{PingRequest, StatusRequest};
use crate::protocol::packets::status::s2c::{PongResponse, StatusResponse};
use crate::protocol::packets::{login, Property};
use crate::protocol::{BoundedArray, BoundedString, VarInt};
use crate::util::valid_username;
use crate::world::Worlds;
use crate::{
    Biome, BiomeId, Client, Clients, Dimension, DimensionId, Entities, Ticks, PROTOCOL_VERSION,
    VERSION_NAME,
};

pub struct Server {
    pub shared: SharedServer,
    pub clients: Clients,
    pub entities: Entities,
    pub worlds: Worlds,
}

/// A handle to a running Minecraft server containing state which is accessible
/// outside the update loop. Servers are internally refcounted and can be shared
/// between threads.
#[derive(Clone)]
pub struct SharedServer(Arc<SharedServerInner>);

struct SharedServerInner {
    cfg: Box<dyn Config>,
    address: SocketAddr,
    tick_rate: Ticks,
    online_mode: bool,
    max_connections: usize,
    incoming_packet_capacity: usize,
    outgoing_packet_capacity: usize,
    tokio_handle: Handle,
    /// Store this here so we don't drop it.
    _tokio_runtime: Option<Runtime>,
    dimensions: Vec<Dimension>,
    biomes: Vec<Biome>,
    /// The instant the server was started.
    start_instant: Instant,
    /// Receiver for new clients past the login stage.
    new_clients_rx: Receiver<NewClientMessage>,
    new_clients_tx: Sender<NewClientMessage>,
    /// Incremented on every game tick.
    tick_counter: AtomicI64,
    /// A semaphore used to limit the number of simultaneous connections to the
    /// server. Closing this semaphore stops new connections.
    connection_sema: Arc<Semaphore>,
    /// The result that will be returned when the server is shut down.
    shutdown_result: Mutex<Option<ShutdownResult>>,
    /// The RSA keypair used for encryption with clients.
    rsa_key: RsaPrivateKey,
    /// The public part of `rsa_key` encoded in DER, which is an ASN.1 format.
    /// This is sent to clients during the authentication process.
    public_key_der: Box<[u8]>,
    /// For session server requests.
    http_client: HttpClient,
}

/// Contains information about a new client.
pub struct NewClientData {
    pub uuid: Uuid,
    pub username: String,
    pub textures: Option<SignedPlayerTextures>,
    pub remote_addr: SocketAddr,
}

struct NewClientMessage {
    ncd: NewClientData,
    reply: oneshot::Sender<S2cPacketChannels>,
}

/// The result type returned from [`ServerConfig::start`] after the server is
/// shut down.
pub type ShutdownResult = Result<(), ShutdownError>;
pub type ShutdownError = Box<dyn Error + Send + Sync + 'static>;

pub(crate) type S2cPacketChannels = (Sender<C2sPlayPacket>, Receiver<S2cPlayPacket>);
pub(crate) type C2sPacketChannels = (Sender<S2cPlayPacket>, Receiver<C2sPlayPacket>);

impl SharedServer {
    pub fn config(&self) -> &(impl Config + ?Sized) {
        self.0.cfg.as_ref()
    }

    pub fn address(&self) -> SocketAddr {
        self.0.address
    }

    pub fn tick_rate(&self) -> Ticks {
        self.0.tick_rate
    }

    pub fn online_mode(&self) -> bool {
        self.0.online_mode
    }

    pub fn max_connections(&self) -> usize {
        self.0.max_connections
    }

    pub fn incoming_packet_capacity(&self) -> usize {
        self.0.incoming_packet_capacity
    }

    pub fn outgoing_packet_capacity(&self) -> usize {
        self.0.outgoing_packet_capacity
    }

    pub fn tokio_handle(&self) -> &Handle {
        &self.0.tokio_handle
    }

    /// Obtains a [`Dimension`] by using its corresponding [`DimensionId`].
    ///
    /// It is safe but unspecified behavior to call this function using a
    /// [`DimensionId`] not originating from the configuration used to construct
    /// the server.
    pub fn dimension(&self, id: DimensionId) -> &Dimension {
        self.0
            .dimensions
            .get(id.0 as usize)
            .expect("invalid dimension ID")
    }

    /// Returns an iterator over all added dimensions and their associated
    /// [`DimensionId`].
    pub fn dimensions(&self) -> impl FusedIterator<Item = (DimensionId, &Dimension)> + Clone {
        self.0
            .dimensions
            .iter()
            .enumerate()
            .map(|(i, d)| (DimensionId(i as u16), d))
    }

    /// Obtains a [`Biome`] by using its corresponding [`BiomeId`].
    pub fn biome(&self, id: BiomeId) -> &Biome {
        self.0.biomes.get(id.0 as usize).expect("invalid biome ID")
    }

    /// Returns an iterator over all added biomes and their associated
    /// [`BiomeId`] in ascending order.
    pub fn biomes(
        &self,
    ) -> impl ExactSizeIterator<Item = (BiomeId, &Biome)> + DoubleEndedIterator + FusedIterator + Clone
    {
        self.0
            .biomes
            .iter()
            .enumerate()
            .map(|(i, b)| (BiomeId(i as u16), b))
    }

    /// Returns the instant the server was started.
    pub fn start_instant(&self) -> Instant {
        self.0.start_instant
    }

    /// Returns the number of ticks that have elapsed since the server began.
    pub fn current_tick(&self) -> Ticks {
        self.0.tick_counter.load(Ordering::SeqCst)
    }

    /// Immediately stops new connections to the server and initiates server
    /// shutdown. The given result is returned through [`ServerConfig::start`].
    ///
    /// You may want to disconnect all players with a message prior to calling
    /// this function.
    pub fn shutdown<R, E>(&self, res: R)
    where
        R: Into<Result<(), E>>,
        E: Into<Box<dyn Error + Send + Sync + 'static>>,
    {
        self.0.connection_sema.close();
        *self.0.shutdown_result.lock().unwrap() = Some(res.into().map_err(|e| e.into()));
    }
}

/// Consumes the configuration and starts the server.
///
/// The function returns when the server has shut down, a runtime error
/// occurs, or the configuration is invalid.
pub fn start_server(config: impl Config) -> ShutdownResult {
    let shared = setup_server(config).map_err(ShutdownError::from)?;

    let _guard = shared.tokio_handle().enter();

    let mut server = Server {
        shared: shared.clone(),
        clients: Clients::new(),
        entities: Entities::new(),
        worlds: Worlds::new(shared.clone()),
    };

    shared.config().init(&mut server);

    tokio::spawn(do_accept_loop(shared));

    do_update_loop(&mut server)
}

fn setup_server(cfg: impl Config) -> anyhow::Result<SharedServer> {
    let max_connections = cfg.max_connections();
    let address = cfg.address();
    let tick_rate = cfg.tick_rate();

    ensure!(tick_rate > 0, "tick rate must be greater than zero");

    let online_mode = cfg.online_mode();

    let incoming_packet_capacity = cfg.incoming_packet_capacity();

    ensure!(
        incoming_packet_capacity > 0,
        "serverbound packet capacity must be nonzero"
    );

    let outgoing_packet_capacity = cfg.outgoing_packet_capacity();

    ensure!(
        outgoing_packet_capacity > 0,
        "outgoing packet capacity must be nonzero"
    );

    let tokio_handle = cfg.tokio_handle();
    let dimensions = cfg.dimensions();

    ensure!(
        !dimensions.is_empty(),
        "at least one dimension must be added"
    );

    ensure!(
        dimensions.len() <= u16::MAX as usize,
        "more than u16::MAX dimensions added"
    );

    for (i, dim) in dimensions.iter().enumerate() {
        ensure!(
            dim.min_y % 16 == 0 && (-2032..=2016).contains(&dim.min_y),
            "invalid min_y in dimension #{i}",
        );

        ensure!(
            dim.height % 16 == 0
                && (0..=4064).contains(&dim.height)
                && dim.min_y.saturating_add(dim.height) <= 2032,
            "invalid height in dimension #{i}",
        );

        ensure!(
            (0.0..=1.0).contains(&dim.ambient_light),
            "ambient_light is out of range in dimension #{i}",
        );

        if let Some(fixed_time) = dim.fixed_time {
            assert!(
                (0..=24_000).contains(&fixed_time),
                "fixed_time is out of range in dimension #{i}",
            );
        }
    }

    let biomes = cfg.biomes();

    ensure!(!biomes.is_empty(), "at least one biome must be added");

    ensure!(
        biomes.len() <= u16::MAX as usize,
        "more than u16::MAX biomes added"
    );

    let mut names = HashSet::new();

    for biome in biomes.iter() {
        ensure!(
            names.insert(biome.name.clone()),
            "biome \"{}\" already added",
            biome.name
        );
    }

    let rsa_key = RsaPrivateKey::new(&mut OsRng, 1024)?;

    let public_key_der =
        rsa_der::public_key_to_der(&rsa_key.n().to_bytes_be(), &rsa_key.e().to_bytes_be())
            .into_boxed_slice();

    let (new_clients_tx, new_clients_rx) = flume::bounded(1);

    let runtime = if tokio_handle.is_none() {
        Some(Runtime::new()?)
    } else {
        None
    };

    let tokio_handle = match &runtime {
        Some(rt) => rt.handle().clone(),
        None => tokio_handle.unwrap(),
    };

    let server = SharedServerInner {
        cfg: Box::new(cfg),
        address,
        tick_rate,
        online_mode,
        max_connections,
        incoming_packet_capacity,
        outgoing_packet_capacity,
        tokio_handle,
        _tokio_runtime: runtime,
        dimensions,
        biomes,
        start_instant: Instant::now(),
        new_clients_rx,
        new_clients_tx,
        tick_counter: AtomicI64::new(0),
        connection_sema: Arc::new(Semaphore::new(max_connections)),
        shutdown_result: Mutex::new(None),
        rsa_key,
        public_key_der,
        http_client: HttpClient::new(),
    };

    Ok(SharedServer(Arc::new(server)))
}

fn do_update_loop(server: &mut Server) -> ShutdownResult {
    let mut tick_start = Instant::now();

    let shared = server.shared.clone();
    loop {
        if let Some(res) = shared.0.shutdown_result.lock().unwrap().take() {
            return res;
        }

        while let Ok(msg) = shared.0.new_clients_rx.try_recv() {
            join_player(server, msg);
        }

        // Get serverbound packets first so they are not dealt with a tick late.
        server.clients.par_iter_mut().for_each(|(_, client)| {
            client.handle_serverbound_packets(&server.entities);
        });

        shared.config().update(server);

        server.worlds.par_iter_mut().for_each(|(id, world)| {
            world.chunks.par_iter_mut().for_each(|(_, chunk)| {
                if chunk.created_tick() == shared.current_tick() {
                    // Chunks created this tick can have their changes applied immediately because
                    // they have not been observed by clients yet. Clients will not have to be sent
                    // the block change packet in this case.
                    chunk.apply_modifications();
                }
            });

            world.spatial_index.update(&server.entities, id);
        });

        server.clients.par_iter_mut().for_each(|(_, client)| {
            client.update(&shared, &server.entities, &server.worlds);
        });

        server.entities.update();

        server.worlds.par_iter_mut().for_each(|(_, world)| {
            world.chunks.par_iter_mut().for_each(|(_, chunk)| {
                chunk.apply_modifications();
            });

            world.meta.update();
        });

        // Sleep for the remainder of the tick.
        let tick_duration = Duration::from_secs_f64((shared.0.tick_rate as f64).recip());
        thread::sleep(tick_duration.saturating_sub(tick_start.elapsed()));

        tick_start = Instant::now();
        shared.0.tick_counter.fetch_add(1, Ordering::SeqCst);
    }
}

fn join_player(server: &mut Server, msg: NewClientMessage) {
    let (clientbound_tx, clientbound_rx) = flume::bounded(server.shared.0.outgoing_packet_capacity);
    let (serverbound_tx, serverbound_rx) = flume::bounded(server.shared.0.incoming_packet_capacity);

    let s2c_packet_channels: S2cPacketChannels = (serverbound_tx, clientbound_rx);
    let c2s_packet_channels: C2sPacketChannels = (clientbound_tx, serverbound_rx);

    let _ = msg.reply.send(s2c_packet_channels);

    let client = Client::new(c2s_packet_channels, &server.shared, msg.ncd);

    server.clients.insert(client);
}

struct Codec {
    enc: Encoder<OwnedWriteHalf>,
    dec: Decoder<OwnedReadHalf>,
}

async fn do_accept_loop(server: SharedServer) {
    log::trace!("entering accept loop");

    let listener = match TcpListener::bind(server.0.address).await {
        Ok(listener) => listener,
        Err(e) => {
            server.shutdown(Err(e).context("failed to start TCP listener"));
            return;
        }
    };

    loop {
        match server.0.connection_sema.clone().acquire_owned().await {
            Ok(permit) => match listener.accept().await {
                Ok((stream, remote_addr)) => {
                    let server = server.clone();
                    tokio::spawn(async move {
                        // Setting TCP_NODELAY to true appears to trade some throughput for improved
                        // latency. Testing is required to determine if this is worth keeping.
                        if let Err(e) = stream.set_nodelay(true) {
                            log::error!("failed to set TCP nodelay: {e}");
                        }

                        if let Err(e) = handle_connection(server, stream, remote_addr).await {
                            log::debug!("connection to {remote_addr} ended: {e:#}");
                        }
                        drop(permit);
                    });
                }
                Err(e) => {
                    log::error!("failed to accept incoming connection: {e}");
                }
            },
            // Closed semaphore indicates server shutdown.
            Err(_) => return,
        }
    }
}

async fn handle_connection(
    server: SharedServer,
    stream: TcpStream,
    remote_addr: SocketAddr,
) -> anyhow::Result<()> {
    let timeout = Duration::from_secs(10);

    let (read, write) = stream.into_split();
    let mut c = Codec {
        enc: Encoder::new(write, timeout),
        dec: Decoder::new(read, timeout),
    };

    // TODO: peek stream for 0xFE legacy ping

    match c.dec.read_packet::<Handshake>().await?.next_state {
        HandshakeNextState::Status => handle_status(server, &mut c, remote_addr)
            .await
            .context("error during status"),
        HandshakeNextState::Login => match handle_login(&server, &mut c, remote_addr)
            .await
            .context("error during login")?
        {
            Some(npd) => handle_play(&server, c, npd)
                .await
                .context("error during play"),
            None => Ok(()),
        },
    }
}

async fn handle_status(
    server: SharedServer,
    c: &mut Codec,
    remote_addr: SocketAddr,
) -> anyhow::Result<()> {
    c.dec.read_packet::<StatusRequest>().await?;

    match server.0.cfg.server_list_ping(&server, remote_addr).await {
        ServerListPing::Respond {
            online_players,
            max_players,
            description,
            favicon_png,
        } => {
            let mut json = json!({
                "version": {
                    "name": VERSION_NAME,
                    "protocol": PROTOCOL_VERSION
                },
                "players": {
                    "online": online_players,
                    "max": max_players,
                    // TODO: player sample?
                },
                "description": description,
            });

            if let Some(data) = favicon_png {
                let mut buf = "data:image/png;base64,".to_string();
                base64::encode_config_buf(data, base64::STANDARD, &mut buf);
                json.as_object_mut()
                    .unwrap()
                    .insert("favicon".to_string(), Value::String(buf));
            }

            c.enc
                .write_packet(&StatusResponse {
                    json_response: json.to_string(),
                })
                .await?;
        }
        ServerListPing::Ignore => return Ok(()),
    }

    let PingRequest { payload } = c.dec.read_packet().await?;

    c.enc.write_packet(&PongResponse { payload }).await?;

    Ok(())
}

/// Handle the login process and return the new player's data if successful.
async fn handle_login(
    server: &SharedServer,
    c: &mut Codec,
    remote_addr: SocketAddr,
) -> anyhow::Result<Option<NewClientData>> {
    let LoginStart {
        username: BoundedString(username),
        sig_data: _, // TODO
    } = c.dec.read_packet().await?;

    ensure!(valid_username(&username), "invalid username '{username}'");

    let (uuid, textures) = if server.0.online_mode {
        let my_verify_token: [u8; 16] = rand::random();

        c.enc
            .write_packet(&EncryptionRequest {
                server_id: Default::default(), // Always empty
                public_key: server.0.public_key_der.to_vec(),
                verify_token: my_verify_token.to_vec().into(),
            })
            .await?;

        let EncryptionResponse {
            shared_secret: BoundedArray(encrypted_shared_secret),
            token_or_sig,
        } = c.dec.read_packet().await?;

        let shared_secret = server
            .0
            .rsa_key
            .decrypt(PaddingScheme::PKCS1v15Encrypt, &encrypted_shared_secret)
            .context("failed to decrypt shared secret")?;

        let _opt_signature = match token_or_sig {
            VerifyTokenOrMsgSig::VerifyToken(BoundedArray(encrypted_verify_token)) => {
                let verify_token = server
                    .0
                    .rsa_key
                    .decrypt(PaddingScheme::PKCS1v15Encrypt, &encrypted_verify_token)
                    .context("failed to decrypt verify token")?;

                ensure!(
                    my_verify_token.as_slice() == verify_token,
                    "verify tokens do not match"
                );
                None
            }
            VerifyTokenOrMsgSig::MsgSig(sig) => Some(sig),
        };

        let crypt_key: [u8; 16] = shared_secret
            .as_slice()
            .try_into()
            .context("shared secret has the wrong length")?;

        c.enc.enable_encryption(&crypt_key);
        c.dec.enable_encryption(&crypt_key);

        #[derive(Debug, Deserialize)]
        struct AuthResponse {
            id: String,
            name: String,
            properties: Vec<Property>,
        }

        let hash = Sha1::new()
            .chain(&shared_secret)
            .chain(&server.0.public_key_der)
            .finalize();

        let hex_hash = weird_hex_encoding(&hash);

        let url = format!("https://sessionserver.mojang.com/session/minecraft/hasJoined?username={username}&serverId={hex_hash}&ip={}", remote_addr.ip());
        let resp = server.0.http_client.get(url).send().await?;

        let status = resp.status();
        ensure!(
            status.is_success(),
            "session server GET request failed: {status}"
        );

        let data: AuthResponse = resp.json().await?;

        ensure!(data.name == username, "usernames do not match");

        let uuid = Uuid::parse_str(&data.id).context("failed to parse player's UUID")?;

        let textures = match data.properties.into_iter().find(|p| p.name == "textures") {
            Some(p) => SignedPlayerTextures::from_base64(
                p.value,
                p.signature.context("missing signature for textures")?,
            )?,
            None => bail!("failed to find textures in auth response"),
        };

        (uuid, Some(textures))
    } else {
        // Derive the player's UUID from a hash of their username.
        let uuid = Uuid::from_slice(&Sha256::digest(&username)[..16]).unwrap();

        (uuid, None)
    };

    let compression_threshold = 256;
    c.enc
        .write_packet(&SetCompression {
            threshold: VarInt(compression_threshold as i32),
        })
        .await?;

    c.enc.enable_compression(compression_threshold);
    c.dec.enable_compression(compression_threshold);

    let npd = NewClientData {
        uuid,
        username,
        textures,
        remote_addr,
    };

    if let Err(reason) = server.0.cfg.login(server, &npd).await {
        log::info!("Disconnect at login: \"{reason}\"");
        c.enc
            .write_packet(&login::s2c::Disconnect { reason })
            .await?;
        return Ok(None);
    }

    c.enc
        .write_packet(&LoginSuccess {
            uuid: npd.uuid,
            username: npd.username.clone().into(),
            properties: Vec::new(),
        })
        .await?;

    Ok(Some(npd))
}

async fn handle_play(server: &SharedServer, c: Codec, ncd: NewClientData) -> anyhow::Result<()> {
    let (reply_tx, reply_rx) = oneshot::channel();

    server
        .0
        .new_clients_tx
        .send_async(NewClientMessage {
            ncd,
            reply: reply_tx,
        })
        .await?;

    let (packet_tx, packet_rx) = match reply_rx.await {
        Ok(res) => res,
        Err(_) => return Ok(()), // Server closed
    };

    let Codec { mut enc, mut dec } = c;

    tokio::spawn(async move {
        while let Ok(pkt) = packet_rx.recv_async().await {
            if let Err(e) = enc.write_packet(&pkt).await {
                log::debug!("error while sending play packet: {e:#}");
                break;
            }
        }
    });

    loop {
        let pkt = dec.read_packet().await?;
        if packet_tx.send_async(pkt).await.is_err() {
            break;
        }
    }

    Ok(())
}

fn weird_hex_encoding(bytes: &[u8]) -> String {
    BigInt::from_signed_bytes_be(bytes).to_str_radix(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weird_hex_encoding_correct() {
        assert_eq!(
            weird_hex_encoding(&Sha1::digest("Notch")),
            "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
        );
        assert_eq!(
            weird_hex_encoding(&Sha1::digest("jeb_")),
            "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
        );
        assert_eq!(
            weird_hex_encoding(&Sha1::digest("simon")),
            "88e16a1019277b15d58faf0541e11910eb756f6"
        );
    }
}
