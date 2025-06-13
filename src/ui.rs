#![deny(unused_must_use)]

use core::{marker::PhantomData, sync::atomic::{AtomicBool, AtomicU8, Ordering}};

use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::{Channel, Sender},
};
use embassy_time::Instant;
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle, MonoTextStyleBuilder},
    pixelcolor::BinaryColor, prelude::*, text::{Baseline, Text}
};

extern crate alloc;
use heapless::String;
use alloc::fmt::Write;
use log::{error, info};

use crate::{dht11::Dht11, mqtt::{MqttMessage, MqttResponse}, ButtonType, GraphicsDisplay, SensorMessage};


pub static BUTTON_CHANNEL: Channel<CriticalSectionRawMutex, ButtonType, 10> = Channel::new();
pub static SENSOR_CHANNEL: Channel<CriticalSectionRawMutex, SensorMessage, 1> = Channel::new();

const GENERIC_MIN_READ_DELAY: u8 = 2;
const GENERIC_MAX_READ_DELAY: u8 = 30;

pub static MOTION_READ_DELAY: AtomicU8 = AtomicU8::new(2);
pub static CONTACT_READ_DELAY: AtomicU8 = AtomicU8::new(2);

pub static MOTION_ENABLED: AtomicBool = AtomicBool::new(true);
pub static CONTACT_ENABLED: AtomicBool = AtomicBool::new(true);
pub static MQTT_ENABLED: AtomicBool = AtomicBool::new(true);


// adding labels to display lines for convenience
pub struct DisplayLine;
impl DisplayLine {
    pub const LINE1:i32 = 0;
    pub const LINE2:i32 = 12;
    pub const LINE3:i32 = 24;
    pub const LINE4:i32 = 36;
    pub const LINE5:i32 = 48;
}

// adding labels to various indentations
pub const DISPLAY_INDENT: i32 = 2;
pub const MQTT_PROMPT_INDENT: i32 = DISPLAY_INDENT + 14*6;

// consts storing font information for the display
// one for dark background, one for bright background
pub const TEXT_STYLE: MonoTextStyle<'_, BinaryColor> =
    MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .background_color(BinaryColor::Off)
        .build();

const INVERTED_TEXT_STYLE: MonoTextStyle<'_, BinaryColor> =
    MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::Off)
        .background_color(BinaryColor::On)
        .build();


// enum with various convenience functions for each value type (received form sensors)
#[derive(Clone, Copy)]
pub enum ValueType {
    Humidity,
    Temperature,
    Motion,
    Contact,
}

impl ValueType {
    // returns on which topic each value should be sent over mqtt
    pub fn topic(&self) -> &str {
        match self {
            Self::Contact => "esp32/contact",
            Self::Humidity => "esp32/humidity",
            Self::Motion => "esp32/motion",
            Self::Temperature => "esp32/temperature",
        }
    }
    // returns on which line should each value is displayed
    pub fn line(&self) -> i32 {
        match self {
            Self::Temperature => DisplayLine::LINE1,
            Self::Humidity => DisplayLine::LINE2,
            Self::Motion => DisplayLine::LINE3,
            Self::Contact => DisplayLine::LINE4,
        }
    }
    // returns a point on display, where each value should be displayed
    pub fn point(&self) -> Point {
        match self {
            Self::Temperature => Point { x: DISPLAY_INDENT + 6*6 , y: DisplayLine::LINE1 },
            Self::Humidity => Point { x: DISPLAY_INDENT + 10*6, y: DisplayLine::LINE2 },
            Self::Motion => Point { x: DISPLAY_INDENT + 8*6, y: DisplayLine::LINE3 },
            Self::Contact => Point { x: DISPLAY_INDENT + 9*6, y: DisplayLine::LINE4 },
        }
    }
}


// unit structs, for cleaner typing in trackers
struct Dht;
struct Temperature;
struct Humidity;
struct Motion;
struct Contact;
// generic tracker storing information about various events
// when it happened, and whether it was handled
// it uses PhantomData, as topic is only used for typing
struct Tracker<T> {
    _topic: PhantomData<T>,
    pub time: Instant,
    pub handled: bool,
}

impl<T> Tracker<T> {
    // by default, the event is handled (as nothing happened yet)
    pub fn new() -> Self {
        Self { _topic: PhantomData, time: Instant::now(), handled: true }
    }
    // reset the tracker, used when new event arrived
    pub fn reset(&mut self) {
        self.time = Instant::now();
        self.handled = false;
    }
}

// send trackers for each value type
// stores when data was sent
// mainly used for showing mqtt sending progress (sending, sent, error)
struct SendTrackers {
    temperature: Tracker<Temperature>,
    humidity: Tracker<Humidity>,
    motion: Tracker<Motion>,
    contact: Tracker<Contact>
}

impl SendTrackers {
    pub fn new() -> Self {
        Self {
            temperature: Tracker::<Temperature>::new(),
            humidity: Tracker::<Humidity>::new(),
            motion: Tracker::<Motion>::new(),
            contact: Tracker::<Contact>::new(),
        }
    }
}

// read trackers for each value type
// stores when data was read from sensors
// mainly used for blinking over values when they update
struct ReadTrackers {
    dht: Tracker<Dht>,
    motion: Tracker<Motion>,
    contact: Tracker<Contact>,
}

impl ReadTrackers {
    pub fn new() -> Self {
        Self {
            dht: Tracker::<Dht>::new(),
            motion: Tracker::<Motion>::new(),
            contact: Tracker::<Contact>::new(),
        }
    }
}

// struct containing current sensor values
struct CurrentValues {
    pub temperature: u8,
    pub humidity: u8,
    pub motion: u8,
    pub contact: u8,
}

impl CurrentValues {
    // creating the struct with default values
    pub fn new() -> Self {
        Self {
            temperature: 255,
            humidity: 255,
            motion: 0,
            contact: 0,
        }
    }
}

pub struct Ui<'a> {
    pub state: UiState,
    // static buffer, able to store a whole line to be displayed on display
    line_buffer: String<21>,
    display: GraphicsDisplay,
    // stores what is currently selected in setup state
    selection: DelaySelection,
    // handle for sending data over mqtt
    mqtt_sender: Sender<'a, CriticalSectionRawMutex, MqttMessage, 20>,
    // dht variables
    dht: Dht11<'a>,
    dht_delay: u8,
    dht_enabled: bool,
    // trackers
    read_trackers: ReadTrackers,
    send_trackers: SendTrackers,

    current_values: CurrentValues,

}

// macro made to ensure that the buffer is flushed every time new data is put into it
macro_rules! set_buffer {
    ($self:ident, $fmt:literal, $($arg:expr)*) => {{
        $self.line_buffer.clear();
        let _ = write!($self.line_buffer, $fmt, $($arg)*);
    }};
}


impl<'a> Ui<'a> {
    // initiating with default values and those prepared by the main thread
    pub async fn new(display: GraphicsDisplay, dht: Dht11<'a>, mqtt_sender: Sender<'a, CriticalSectionRawMutex, MqttMessage, 20>) -> Self {
        let mut ui = Self {   
            state: UiState::Displaying,
            line_buffer: String::<21>::new(),
            display,
            selection: DelaySelection::DHT,
            mqtt_sender,

            dht,
            dht_delay: 2,
            dht_enabled: true,

            read_trackers: ReadTrackers::new(),
            send_trackers: SendTrackers::new(),
            current_values: CurrentValues::new(),
        };
        // show main screen on display as it's initiated
        ui.redraw().await;
        ui
    }

    // setter for ui state, also redraws the ui, as the state changed
    pub async fn set_state(&mut self, state: UiState) {
        self.state = state;
        self.redraw().await;
    }

    // function to handle updating data in current state
    // used when something changes within the same state
    pub async fn update(&mut self) {
        self.draw_content();
        let _ = self.display.flush().await;
    }

    // handler for a button press event
    pub async fn handle_button_press(&mut self, button: ButtonType) {
        match self.state {
            // go into setup when any button got pressed
            UiState::Displaying => self.set_state(UiState::SelectingDelay).await,
            UiState::SelectingDelay => match button {
                // button A goes back to displaying mode
                ButtonType::A => {
                    self.selection = DelaySelection::DHT;
                    self.set_state(UiState::Displaying).await;
                },
                // button B goes up in selection
                ButtonType::B => {
                    self.clear_arrow();
                    self.selection = self.selection.previous();
                    self.update().await;
                },
                // button C goes down in selection
                ButtonType::C => {
                    self.clear_arrow();
                    self.selection = self.selection.next();
                    self.update().await;
                },
                // button D enters the state, which modifies currently selected delay
                ButtonType::D => self.set_state(UiState::ModifyingDelay).await,
            },
            UiState::ModifyingDelay => match button {
                // button A toggles currently selected object (sensors or mqtt)
                ButtonType::A => self.toggle_selected().await,
                // button B increases the delay for sensors (panics when called for mqtt)
                ButtonType::B => self.increase_delay().await,
                // button C decreases the delay for sensors (panics when called for mqtt)
                ButtonType::C => self.decrease_delay().await,
                // button D goes back to delay selection
                ButtonType::D => self.set_state(UiState::SelectingDelay).await,
            }
        }
    }

    // handler for mqtt response
    pub async fn handle_mqtt_response(&mut self, resp: MqttResponse) {
        match self.state {
            UiState::Displaying => {
                // based on the response, stores what should be displayed 
                match resp.status {
                    Ok(_) => set_buffer!(self, "   Sent",),
                    Err(_) => set_buffer!(self, "  Error",),
                }
                // getting the line number, which gets the display update
                // also resetting the send trackers in the meantime (as mqtt sending was at least attempted)
                let line = match resp.topic {
                    ValueType::Temperature =>  {
                        self.send_trackers.temperature.reset();
                        DisplayLine::LINE1
                    },
                    ValueType::Humidity => {
                        self.send_trackers.humidity.reset();
                        DisplayLine::LINE2
                    },
                    ValueType::Motion => {
                        self.send_trackers.motion.reset();
                        DisplayLine::LINE3
                    },
                    ValueType::Contact => {
                        self.send_trackers.contact.reset();
                        DisplayLine::LINE4
                    },
                };
                // drawing updated values
                self.draw_at(Point { x: MQTT_PROMPT_INDENT, y: line});
                let _ = self.display.flush().await;
            },
            // ui shouldn't listen for mqtt responses outside of displaying state
            _ => panic!("UI: MQTT response received outside of displaying state"),
        }
    }

    // handler for sensor messages
    pub async fn handle_sensor_message(&mut self, msg: SensorMessage) {
        match self.state {
            // check what sensor sent a message
            UiState::Displaying => match msg {
                // for current sensor
                // - send data over mqtt
                // - reset read trackers (as data appeared)
                // - update current value
                // - display value (inverted for blinking)
                SensorMessage::MotionSensor => {
                    self.send(MqttMessage { topic: ValueType::Motion, value: 1 }).await;
                    self.read_trackers.motion.reset();
                    self.current_values.motion = 1;
                    self.draw_inverted_value(ValueType::Motion);
                    let _ = self.display.flush().await;
                },
                SensorMessage::ContactSensor => {
                    self.send(MqttMessage { topic: ValueType::Contact, value: 1 }).await;
                    self.read_trackers.contact.reset();
                    self.current_values.contact = 1;
                    self.draw_inverted_value(ValueType::Contact);
                    let _ = self.display.flush().await;
                    
                },
            }
            // ui should not listen for sensor changes outside of displaying state
            _ => panic!("UI: Sensor message received outside of displaying state"),
        }
    }

    // function used to make ui update internal states
    pub async fn tick(&mut self) {
        // handling dht if it's enabled
        if self.dht_enabled {
            // if time between dht reads passed
            let dht_elapsed = self.read_trackers.dht.time.elapsed().as_secs();
            if dht_elapsed >= self.dht_delay.into() {
                // try to read data
                match self.dht.read_with_retry(3).await {
                    // if data was read
                    Ok((temperature, humidity)) => {
                        // send values over mqtt
                        self.send(MqttMessage { topic: ValueType::Temperature, value: temperature }).await;
                        self.send(MqttMessage { topic: ValueType::Humidity, value: humidity }).await;
                        // reset read trackers as data appeared
                        self.read_trackers.dht.reset();
                        // update current values
                        self.current_values.temperature = temperature;
                        self.current_values.humidity = humidity;
                        // draw them inverted for blinking
                        self.draw_inverted_value(ValueType::Temperature);
                        self.draw_inverted_value(ValueType::Humidity);
                        let _ = self.display.flush().await;
                    },
                    // skip if it failed to read from dht
                    Err(error) => error!("Failed to read from DHT: {:?}", error),
                }
            // if 1 second passed after blinking 
            } else if !self.read_trackers.dht.handled && dht_elapsed >= 1 {
                // set read event as handled
                self.read_trackers.dht.handled = true;
                // remove the blink
                self.draw_value(ValueType::Temperature);
                self.draw_value(ValueType::Humidity);
                let _ = self.display.flush().await;
            }
        }

        // handling the motion sensor if it's enabled
        if MOTION_ENABLED.load(Ordering::Acquire) {
            let read_motion_elapsed = self.read_trackers.motion.time.elapsed().as_secs();
            let read_delay: u64 = MOTION_READ_DELAY.load(Ordering::Acquire).into();
            // if last read was longer than minimum time between reads
            if read_motion_elapsed >= read_delay + 1 {
                // assume that there is no motion
                // send no motion to mqtt
                self.send(MqttMessage { topic: ValueType::Motion, value: 0 }).await;
                // reset read trackers
                self.read_trackers.motion.reset();
                // update current values
                self.current_values.motion = 0;
                // draw inverted for blinking
                self.draw_inverted_value(ValueType::Motion);
                let _ = self.display.flush().await;
            // if the read tracker is not handled, and blink lasted for at least a second
            } else if !self.read_trackers.motion.handled && read_motion_elapsed >= 1 {
                // set as handled
                self.read_trackers.motion.handled = true;
                // remove the blink
                self.draw_value(ValueType::Motion);
                let _ = self.display.flush().await;
            }
        }
        // handling the contact sensor if it's enabled
        if CONTACT_ENABLED.load(Ordering::Acquire) {
            let read_contact_elapsed = self.read_trackers.contact.time.elapsed().as_secs();
            let read_delay: u64 = CONTACT_READ_DELAY.load(Ordering::Acquire).into();
            // if last read was longer than minimum time between reads
            if read_contact_elapsed >= read_delay + 1 {
                // assume that there is no contact
                // send no contact to mqtt
                self.send(MqttMessage { topic: ValueType::Contact, value: 0 }).await;
                // reset read trackers
                self.read_trackers.contact.reset();
                // update current values
                self.current_values.contact = 0;
                // draw inverted for blinking
                self.draw_inverted_value(ValueType::Contact);
                let _ = self.display.flush().await;
            // if the read tracker is not handled, and blink lasted for at least a second
            } else if !self.read_trackers.contact.handled && read_contact_elapsed >= 1 {
                // set as handled
                self.read_trackers.contact.handled = true;
                // remove the blink
                self.draw_value(ValueType::Contact);
                let _ = self.display.flush().await;
            }
        }
        // for each of the send trackers
        // if they are not handled yet, and the message was displayed for at least a second:
        // - set as handled
        // - clear the message from the display
        let send_temp_elapsed = self.send_trackers.temperature.time.elapsed().as_secs();
        if !self.send_trackers.temperature.handled && send_temp_elapsed >= 1 {
            self.send_trackers.temperature.handled = true;
            self.clear_mqtt_message(ValueType::Temperature).await;
        }
        let send_humidity_elapsed = self.send_trackers.humidity.time.elapsed().as_secs();
        if !self.send_trackers.humidity.handled && send_humidity_elapsed >= 1 {
            self.send_trackers.humidity.handled = true;
            self.clear_mqtt_message(ValueType::Humidity).await;
        }
        let send_motion_elapsed = self.send_trackers.motion.time.elapsed().as_secs();
        if !self.send_trackers.motion.handled && send_motion_elapsed >= 1 {
            self.send_trackers.motion.handled = true;
            self.clear_mqtt_message(ValueType::Motion).await;
        }
        let send_contact_elapsed = self.send_trackers.contact.time.elapsed().as_secs();
        if !self.send_trackers.contact.handled && send_contact_elapsed >= 1 {
            self.send_trackers.contact.handled = true;
            self.clear_mqtt_message(ValueType::Contact).await;
        }
    }

    // redraws the whole ui for current state
    async fn redraw(&mut self) {
        if self.state != UiState::ModifyingDelay {
            self.display.clear();
        }
        self.draw_content();

        let bottom_line = match self.state {
            UiState::Displaying => "[Delays]",
            UiState::SelectingDelay => "[<-]  [^]   [v]   [S]",
            UiState::ModifyingDelay => "[T]   [+]   [-]   [U]",
        };

        Text::with_baseline(
            bottom_line,
        Point { x: DISPLAY_INDENT, y: DisplayLine::LINE5 },
        TEXT_STYLE,
            Baseline::Top
        ).draw(&mut self.display).unwrap();

        let _ = self.display.flush().await;
    }


    fn draw_content(&mut self) {
        match self.state {
            UiState::Displaying => {
                match self.dht_enabled {
                    true => {     
                        if self.current_values.temperature == 255 {
                            set_buffer!(self, "Temp: ???",);
                        } else {
                            set_buffer!(self, "Temp: {:>2}C", self.current_values.temperature);
                        }
                        self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE1});
   
                        if self.current_values.humidity == 255 {
                            set_buffer!(self, "Humidity: ???",);
                        } else {
                            set_buffer!(self, "Humidity: {:>2}%", self.current_values.humidity);
                        }
                        self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE2});
                    },
                    false => {
                        set_buffer!(self, "Temp: OFF",);
                        self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE1});
                        set_buffer!(self, "Humidity: OFF",);
                        self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE2});
                    }
                }

                match MOTION_ENABLED.load(Ordering::Acquire) {
                    true => {
                        if self.current_values.motion == 255 {
                            set_buffer!(self, "Motion: ???",);
                        } else {
                            set_buffer!(self, "Motion: {}", if self.current_values.motion != 0 { "YES" } else { " NO" });
                            
                        }
                    },
                    false => {
                        set_buffer!(self, "Motion: OFF",);
                    }
                }
                self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE3});

                match CONTACT_ENABLED.load(Ordering::Acquire) {
                    true => {
                        if self.current_values.contact == 255 {
                            set_buffer!(self, "Contact: ???",);
                        } else {
                            set_buffer!(self, "Contact: {:>2}", if self.current_values.contact != 0 { "YES" } else { " NO" });
                            
                        }
                    },
                    false => {
                        set_buffer!(self, "Contact: OFF",);
                    }
                }
                self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE4});
            },
            UiState::SelectingDelay => {
                match self.dht_enabled {
                    true => set_buffer!(self, "DHT:     {:>2}s", self.dht_delay),
                    false => set_buffer!(self, "DHT:     OFF",),
                }
                self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE1});

                match MOTION_ENABLED.load(Ordering::Acquire) {
                    true  => set_buffer!(self, "Motion:  {:>2}s", MOTION_READ_DELAY.load(Ordering::Acquire)),
                    false => set_buffer!(self, "Motion:  OFF",),
                }
                self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE2});
                

                match CONTACT_ENABLED.load(Ordering::Acquire) {
                    true  => set_buffer!(self, "Contact: {:>2}s", CONTACT_READ_DELAY.load(Ordering::Acquire)),
                    false => set_buffer!(self, "Contact: OFF",),
                }
                self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE3});

                match MQTT_ENABLED.load(Ordering::Acquire) {
                    true  => set_buffer!(self, "MQTT:     ON",),
                    false => set_buffer!(self, "MQTT:    OFF",),
                }
                self.draw_at(Point { x: DISPLAY_INDENT, y: DisplayLine::LINE4});

                self.draw_arrow();
            },
            UiState::ModifyingDelay => {
                if self.selection == DelaySelection::MQTT {
                    set_buffer!(self, "{}", if self.is_selection_enabled() { " ON" } else { "OFF" })
                }
                else if self.is_selection_enabled() {
                    set_buffer!(self, "{:>2}s", self.get_selected_delay())
                } else {
                    set_buffer!(self, "OFF",)
                };
                self.draw_inverted_at(Point { x: DISPLAY_INDENT + 9*6, y: self.selection.line() });
            }
        }
    }

    fn get_selected_delay(&self) -> u8 {
        match self.selection {
            DelaySelection::DHT => self.dht_delay,
            DelaySelection::Contact => CONTACT_READ_DELAY.load(Ordering::Acquire),
            DelaySelection::Motion => MOTION_READ_DELAY.load(Ordering::Acquire),
            DelaySelection::MQTT => panic!("UI: Tried to read non-existing MQTT delay"),
        }
    }

    fn store_selected_delay(&mut self, val: u8) {
        match self.selection {
            DelaySelection::DHT => self.dht_delay = val,
            DelaySelection::Contact => CONTACT_READ_DELAY.store(val, Ordering::Release),
            DelaySelection::Motion => MOTION_READ_DELAY.store(val, Ordering::Release),
            DelaySelection::MQTT => panic!("UI: Tried to set non-existing MQTT delay"),
        };
    }

    async fn toggle_selected(&mut self) {
        match self.selection {
            DelaySelection::DHT => self.dht_enabled = !self.dht_enabled,
            DelaySelection::Contact => _ = CONTACT_ENABLED.fetch_not(Ordering::AcqRel),
            DelaySelection::Motion => _ = MOTION_ENABLED.fetch_not(Ordering::AcqRel),
            DelaySelection::MQTT =>  _ = MQTT_ENABLED.fetch_not(Ordering::AcqRel),
        };
        self.update().await;
    }

    fn is_selection_enabled(&self) -> bool {
        match self.selection {
            DelaySelection::DHT => self.dht_enabled,
            DelaySelection::Contact => CONTACT_ENABLED.load(Ordering::Acquire),
            DelaySelection::Motion => MOTION_ENABLED.load(Ordering::Acquire),
            DelaySelection::MQTT => MQTT_ENABLED.load(Ordering::Acquire),
        }
    }

    fn is_enabled(&self, value_type: ValueType) -> bool {
        match value_type {
            ValueType::Temperature | ValueType::Humidity => self.dht_enabled,
            ValueType::Contact => CONTACT_ENABLED.load(Ordering::Acquire),
            ValueType::Motion => MOTION_ENABLED.load(Ordering::Acquire),
        }
    }

    fn draw_arrow(&mut self) {
        let _ = Text::with_baseline(
            "<------", 
            Point { x: DISPLAY_INDENT + 13*6, y: self.selection.line() }, 
            TEXT_STYLE, 
            Baseline::Top
        ).draw(&mut self.display);
    }

    fn clear_arrow(&mut self) {
        let _ = Text::with_baseline(
            "       ", 
            Point { x: DISPLAY_INDENT + 13*6, y: self.selection.line() }, 
            TEXT_STYLE, 
            Baseline::Top
        ).draw(&mut self.display);
    }

    fn draw_at(&mut self, point: Point) {
        let _ = Text::with_baseline(
            &self.line_buffer, 
            point, 
            TEXT_STYLE, 
            Baseline::Top
        ).draw(&mut self.display);
    }

    fn draw_inverted_at(&mut self, point: Point) {
        let _ = Text::with_baseline(
            &self.line_buffer, 
            point, 
            INVERTED_TEXT_STYLE, 
            Baseline::Top
        ).draw(&mut self.display);
    }


    fn draw_value(&mut self, value_type: ValueType) {
        if !self.is_enabled(value_type) {
            return;
        }
        match value_type {
            ValueType::Temperature => set_buffer!(self, "{:>2}C", self.current_values.temperature),
            ValueType::Humidity => set_buffer!(self, "{:>2}%", self.current_values.humidity),
            ValueType::Motion => set_buffer!(self, "{}", if self.current_values.motion != 0 { "YES" } else { " NO" }),
            ValueType::Contact => set_buffer!(self, "{}", if self.current_values.contact != 0 { "YES" } else { " NO" }),
        }
        info!("Drawing {} at point {:?}", self.line_buffer, value_type.point());
        self.draw_at(value_type.point());
    }

    fn draw_inverted_value(&mut self, value_type: ValueType) {
        if !self.is_enabled(value_type) {
            return;
        }
        match value_type {
            ValueType::Temperature => set_buffer!(self, "{:>2}C", self.current_values.temperature),
            ValueType::Humidity => set_buffer!(self, "{:>2}%", self.current_values.humidity),
            ValueType::Motion => set_buffer!(self, "{}", if self.current_values.motion != 0 { "YES" } else { " NO" }),
            ValueType::Contact => set_buffer!(self, "{}", if self.current_values.contact != 0  { "YES" } else { " NO" }),
        };
        info!("Drawing inverted {} at point {:?}", self.line_buffer, value_type.point());
        self.draw_inverted_at(value_type.point());
    }

    async fn clear_mqtt_message(&mut self, value_type: ValueType) {
        let _ = Text::with_baseline(
            "       ",
            Point { x: 14*6, y: value_type.line() },
            TEXT_STYLE,
            Baseline::Top
        ).draw(&mut self.display);
        let _ = self.display.flush().await;
    }

    async fn send(&mut self, msg: MqttMessage) {
        if MQTT_ENABLED.load(Ordering::Acquire) {
            set_buffer!(self, "Sending",);
            self.draw_at(Point { x: MQTT_PROMPT_INDENT, y: msg.topic.line() });
            let _ = self.display.flush().await;
            self.mqtt_sender.send(msg).await;
        }
    }

    async fn decrease_delay(&mut self) {
        if self.is_selection_enabled() && self.selection != DelaySelection::MQTT {
            let mut delay = self.get_selected_delay();
            if delay > GENERIC_MIN_READ_DELAY {
                delay -= 1;
                self.store_selected_delay(delay);
            }
        }
        self.update().await;
    }

    async fn increase_delay(&mut self) {
        if self.is_selection_enabled() && self.selection != DelaySelection::MQTT {
            let mut delay = self.get_selected_delay();
            if delay < GENERIC_MAX_READ_DELAY {
                delay += 1;
                self.store_selected_delay(delay);
            }
        }
        self.update().await;
    } 

}

#[derive(PartialEq)]
pub enum UiState {
    SelectingDelay,
    ModifyingDelay,
    Displaying,
}

#[derive(PartialEq)]
enum DelaySelection {
    DHT,
    Motion,
    Contact,
    MQTT
}

impl DelaySelection {
    pub fn previous(&self) -> Self {
        match self {
            Self::DHT => Self::DHT,
            Self::Motion => Self::DHT,
            Self::Contact => Self::Motion,
            Self::MQTT => Self::Contact,
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::DHT => Self::Motion,
            Self::Motion => Self::Contact,
            Self::Contact => Self::MQTT,
            Self::MQTT => Self::MQTT,
        }
    }

    pub fn line(&self) -> i32 {
        match self {
            Self::DHT => DisplayLine::LINE1,
            Self::Motion => DisplayLine::LINE2,
            Self::Contact => DisplayLine::LINE3,
            Self::MQTT => DisplayLine::LINE4,
        }
    }
}

