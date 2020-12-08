#![feature(proc_macro_hygiene, decl_macro)]
#![allow(non_snake_case)]

#[macro_use]
extern crate rocket;

use rocket::{
    config::{Config, Environment, LoggingLevel},
};

use serde::Serialize;
use serde_json;

use rocket_contrib::serve::StaticFiles;

use std::{convert::TryInto, io, time::{Duration, Instant}};

use local_ipaddress;
use serialport::{self, SerialPortType};

// Bits for serial communication with a PC over USB.
// Copy+pasted from Water Monitor main file.
const SUCCESS_MSG: [u8; 3] = [50, 50, 50]; // Send this to indicate an error.
const ERROR_MSG: [u8; 3] = [99, 99, 99]; // Send this to indicate an error.
const MSG_START_BITS: [u8; 2] = [100, 150];
const MSG_END_BITS: [u8; 1] = [200];
const OK_BIT: u8 = 10;
const ERROR_BIT: u8 = 20;

const REFRESH_INTERVAL: u32 = 1_000;  // Time between querying the WM for readings in ms.

static mut READINGS: Option<Readings> = None;
static mut LAST_UPDATE: Option<Instant> = None;

/// We use SensorError on results from the `WaterMonitor` struct.
/// `SensorError` and `Readings` are copied directly from the Rust drivers.
#[derive(Copy, Clone, Debug, Serialize)]
pub enum SensorError {
    Bus,          // eg an I2C or SPI error
    NotConnected, // todo
    BadMeasurement,
}

// pub enum SerialError {};

/// Convert bytes to a float
/// Copy+pasted from `water_monitor::util`
pub fn bytes_to_float(bytes: &[u8]) -> f32 {
    let bytes: [u8; 4] = bytes.try_into().unwrap();
    // todo: Be vs Le vs ne
    f32::from_bits(u32::from_ne_bytes(bytes))
}

#[derive(Debug, Clone, Serialize)]
pub struct Readings {
    pub T: Result<f32, SensorError>,
    pub pH: Result<f32, SensorError>,
    pub ORP: Result<f32, SensorError>,
    pub ec: Result<f32, SensorError>,
}

impl Readings {
    /// Read a 20-byte set. Each reading is 5 bytes: 1 for ok/error, the other
    /// 4 for a float. Copy+pasted from drivers.
    pub fn from_bytes(buf: &[u8]) -> Self {
        let mut result = Readings {
            // These errors are identified in the Water Monitor firmware, and
            // passed explicitly with the error code to indicate this.
            T: Err(SensorError::BadMeasurement),
            pH: Err(SensorError::BadMeasurement),
            ORP: Err(SensorError::BadMeasurement),
            ec: Err(SensorError::BadMeasurement),
        };

        if buf[0] == OK_BIT {
            result.T = Ok(bytes_to_float(&buf[1..5]));
        }

        if buf[5] == OK_BIT {
            result.pH = Ok(bytes_to_float(&buf[6..10]));
        }

        if buf[10] == OK_BIT {
            result.ORP = Ok(bytes_to_float(&buf[11..15]));
        }

        if buf[15] == OK_BIT {
            result.ec = Ok(bytes_to_float(&buf[16..20]));
        }

        result
    }
}

impl Default for Readings {
    fn default() -> Self {
        Self {
            T: Err(SensorError::NotConnected),
            pH: Err(SensorError::NotConnected),
            ORP: Err(SensorError::NotConnected),
            ec: Err(SensorError::NotConnected),
        }
    }
}

/// This mirrors that in the Python driver
struct WaterMonitor {
    ser: Box<dyn serialport::SerialPort>,
}

impl WaterMonitor {
    pub fn new() -> Result<Self, io::Error> {
        if let Ok(ports) = serialport::available_ports() {
            for port in &ports {
                if let SerialPortType::UsbPort(info) = &port.port_type {
                    if let Some(sn) = &info.serial_number {
                        if sn == "WM" {
                            return Ok(Self {
                                ser: serialport::open(&port.port_name)
                                    .expect("Problem opening the serial port."),
                            });
                        }
                    }
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Can't get readings from the Water Monitor.",
        ))
    }

    pub fn read_all(&mut self) -> Result<Readings, io::Error> {
        let xmit_buf = &[100, 150, 200]; // todo: Don't hard code it like this.

        self.ser.write(xmit_buf)?;

        let mut rx_buf = [0; 20];
        self.ser.read(&mut rx_buf)?;

        Ok(Readings::from_bytes(&rx_buf))
    }

    /// Close the serial port
    pub fn close(&mut self) {}
}

/// Get readings over JSON, which we've cached.
#[get("/readings")]
fn view_readings() -> String {
    let last_update = unsafe { LAST_UPDATE.as_ref().unwrap() };

    if (Instant::now() - *last_update) > Duration::new(0, REFRESH_INTERVAL * 1_000_000) {
        get_readings().unwrap();
        unsafe { LAST_UPDATE = Some(Instant::now()) };
    }

    let readings = unsafe { &READINGS.as_ref().unwrap() };
    return serde_json::to_string(readings).unwrap_or("Problem taking readings".into());
}

/// Request readings from the Water Monitor over USB/serial. Cache them as a
/// global variable. Requesting the readings directly from the frontend could result in
/// conflicts, where multiple frontends are requesting readings from the WM directly
/// in too short an interval.
fn get_readings() -> Result<(), io::Error> {
    let water_monitor = WaterMonitor::new();

    if let Ok(mut wm) = water_monitor {
        let readings = wm.read_all().expect("Problem taking readings");
        wm.close();
        unsafe { READINGS = Some(readings) };
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Can't find the Water Monitor.",
        ))
    }
}

fn main() {
    unsafe { READINGS = Some(Readings::default()) };

    get_readings().unwrap();
    unsafe { LAST_UPDATE = Some(Instant::now()) };

    println!(
        "AnyLeaf Water Monitor app launched. You can connect by opening `localhost` in a \
    web browser on this computer, or by navigating to `{}` on another device on this network, \
    like your phone.",
        local_ipaddress::get().unwrap_or("(Problem finding IP address)".into())
    );

    let config = Config::build(Environment::Staging)
        // .address("1.2.3.4")
        .port(80) // 80 means default, ie users can just go to localhost
        .log_level(LoggingLevel::Critical) // Don't show the user the connections.
        .finalize()
        .expect("Problem setting up our custom config");

    rocket::custom(config)
        .mount("/", StaticFiles::from("static"))
        .mount("/api", routes![view_readings])
        .launch();
}
