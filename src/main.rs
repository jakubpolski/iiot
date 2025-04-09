#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, IpAddress, Runner, StackResources};
use embassy_time::Timer;
use esp_hal::{
    clock::CpuClock, gpio::Flex, i2c::master::{Config, I2c}, rng::Rng, timer::timg::TimerGroup, Async
};

// thread synchronization
use embassy_sync::channel::Channel;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

mod dht11;
use dht11::Dht11;

use esp_wifi::{init, wifi::{ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice, WifiState}, EspWifiController};
use log::{info, error};
use esp_alloc as _;

extern crate alloc;
use heapless::String;
use alloc::fmt::Write;

// display
use oled_async::{prelude::*, Builder};
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Baseline, Text}
};
use rust_mqtt::{client::{client::MqttClient, client_config::ClientConfig}, packet::v5::reason_codes::ReasonCode, utils::rng_generator::CountingRng};

struct DisplayLine;
impl DisplayLine {
    pub const LINE1:i32 = 0;
    //pub const LINE2:i32 = 15;
    //pub const LINE3:i32 = 30;
    pub const LINE4:i32 = 45;
}

pub struct DisplayMessage {
    pub text: String<21>,
    pub position: Point,
}
static DISPLAY_CHANNEL: Channel<CriticalSectionRawMutex, DisplayMessage, 10> = Channel::new();

type DisplayInterface = display_interface_i2c::I2CInterface<I2c<'static, Async>>;
type DisplayType = oled_async::displays::ssd1309::Ssd1309_128_64;
type GraphicsDisplay = GraphicsMode<DisplayType, DisplayInterface>;

const TEXT_STYLE: MonoTextStyle<'_, BinaryColor> =
    MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .background_color(BinaryColor::Off)
        .build();

#[embassy_executor::task]
async fn display_task(mut display: GraphicsDisplay) {
    info!("display_task begins");
    let receiver = DISPLAY_CHANNEL.receiver();
    let mut needs_flush = false;
    info!("entering display_task loop");
    loop {
        while let Ok(DisplayMessage { text, position }) = receiver.try_receive() {
            info!("Display: Processing message: {}, pos:({},{}), queue len: {}", text, position.x, position.y, receiver.len());
            Text::with_baseline(&text, position, TEXT_STYLE, Baseline::Top)
                .draw(&mut display)
                .unwrap();
            needs_flush = true;
        }
        
        if needs_flush {
            display.flush().await.unwrap();
            needs_flush = false;
        }
        
        Timer::after_millis(100).await;
    }
}


const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const MQTT_BROKER_IP: IpAddress = IpAddress::v4(192, 168, 0, 101);
const MQTT_BUFFER_LEN: usize = 80;


#[esp_hal_embassy::main]
async fn iiot(spawner: Spawner) -> ! {
    // generator version: 0.3.1

    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    info!("Initializing embassy");
    esp_alloc::heap_allocator!(72 * 1024);
    let timg1 = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init(timg1.timer0);
    info!("Embassy initialized");


    info!("Initializing wifi");
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut rng = Rng::new(peripherals.RNG);

    let init = &*mk_static!(
        EspWifiController<'static>,
        init(timg0.timer0, rng.clone(), peripherals.RADIO_CLK).unwrap()
    );

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) = 
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();
    

    let config = embassy_net::Config::dhcpv4(Default::default());
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after_millis(500).await;
    }

    info!("Waiting to get the IP address");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after_millis(500).await;
    }
    info!("Wifi initialized");

    info!("Initializing I2C");
    let i2c = I2c::new(peripherals.I2C0, Config::default())
        .unwrap()
        .with_scl(peripherals.GPIO22)
        .with_sda(peripherals.GPIO21)
        .into_async();
    info!("I2C initialized");

    info!("Initializing the display");
    let di = DisplayInterface::new(i2c, 0x3C, 0x40);
    let display = Builder::new(DisplayType{})
        .with_rotation(crate::DisplayRotation::Rotate180)
        .connect(di);
    
    let mut graphics: GraphicsDisplay = display.into();
    graphics.init().await.unwrap();
    graphics.clear();
    graphics.flush().await.unwrap();
    info!("Display initialized");

    info!("Initializing DHT11");
    let dht_pin = Flex::new(peripherals.GPIO18);
    let mut dht = Dht11::new(dht_pin);
    let _ = dht.read().await; // dummy read for initialization
    let mut buf = String::<21>::new();
    info!("DHT11 initialized!");
    /*
    info!("Starting background tasks");
    let result = spawner.spawn(display_task(graphics));
    info!("Did we spawn it? {:?}", result.is_ok());
    info!("Background tasks started");
    */
    let sender = DISPLAY_CHANNEL.sender();
    loop {
        Timer::after_secs(1).await;
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        let remote_endpoint = (MQTT_BROKER_IP, 1883);

        info!("Connecting to the MQTT broker");
        let connection = socket.connect(remote_endpoint).await;
        if let Err(e) = connection {
            error!("Connection error: {:?}", e);
            continue;
        }
        info!("Connected");

        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1);
        config.add_client_id("clientId-ESP32-IIOT");
        config.max_packet_size = 100;
        let mut recv_buffer = [0; MQTT_BUFFER_LEN];
        let mut write_buffer = [0; MQTT_BUFFER_LEN];

        let mut client =
            MqttClient::<_, 5, _>::new(
                socket,
                &mut write_buffer,
                MQTT_BUFFER_LEN,
                &mut recv_buffer,
                MQTT_BUFFER_LEN,
                config
            );
        
        match client.connect_to_broker().await {
            Ok(()) => {},
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => {
                    error!("MQTT network error");
                    continue;
                },
                _ => {
                    error!("Other MQTT error: {:?}", mqtt_error);
                    continue;
                }
            }
        }
        loop {
            Timer::after_secs(2).await;

            match dht.read_with_retry().await {
                Ok((temperature, humidity)) => {
                    buf.clear();
                    write!(buf, "T: {}C, H: {}%    ", temperature, humidity).unwrap();
                    /*
                    info!("Main: Sending {} into display", buf);
                    let _ = sender.send(DisplayMessage {
                        text: buf.clone(),
                        position: Point::new(2, DisplayLine::LINE4)
                    }).await;
                    */
                    match client.send_message(
                        "ESP32", // channel
                        buf.as_bytes(), // message
                        rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS1, // qos
                        true // retain
                    ).await {
                        Ok(_) => {},
                        Err(mqtt_error) => match mqtt_error {
                            ReasonCode::NetworkError => {
                                error!("MQTT network error");
                            },
                            _ => {
                                error!("Other MQTT error {:?}", mqtt_error);
                            }
                        }
                    }
                },
                Err(error) => error!("Failed to read from DHT: {:?}", error),
            }
        }
    }
}


#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    info!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_wifi::wifi::wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after_millis(5000).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi");
            controller.start_async().await.unwrap();
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => info!("Wifi connected!"),
            Err(e) => {
                info!("Failed to connect to wifi: {e:?}");
                Timer::after_millis(5000).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) {
    runner.run().await
}