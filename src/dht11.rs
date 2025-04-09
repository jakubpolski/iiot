use embassy_time::{Instant, Timer};
use esp_hal::{delay::Delay, gpio::{Flex, Level, Pull}};

#[derive(Debug)]
pub enum DhtError {
    NoResponse,
    InvalidResponse,
    ChecksumMismatch
}

const DHT_BUFFER_SIZE: usize = 5;

pub struct Dht11<'a>
where 
{
    pin: Flex<'a>,
    buffer: [u8; DHT_BUFFER_SIZE],
    delay: Delay
}

impl<'a> Dht11<'a>
where
{
    pub fn new(pin: Flex<'a>) -> Self {
        Self { pin, buffer: [0u8; DHT_BUFFER_SIZE], delay: Delay::new() }
    }

    pub async fn read(&mut self) -> Result<(u8, u8), DhtError> {
        // clear the buffer
        for i in 0..DHT_BUFFER_SIZE { self.buffer[i] = 0; }
        // sensor reset is not as time sensitive to be in a critical section
        self.pin.set_as_output();
        self.pin.set_low();
        Timer::after_millis(18).await;
        // time sensitive operations
        let result = critical_section::with(|_| {
            // finish start signal
            self.pin.set_high();
            self.delay.delay_micros(40);
            
            // look for dht11 response
            self.pin.set_as_input(Pull::None);
            if !self.wait_for_level(Level::Low, 100) {
                return Err(DhtError::NoResponse);
            }
            if !self.wait_for_level(Level::High, 100) {
                return Err(DhtError::InvalidResponse);
            }
            // read 5 bytes
            for byte in 0..5 {
                for bit in (0..8).rev() {
                    // wait until dht11 starts to send
                    if !self.wait_for_level(Level::Low,100) {
                        return Err(DhtError::NoResponse);
                    }
                    if !self.wait_for_level(Level::High, 100){
                        return Err(DhtError::InvalidResponse);
                    }
                    // read data
                    let high_time = self.measure_high_pulse();
                    if high_time > 40 {
                        self.buffer[byte] |= 1 << bit;
                    }
                }
            }
            Ok(())
        });

        match result {
            Ok(_) => {
                if self.buffer[4] != (self.buffer[0] + self.buffer[1] + self.buffer[2] + self.buffer[3]) {
                    Err(DhtError::ChecksumMismatch)
                } else {
                    Ok((self.buffer[2], self.buffer[0])) 
                }
            },
            Err(error) => Err(error)
        }
    }

    pub async fn read_with_retry(&mut self) -> Result<(u8, u8), DhtError> {
        let mut last_error: DhtError = DhtError::NoResponse;
        for i in 0..3 {
            match self.read().await {
                Ok(v) => return Ok(v),
                Err(error) => {
                    last_error = error;
                    Timer::after_millis(100).await;
                }
            }
        }
        Err(last_error)
    }

    fn wait_for_level(&mut self, level: Level, timeout_us: u64) -> bool {
        let start = Instant::now();
        while self.pin.level() != level {
            if start.elapsed().as_micros() > timeout_us {
                return false;
            }
            // not sure how precise the delays are so i just delay multiple times
            // for less time and measure with Instant
            self.delay.delay_nanos(300);
        }
        true
    } 
    
    fn measure_high_pulse(&mut self) -> u32 {
        let start = Instant::now();
        while self.pin.is_high() {
            if start.elapsed().as_micros() > 100 {
                break;
            }
            self.delay.delay_nanos(300);
        }
        let length = start.elapsed().as_micros() as u32;
        length
    }
 }