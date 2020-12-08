#![feature(proc_macro_hygiene, decl_macro)]
#![allow(non_snake_case)]

#[macro_use]
extern crate rocket;

use serialport::{self, SerialPortType};
use crate::SensorError::BadMeasurement;

// Bits for serial communication with a PC over USB.
// Copy+pasted from Water Monitor main file.
const SUCCESS_MSG: [u8; 3] = [50, 50, 50]; // Send this to indicate an error.
const ERROR_MSG: [u8; 3] = [99, 99, 99]; // Send this to indicate an error.
const MSG_START_BITS: [u8; 2] = [100, 150];
const MSG_END_BITS: [u8; 1] = [200];
const OK_BIT: u8 = 10;
const ERROR_BIT: u8 = 20;

/// We use SensorError on results from the `WaterMonitor` struct.
/// `SensorError` and `Readings` are copied directly from the Rust drivers.
#[derive(Copy, Clone, Debug)]
pub enum SensorError {
    Bus,          // eg an I2C or SPI error
    NotConnected, // todo
    BadMeasurement,
}

#[derive(Debug, Clone)]
pub struct Readings {
    pub T: Result<f32, SensorError>,
    pub pH: Result<f32, SensorError>,
    pub ORP: Result<f32, SensorError>,
    pub ec: Result<f32, SensorError>,
}

impl Readings {
    /// Read a 20-byte set. Each reading is 5 bytes: 1 for ok/error, the other
    /// 4 for a float. Copy+pasted from drivers.
    fn from_bytes(buf: &[u8]) -> Self {
        let mut result = Readings {
            // todo: These aren't sensor errors. Add new error type or variant?
            T: Ok(0.),
            pH: Ok(0.),
            ORP: Ok(0.),
            ec: Ok(0.),
        };

        if buf[0] == OK_BIT {
            result.T = buf[1..5].to_bits().to_ne_bytes()
        }

        if buf[5] == OK_BIT {
            result.pH = buf[6..10].to_bits().to_ne_bytes()
        }

        if buf[10] == OK_BIT {
            result.ORPT = buf[11..15].to_bits().to_ne_bytes()
        }

        if buf[15] == OK_BIT {
            result.ec = buf[16..20].to_bits().to_ne_bytes()
        }

        result
    }


}

/// This mirrors that in the Python driver
struct WaterMonitor {
    ser: Box<dyn serialport::SerialPort>,
}

impl WaterMonitor {
    pub fn new() -> Self {
        if let Ok(ports) = serialport::available_ports() {
            for port in &ports {
                if let SerialPortType::UsbPort(info) = &port.port_type {
                    if let Some(sn) = &info.serial_number {
                        if sn == "WM" {
                            return Self {
                                ser: serialport::open(&port.port_name)
                                    .expect("Problem opening the serial port."),
                            };
                        }
                    }
                }
            }
        }
        panic!("Unable to find the Water Monitor. Is it plugged in?");
    }

    pub fn read_all(&mut self) -> Readings {

        let xmit_buf = &[100, 150, 200]; // todo: Don't hard code it like this.

        self.ser.write(xmit_buf).expect("Problem writing data");

        let mut rx_buf = &[];
        self.ser.read(&mut rx_buf).expect("Problem reading data");

        println!("RX BUF: {:?}", &rx_buf);
        println!("READING: {:?}", Readings::from_bytes(rx_buf));

        Readings::from_bytes(rx_buf)

    }

    /// Close the serial port
    pub fn close(&mut self) {}
}

#[get("/")]
fn hello() -> String {
    String::from("Hello, world!") + &format!("{:?}", serialport::available_ports())
}

fn main() {
    let mut wm = WaterMonitor::new();

    rocket::ignite().mount("/", routes![hello]).launch();
}
