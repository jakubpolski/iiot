#![deny(unused_must_use)]

use embassy_time::{Instant, Timer};
use esp_hal::{delay::Delay, gpio::{Flex, Level, Pull}};
use log::error;

// enum containing dht responses
#[derive(Debug)]
pub enum DhtError {
    NoResponse,
    InvalidResponse,
    ChecksumMismatch
}


const DHT_BUFFER_SIZE: usize = 5;

// dht struct contains the pin it uses, buffer for data stored from sensors
// and a delay to use inside the critical section
pub struct Dht11<'a> {
    pin: Flex<'a>,
    buffer: [u8; DHT_BUFFER_SIZE],
    delay: Delay,
}

impl<'a> Dht11<'a> {
    pub fn new(pin: Flex<'a>) -> Self {
        Self { pin, buffer: [0u8; DHT_BUFFER_SIZE], delay: Delay::new() }
    }

    pub async fn read(&mut self) -> Result<(u8, u8), DhtError> {
        // clear the buffer
        self.buffer.fill(0);

        // sensor reset (at least 18ms) is not as time sensitive to be in a critical section
        self.pin.set_as_output();
        self.pin.set_low();
        Timer::after_millis(20).await;

        // time sensitive operations
        let result: Result<(), DhtError> = critical_section::with(|_| {
            // look for sensor response (should take 20-40us)
            self.pin.set_high();
            self.pin.set_as_input(Pull::None);
            if !self.wait_for_level(Level::Low, 50) {
                return Err(DhtError::NoResponse);
            }
            // look for response finish (should take 80us)
            if !self.wait_for_level(Level::High, 100) {
                return Err(DhtError::InvalidResponse);
            }
            // wait until dht is ready for transmitting data (should take 80us)
            if !self.wait_for_level(Level::Low, 100) {
                return Err(DhtError::InvalidResponse);
            }
            // read 5 bytes
            for byte in 0..DHT_BUFFER_SIZE {
                // for each bit (minding the ordering)
                for bit in (0..8).rev() {
                    // low level between data signals should take around 50us
            if !self.wait_for_level(Level::High, 60){
                return Err(DhtError::InvalidResponse);
            }
            // reading signal length
            let high_time = self.measure_high_pulse(100);
            // high signal length around 26-28us means 0, around 70us means 1
            if high_time > 40 {
                self.buffer[byte] |= 1 << bit;
            }
                }
            }
            // sensor pulls down bus's voltage after finishing for 50us
            self.delay.delay_micros(50);
            Ok(())
        });

match result {
    Ok(_) => {
        // test if checksum matches
        if self.buffer[4] != (self.buffer[0] + self.buffer[1] + self.buffer[2] + self.buffer[3]) {
            Err(DhtError::ChecksumMismatch)
        // dht11 does not support decimal places, so only two bytes are returned
        } else {
            Ok((self.buffer[2], self.buffer[0])) 
        }
    },
    Err(error) => Err(error)
}
    }

    // try to read from dht 3 times
    pub async fn read_with_retry(&mut self, retry_count: u8) -> Result<(u8, u8), DhtError> {
        let mut last_error: DhtError = DhtError::NoResponse;
        for i in 0..retry_count {
            match self.read().await {
                Ok(v) => return Ok(v),
                Err(error) => {
                    error!("Retry: {}, error: {:?}", i+1, error);
                    last_error = error;
                    // wait 100ms before retrying
                    Timer::after_millis(100).await;
                }
            }
        }
        Err(last_error)
    }

    // waiting for desired output level with a timeout
    fn wait_for_level(&mut self, level: Level, timeout_us: u64) -> bool {
        let start = Instant::now();
        while self.pin.level() != level {
            if start.elapsed().as_micros() > timeout_us {
                return false;
            }
        }
        true
    } 
    
    // measuring high pulse length in microseconds (signal has to be high by the time this function starts)
    fn measure_high_pulse(&mut self, timeout_us: u64) -> u32 {
        let start = Instant::now();
        let mut length = start.elapsed().as_micros();
        // saving elapsed time until pin goes low, or times out at 100us
        while self.pin.is_high() {
            // save elapsed time since function got called
            length = start.elapsed().as_micros();
            // timeout after 100us
            if length > timeout_us {
                break;
            }
        }
        length as u32
    }
 }