use std::net::SocketAddr;

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use log::info;

use naia_client_socket::{ClientSocket, ClientSocketTrait, MessageSender};
pub use naia_shared::{
    ConnectionConfig, EntityType, Event, EventType, HostTickManager, LocalEntityKey, ManagerType,
    Manifest, PacketReader, PacketType, PacketWriter, SharedConfig, Timer, Timestamp,
};

use super::{
    client_config::ClientConfig, client_entity_message::ClientEntityMessage,
    client_event::ClientEvent, client_tick_manager::ClientTickManager, error::NaiaClientError,
    server_connection::ServerConnection, Packet,
};
use crate::client_connection_state::{
    ClientConnectionState, ClientConnectionState::AwaitingChallengeResponse,
};
use naia_shared::StandardHeader;

/// Client can send/receive events to/from a server, and has a pool of in-scope
/// entities that are synced with the server
#[derive(Debug)]
pub struct NaiaClient<T: EventType, U: EntityType> {
    manifest: Manifest<T, U>,
    server_address: SocketAddr,
    connection_config: ConnectionConfig,
    socket: Box<dyn ClientSocketTrait>,
    sender: MessageSender,
    server_connection: Option<ServerConnection<T, U>>,
    pre_connection_timestamp: Option<Timestamp>,
    pre_connection_digest: Option<Box<[u8]>>,
    handshake_timer: Timer,
    connection_state: ClientConnectionState,
    auth_event: Option<T>,
    tick_manager: ClientTickManager,
}

impl<T: EventType, U: EntityType> NaiaClient<T, U> {
    /// Create a new client, given the server's address, a shared manifest, an
    /// optional Config, and an optional Authentication event
    pub fn new(
        server_address: SocketAddr,
        manifest: Manifest<T, U>,
        client_config: Option<ClientConfig>,
        shared_config: SharedConfig,
        auth: Option<T>,
    ) -> Self {
        let client_config = match client_config {
            Some(config) => config,
            None => ClientConfig::default(),
        };

        let connection_config = ConnectionConfig::new(
            client_config.disconnection_timeout_duration,
            client_config.heartbeat_interval,
            client_config.ping_interval,
            client_config.ping_sample_size,
        );

        let mut client_socket = ClientSocket::connect(server_address);
        if let Some(config) = shared_config.link_condition_config {
            client_socket = client_socket.with_link_conditioner(&config);
        }

        let mut handshake_timer = Timer::new(client_config.send_handshake_interval);
        handshake_timer.ring_manual();
        let message_sender = client_socket.get_sender();

        NaiaClient {
            server_address,
            manifest,
            socket: client_socket,
            sender: message_sender,
            connection_config,
            handshake_timer,
            server_connection: None,
            pre_connection_timestamp: None,
            pre_connection_digest: None,
            connection_state: AwaitingChallengeResponse,
            auth_event: auth,
            tick_manager: ClientTickManager::new(shared_config.tick_interval),
        }
    }

    /// Must be called regularly, performs updates to the connection, and
    /// retrieves event/entity updates sent by the Server
    pub fn receive(&mut self) -> Result<ClientEvent<T>, NaiaClientError> {
        // update current tick
        self.tick_manager.update_frame();

        // send handshakes, send heartbeats, timeout if need be
        match &mut self.server_connection {
            Some(connection) => {
                if connection.should_drop() {
                    self.server_connection = None;
                    self.pre_connection_timestamp = None;
                    self.pre_connection_digest = None;
                    self.connection_state = AwaitingChallengeResponse;
                    return Ok(ClientEvent::Disconnection);
                }
                if connection.should_send_heartbeat() {
                    NaiaClient::internal_send_with_connection(
                        &mut self.sender,
                        connection,
                        PacketType::Heartbeat,
                        Packet::empty(),
                    );
                }
                // send a packet
                while let Some(payload) = connection.get_outgoing_packet(&self.manifest) {
                    self.sender
                        .send(Packet::new_raw(payload))
                        .expect("send failed!");
                    connection.mark_sent();
                }
                // receive event
                if let Some(event) = connection.get_incoming_event() {
                    return Ok(ClientEvent::Event(event));
                }
                // receive entity message
                if let Some(message) = connection.get_incoming_entity_message() {
                    match message {
                        ClientEntityMessage::Create(local_key) => {
                            return Ok(ClientEvent::CreateEntity(local_key));
                        }
                        ClientEntityMessage::Delete(local_key) => {
                            return Ok(ClientEvent::DeleteEntity(local_key));
                        }
                        ClientEntityMessage::Update(local_key) => {
                            return Ok(ClientEvent::UpdateEntity(local_key));
                        }
                    }
                }
            }
            None => {
                if self.handshake_timer.ringing() {
                    match self.connection_state {
                        ClientConnectionState::AwaitingChallengeResponse => {
                            if self.pre_connection_timestamp.is_none() {
                                self.pre_connection_timestamp = Some(Timestamp::now());
                            }

                            let mut timestamp_bytes = Vec::new();
                            self.pre_connection_timestamp
                                .as_mut()
                                .unwrap()
                                .write(&mut timestamp_bytes);
                            NaiaClient::<T, U>::internal_send_connectionless(
                                &mut self.sender,
                                PacketType::ClientChallengeRequest,
                                Packet::new(timestamp_bytes),
                            );
                        }
                        ClientConnectionState::AwaitingConnectResponse => {
                            // write timestamp & digest into payload
                            let mut payload_bytes = Vec::new();
                            self.pre_connection_timestamp
                                .as_mut()
                                .unwrap()
                                .write(&mut payload_bytes);
                            for digest_byte in self.pre_connection_digest.as_ref().unwrap().as_ref()
                            {
                                payload_bytes.push(*digest_byte);
                            }
                            // write auth event object if there is one
                            if let Some(auth_event) = &mut self.auth_event {
                                let type_id = auth_event.get_type_id();
                                let naia_id = self.manifest.get_event_naia_id(&type_id); // get naia id
                                payload_bytes.write_u16::<BigEndian>(naia_id).unwrap(); // write naia id
                                auth_event.write(&mut payload_bytes);
                            }
                            info!(
                                "sending ClientConnectRequest with tick: {}",
                                self.tick_manager.get_tick()
                            );
                            NaiaClient::<T, U>::internal_send_connectionless(
                                &mut self.sender,
                                PacketType::ClientConnectRequest,
                                Packet::new(payload_bytes),
                            );
                        }
                        _ => {}
                    }

                    self.handshake_timer.reset();
                }
            }
        }

        // receive from socket
        let mut output: Option<Result<ClientEvent<T>, NaiaClientError>> = None;
        while output.is_none() {
            match self.socket.receive() {
                Ok(event) => match event {
                    Some(packet) => {
                        let server_connection_wrapper = self.server_connection.as_mut();

                        if let Some(server_connection) = server_connection_wrapper {
                            server_connection.mark_heard();

                            let (header, payload) = StandardHeader::read(packet.payload());
                            server_connection.process_incoming_header(&header);

                            match header.packet_type() {
                                PacketType::Data => {
                                    server_connection
                                        .process_incoming_data(&self.manifest, &payload);
                                    continue;
                                }
                                PacketType::Heartbeat => {
                                    continue;
                                }
                                _ => {}
                            }
                        } else {
                            let (header, payload) = StandardHeader::read(packet.payload());
                            match header.packet_type() {
                                PacketType::ServerChallengeResponse => {
                                    if self.connection_state
                                        == ClientConnectionState::AwaitingChallengeResponse
                                    {
                                        if let Some(my_timestamp) = self.pre_connection_timestamp {
                                            let mut reader = PacketReader::new(&payload);
                                            let server_tick = reader
                                                .get_cursor()
                                                .read_u16::<BigEndian>()
                                                .unwrap();
                                            let payload_timestamp = Timestamp::read(&mut reader);

                                            if my_timestamp == payload_timestamp {
                                                let mut digest_bytes: Vec<u8> = Vec::new();
                                                for _ in 0..32 {
                                                    digest_bytes.push(reader.read_u8());
                                                }
                                                self.pre_connection_digest =
                                                    Some(digest_bytes.into_boxed_slice());
                                                info!("receiving ServerChallengeResponse");

                                                self.tick_manager.set_tick(server_tick);

                                                self.connection_state =
                                                    ClientConnectionState::AwaitingConnectResponse;
                                            }
                                        }
                                    }

                                    continue;
                                }
                                PacketType::ServerConnectResponse => {
                                    let server_connection = ServerConnection::new(
                                        self.server_address,
                                        &self.connection_config,
                                    );

                                    self.server_connection = Some(server_connection);
                                    self.connection_state = ClientConnectionState::Connected;
                                    output = Some(Ok(ClientEvent::Connection));
                                    continue;
                                }
                                _ => {}
                            }
                        }
                    }
                    None => {
                        output = Some(Ok(ClientEvent::None));
                        continue;
                    }
                },
                Err(error) => {
                    output = Some(Err(NaiaClientError::Wrapped(Box::new(error))));
                    continue;
                }
            }
        }
        return output.unwrap();
    }

    /// Queues up an Event to be sent to the Server
    pub fn send_event(&mut self, event: &impl Event<T>) {
        if let Some(connection) = &mut self.server_connection {
            connection.queue_event(event);
        }
    }

    fn internal_send_with_connection(
        sender: &mut MessageSender,
        connection: &mut ServerConnection<T, U>,
        packet_type: PacketType,
        packet: Packet,
    ) {
        let new_payload = connection.process_outgoing_header(packet_type, packet.payload());
        sender
            .send(Packet::new_raw(new_payload))
            .expect("send failed!");
        connection.mark_sent();
    }

    fn internal_send_connectionless(
        sender: &mut MessageSender,
        packet_type: PacketType,
        packet: Packet,
    ) {
        let new_payload =
            naia_shared::utils::write_connectionless_payload(packet_type, packet.payload());
        sender
            .send(Packet::new_raw(new_payload))
            .expect("send failed!");
    }

    /// Get the address currently associated with the Server
    pub fn server_address(&self) -> SocketAddr {
        return self.server_address;
    }

    /// Get a reference to an Entity currently in scope for the Client, given
    /// that Entity's Key
    pub fn get_entity(&self, key: LocalEntityKey) -> Option<&U> {
        return self
            .server_connection
            .as_ref()
            .unwrap()
            .get_local_entity(key);
    }
}
