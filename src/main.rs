#![no_std]
#![no_main]
#![deny(unused_must_use)]

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

use esp_backtrace as _;
use log::{info, error};

use esp_alloc as _;
extern crate alloc;
use heapless::String;

use core::sync::atomic::Ordering;

// display
use oled_async::{prelude::*, Builder};
use embedded_graphics::{prelude::*, text::{Baseline, Text}};
// types for convenience
type DisplayInterface = display_interface_i2c::I2CInterface<I2c<'static, Async>>;
type DisplayType = oled_async::displays::ssd1309::Ssd1309_128_64;
type GraphicsDisplay = GraphicsMode<DisplayType, DisplayInterface>;

use embassy_executor::Spawner;
use embassy_futures::select::{select4, Either4};
use embassy_net::{Runner, StackResources};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_hal::{
    clock::CpuClock,
    gpio::{Flex, Input, Pull},
    i2c::master::{Config, I2c},
    rng::Rng,
    timer::timg::TimerGroup,
    Async
};

mod dht11;
use dht11::Dht11;

mod mqtt;
use mqtt::{Mqtt, MqttResponse, MQTT_CMD_CHANNEL, MQTT_RESP_CHANNEL};
use rust_mqtt::packet::v5::publish_packet::QualityOfService;

mod ui;
use ui::{DisplayLine, Ui, UiState, BUTTON_CHANNEL, CONTACT_ENABLED, CONTACT_READ_DELAY, DISPLAY_INDENT, MOTION_ENABLED, MOTION_READ_DELAY, MQTT_ENABLED, TEXT_STYLE, SENSOR_CHANNEL};

use esp_wifi::{
    init,
    wifi::{ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice, WifiState},
    EspWifiController
};

// enum showing which button was pressed
enum ButtonType {
    A,
    B,
    C,
    D,
}

const DEBOUNCE_TIME: Duration = Duration::from_millis(20);
// reusing the button task in a macro, as all behave the same, 
// but each need a seperate task, which passes a different enum variant
macro_rules! button_task {
    // function name and button type
    ($fname:ident, $btype:expr) => {
        #[embassy_executor::task]
        async fn $fname (mut pin: Input<'static>) {
            // handle for sending data over the button channel
            let sender = BUTTON_CHANNEL.sender();
            loop {
                // wait until button gets pressed
                pin.wait_for_falling_edge().await;
                // handle debouncing, skip if press was shorter than debounce time
                Timer::after(DEBOUNCE_TIME).await;
                if pin.is_high() { continue; }
                // otherwise send a message to the button channel
                sender.send($btype).await;
            }
        }
    };
}
button_task!(a_button_task, ButtonType::A);
button_task!(b_button_task, ButtonType::B);
button_task!(c_button_task, ButtonType::C);
button_task!(d_button_task, ButtonType::D);


// enum showing which sensor sent an alert
enum SensorMessage {
    MotionSensor,
    ContactSensor
}

// motion sensor task
#[embassy_executor::task]
async fn motion_task(mut pin: Input<'static>) {
    // handle for sending data over the sensor channel
    let sender = SENSOR_CHANNEL.sender();
    loop {
        // polling every 1s until motion sensor gets enabled in the UI
        if !MOTION_ENABLED.load(Ordering::Acquire) {
            Timer::after_secs(1).await;
            continue;
        }
        // waiting for alert with debouncing
        pin.wait_for_high().await;
        Timer::after(DEBOUNCE_TIME).await;
        if pin.is_low() { continue; }
        // send to channel if alert is correct
        sender.send(SensorMessage::MotionSensor).await;
        // MOTION_READ_DELAY controls minimum delay between alerts
        // loading delay from an atomic variable, and sleeping for that delay time
        Timer::after_secs(MOTION_READ_DELAY.load(Ordering::Acquire).into()).await;
    }
} 

// contact sensor task
#[embassy_executor::task]
async fn contact_task(mut pin: Input<'static>) {
    // handle for sending data over the sensor channel
    let sender = SENSOR_CHANNEL.sender();
    loop {
        // polling every 1s until contact sensor gets enabled in the UI
        if !CONTACT_ENABLED.load(Ordering::Acquire) {
            Timer::after_secs(1).await;
            continue;
        }
        // waiting for alert with debouncing
        pin.wait_for_high().await;
        Timer::after(DEBOUNCE_TIME).await;
        if pin.is_low() { continue; }
        // send to channel if alert is correct
        sender.send(SensorMessage::ContactSensor).await;
        // CONTACT_READ_DELAY controls minimum delay between alerts
        // loading delay from an atomic variable, and sleeping for that delay time
        Timer::after_secs(CONTACT_READ_DELAY.load(Ordering::Acquire).into()).await;
    }
} 

#[embassy_executor::task]
async fn mqtt_task(mqtt: &'static mut Mqtt<'static>) {
    info!("mqtt_task begins");
    let command_receiver = MQTT_CMD_CHANNEL.receiver();
    let response_sender = MQTT_RESP_CHANNEL.sender();
    let mut payload_buffer = String::<3>::new(); // passed values are u8, so 0-255, 3 characters at most
    let mut should_reconnect = true;
    let mut cached_message = None;
    let mut already_sent_error = false;
    let mut last_ping = Instant::now();

    loop {
        Timer::after_millis(500).await;
        // reconnection stage
        if should_reconnect {
            match mqtt.connect().await {
                Ok(_) => {
                    should_reconnect = false;
                    last_ping = Instant::now();
                },
                Err(err) => {
                    error!("Unable to connect - {:?}, retrying", err);
                    continue;
                }
            }
        }
        // pinging stage
        if last_ping.elapsed().as_secs() >= 5 {
            info!("MQTT: Sending keep-alive ping");
            if let Some(client) = &mut mqtt.client {
                match client.send_ping().await {
                    Ok(()) => last_ping = Instant::now(),
                    Err(reason) => {
                        should_reconnect = true;
                        error!("Ping failed - {:?} - reconnecting", reason);
                        continue;
                    }
                }
            } 
        }
        // checking if mqtt is enabled before receiving from queue
        if !MQTT_ENABLED.load(Ordering::Acquire) {
            continue;
        }
        // queue flushing loop
        // trying to take the cached message, or awaiting one from the channel for 5 seconds
        while let Ok(msg) = match cached_message.take() {
            Some(msg) => Ok(msg),
            None => with_timeout(Duration::from_secs(5), command_receiver.receive()).await, 
        } {
            // this should never panic because safety is guaranteed inside mqtt struct
            let client = mqtt.client.as_mut().expect("Client uninitialized");
            info!("MQTT: Sending message, queue length: {}", command_receiver.len());
            // sending the message
            match client.send_message(
                msg.topic(),
                msg.payload(&mut payload_buffer),
                QualityOfService::QoS1,
                false
            ).await {
                Ok(_) => {
                    // respond as sending succeeded
                    already_sent_error = false;
                    response_sender.send(MqttResponse { status: Ok(()), topic: msg.topic }).await;
                    last_ping = Instant::now();
                },
                Err(_) => {
                    // error is sent to ui at most once per message to avoid flooding the channel
                    if !already_sent_error{
                        already_sent_error = true;
                        response_sender.send(MqttResponse { status: Err(()), topic: msg.topic }).await;
                    }
                    error!("MQTT: Sending to {} failed, caching the message and reconnecting...", msg.topic());
                    cached_message = Some(msg);
                    should_reconnect = true;
                    break;
                }
            }
            // letting other tasks run while flushing the queue
            Timer::after_millis(10).await;
        }
    }
}


const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

#[esp_hal_embassy::main]
async fn iiot(spawner: Spawner) -> ! {
    // board configuratuion (copied from template)
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    // heap used by runtime tasks (for example wifi)
    esp_alloc::heap_allocator!(72 * 1024);

    // embassy setup for async tasks (copied from template)
    info!("Initializing embassy");
    let timg1 = TimerGroup::new(peripherals.TIMG1);
    esp_hal_embassy::init(timg1.timer0);
    info!("Embassy initialized");

    // setting up async I2C for the OLED display
    info!("Initializing I2C");
    let i2c = I2c::new(peripherals.I2C0, Config::default())
        .unwrap()
        .with_scl(peripherals.GPIO22)
        .with_sda(peripherals.GPIO21)
        .into_async();
    info!("I2C initialized");

    // connecting the display to I2C
    info!("Initializing the display");
let di = DisplayInterface::new(i2c, 0x3C, 0x40);
let mut display: GraphicsDisplay = Builder::new(DisplayType{})
    .with_rotation(crate::DisplayRotation::Rotate180)
    .connect(di)
    .into();
display.init().await.unwrap();
    info!("Display initialized");

    // display message until wifi connects
    display.clear();
    let _ = Text::with_baseline(
        "Waiting for wifi...", 
        Point { x: DISPLAY_INDENT, y: DisplayLine::LINE1 }, 
        TEXT_STYLE, 
        Baseline::Top
    ).draw(&mut display);
    display.flush().await.unwrap();

    // wifi setup (template configuration to use DHCP)
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
    // starting network tasks
    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    // waiting until network card starts
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after_millis(500).await;
    }
    // display message until IP gets assigned
    display.clear();
    let _ = Text::with_baseline(
        "Waiting for IP...", 
        Point { x: DISPLAY_INDENT, y: DisplayLine::LINE1 }, 
        TEXT_STYLE, 
        Baseline::Top
    ).draw(&mut display);
    display.flush().await.unwrap();

    // waiting until ip gets allocated from DHCP (copied from template)
    info!("Waiting to get the IP address");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after_millis(500).await;
    }
    info!("Wifi initialized");
    
    // display message until peripherals get initialized
    display.clear();
    let _ = Text::with_baseline(
        "Peripheral setup...", 
        Point { x: DISPLAY_INDENT, y: DisplayLine::LINE1 }, 
        TEXT_STYLE, 
        Baseline::Top
    ).draw(&mut display);
    display.flush().await.unwrap();

    // setting up peripherals
    info!("Initializing DHT11");
    let dht_pin = Flex::new(peripherals.GPIO18);
    let mut dht = Dht11::new(dht_pin);
    let _ = dht.read().await; // dummy read for initialization
    info!("DHT11 initialized!");

    // input pins for buttons with internal pullup
    let a_button_pin = Input::new(
        peripherals.GPIO26,
        Pull::Up,
    );
    
    let b_button_pin = Input::new(
        peripherals.GPIO27,
        Pull::Up,
    );

    let c_button_pin = Input::new(
        peripherals.GPIO14,
        Pull::Up,
    );

    let d_button_pin = Input::new(
        peripherals.GPIO12,
        Pull::Up,
    );

    // input pins for sensors with external pullup
    let contact_sensor_pin = Input::new(
        peripherals.GPIO19,
        Pull::None,
    );

    let motion_sensor_pin = Input::new(
        peripherals.GPIO17,
        Pull::None,
    );

    info!("Starting background tasks");
    // buttons and sensors
    let _ = spawner.spawn(a_button_task(a_button_pin));
    let _ = spawner.spawn(b_button_task(b_button_pin));
    let _ = spawner.spawn(c_button_task(c_button_pin));
    let _ = spawner.spawn(d_button_task(d_button_pin));
    let _ = spawner.spawn(motion_task(motion_sensor_pin));
    let _ = spawner.spawn(contact_task(contact_sensor_pin));

    // MQTT initialization
    let mqtt = mk_static!(Mqtt, Mqtt::new(stack));
    let _ = spawner.spawn(mqtt_task(mqtt));
    info!("Background tasks started");
    
    // UI setup
    let mut ui = Ui::new(
        display,
        dht,
        MQTT_CMD_CHANNEL.sender(),
    ).await;

    // UI event receivers
    let sensor_receiver = SENSOR_CHANNEL.receiver();
    let mqtt_receiver = MQTT_RESP_CHANNEL.receiver();
    let button_receiver = BUTTON_CHANNEL.receiver();
    // UI loop
    loop {
        match ui.state {
            // in displaying state it listens for any receiver or polls every 100ms for internal updates
            UiState::Displaying => match select4(button_receiver.receive(), sensor_receiver.receive(), mqtt_receiver.receive(), Timer::after_millis(100)).await {
                Either4::First(button) => ui.handle_button_press(button).await,
                Either4::Second(msg) => ui.handle_sensor_message(msg).await,
                Either4::Third(resp) => ui.handle_mqtt_response(resp).await,
                Either4::Fourth(_) => ui.tick().await,
            }
            // in setup state it only listens for buttons (so it does not drop any signals from sensors and mqtt)
            UiState::SelectingDelay | UiState::ModifyingDelay => {
                let button = button_receiver.receive().await;
                ui.handle_button_press(button).await;
            }
        }
    }
}

// default wifi tasks copied from template
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
                error!("Failed to connect to wifi: {e:?}");
                Timer::after_millis(5000).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) {
    runner.run().await
}