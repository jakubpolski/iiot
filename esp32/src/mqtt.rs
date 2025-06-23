#![deny(unused_must_use)]

use core::cell::UnsafeCell;

extern crate alloc;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use heapless::String;
use alloc::fmt::Write;

use embassy_net::{tcp::{ConnectError, TcpSocket}, IpAddress, Stack};
use embassy_time::Duration;
use rust_mqtt::{client::{client::MqttClient, client_config::ClientConfig}, packet::v5::{publish_packet::QualityOfService, reason_codes::ReasonCode}, utils::rng_generator::CountingRng};
use log::info;
use anyhow::Result;

use crate::ui::ValueType;

// static ip provided by private vpn
const MQTT_BROKER_IP: IpAddress = IpAddress::v4(100, 64, 0, 8);
const MQTT_ENDPOINT:(IpAddress, u16) = (MQTT_BROKER_IP, 1883);

const SOCKET_BUFFER_LEN: usize = 4096;
static RX_BUFFER: StaticBuffer<SOCKET_BUFFER_LEN> = StaticBuffer::<SOCKET_BUFFER_LEN>::new();
static TX_BUFFER: StaticBuffer<SOCKET_BUFFER_LEN> = StaticBuffer::<SOCKET_BUFFER_LEN>::new();

const MQTT_BUFFER_LEN: usize = 80;
static RECV_BUFFER: StaticBuffer<MQTT_BUFFER_LEN> = StaticBuffer::<MQTT_BUFFER_LEN>::new();
static WRITE_BUFFER: StaticBuffer<MQTT_BUFFER_LEN> = StaticBuffer::<MQTT_BUFFER_LEN>::new();

pub static MQTT_CMD_CHANNEL: Channel<CriticalSectionRawMutex, MqttMessage, 20> = Channel::new();
pub static MQTT_RESP_CHANNEL: Channel<CriticalSectionRawMutex, MqttResponse, 20> = Channel::new();


// basic error handling
#[derive(Debug)]
pub enum MqttError {
    ConnectionFailed,
    ProtocolError(()),
}

impl From<ReasonCode> for MqttError {
    fn from(_code: ReasonCode) -> Self {
        MqttError::ProtocolError(())
    }
}

impl From<ConnectError> for MqttError {
    fn from(_: ConnectError) -> Self {
        MqttError::ConnectionFailed
    }
}


type Client<'c> = MqttClient<'c, TcpSocket<'c>, 5, CountingRng>;
pub struct Mqtt<'a> {
    wifi_stack: Stack<'static>,
    // normally i would put this in a mutex, but it's only accessed in mqtt_task
    pub client: Option<Client<'a>>,
}

impl<'a> Mqtt<'a> {
    pub fn new(
        wifi_stack: Stack<'static>,
    ) -> Self {
        Self {
            wifi_stack,
            client: None,
        }
    }

    pub async fn connect(&mut self) -> Result<(), MqttError> {
        info!("MQTT: Connecting");
        // deleting the old client
        self.client = None;

        // clearing the buffers (old client has to be deleted so buffers can be assumed exclusive without a mutex)
        let rx_buffer = unsafe { RX_BUFFER.assume_exclusive() };
        let tx_buffer = unsafe { TX_BUFFER.assume_exclusive() };
        let recv_buffer = unsafe { RECV_BUFFER.assume_exclusive() };
        let write_buffer = unsafe { WRITE_BUFFER.assume_exclusive() };
        
        // clearing the buffers
        rx_buffer.fill(0);
        tx_buffer.fill(0);
        recv_buffer.fill(0);
        write_buffer.fill(0);

        // creating the socket
        let mut socket = TcpSocket::new(
            self.wifi_stack,
            rx_buffer,
            tx_buffer,
        );
        socket.set_timeout(Some(Duration::from_secs(10)));
        socket.connect(MQTT_ENDPOINT).await?;
        // creating client config
        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(QualityOfService::QoS1);
        config.add_client_id("clientId-ESP32-IIOT");
        config.max_packet_size = 100;
        // creating the client
        let mut client = Client::new(
            socket,
            write_buffer,
            MQTT_BUFFER_LEN,
            recv_buffer,
            MQTT_BUFFER_LEN,
            config,
        );
        client.connect_to_broker().await?;
        info!("MQTT: Connected");
        // saving the client
        self.client = Some(client);
        Ok(())
    }
}




pub struct MqttResponse {
    pub status: Result<(), ()>,
    pub topic: ValueType
}

#[derive(Clone, Copy)]
pub struct MqttMessage {
    pub topic: ValueType,
    pub value: u8,
}


impl MqttMessage {
    pub fn topic(&self) -> &str {
        self.topic.topic()
    }

    pub fn payload<'a>(&self, buf: &'a mut String<3>) -> &'a [u8] {
        buf.clear();
        let _ = write!(buf, "{}", self.value);
        buf.as_bytes()
    }
}

// static buffer struct for reusing buffers between reconnections
struct StaticBuffer<const N: usize>(UnsafeCell<[u8; N]>);
unsafe impl<const N: usize> Sync for StaticBuffer<N> {} // Safety: this struct can only be run single-threaded

impl<const N: usize> StaticBuffer<N> {
    pub const fn new() -> Self {
        Self(UnsafeCell::new([0u8; N]))
    }
    pub unsafe fn assume_exclusive(&self) -> &mut [u8; N] {
        &mut *self.0.get()
    }
    
}