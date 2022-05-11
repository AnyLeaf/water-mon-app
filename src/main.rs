#![feature(proc_macro_hygiene, decl_macro)]
#![allow(non_snake_case)]

#[macro_use]
extern crate rocket;

use rocket::config::{Config, Environment, LoggingLevel};

use serde::Serialize;
use serde_json;

use rocket_contrib::serve::StaticFiles;

use std::{
    convert::TryInto,
    io,
    time::{Duration, Instant},
};

use chrono;

use local_ipaddress;
use serialport::{self, SerialPortType};

// Bits for serial communication with a PC over USB.
// Copy+pasted from `quadcopter::protocols::usb
static mut CRC_LUT: [u8; 256] = [0; 256];
const CRC_POLY: u8 = 0xab;

const PARAMS_SIZE: usize = 76; // + message type, payload len, and crc.
const CONTROLS_SIZE: usize = 18; // + message type, payload len, and crc.

const MAX_PAYLOAD_SIZE: usize = PARAMS_SIZE; // For Params.
const MAX_PACKET_SIZE: usize = MAX_PAYLOAD_SIZE + 3; // + message type, payload len, and crc.

struct DecodeError {}

const REFRESH_INTERVAL: u32 = 200; // Time between querying the FC for readings in ms.

static mut READINGS: Option<Readings> = None;
static mut LAST_ATTITUDE_UPDATE: Option<Instant> = None;
static mut LAST_CONTROLS_UPDATE: Option<Instant> = None;

#[derive(Clone, Copy, Eq, PartialEq, TryFromPrimitive)]
#[repr(u8)]
/// Repr is how this type is passed as serial.
pub enum MsgType {
    /// Transmit from FC
    Params = 0,
    SetMotorDirs = 1,
    /// Receive to FC
    ReqParams = 2,
    /// Acknowledgement, eg in response to setting something.
    Ack = 3,
    /// Controls data (From FC)
    Controls = 4,
    /// Request controls data. (From PC)
    ReqControls = 5,
}

impl MsgType {
    pub fn payload_size(&self) -> usize {
        match self {
            Self::Params => PARAMS_SIZE,
            Self::SetMotorDirs => 1, // Packed bits: motors 1-4, R-L. True = CW.
            Self::ReqParams => 0,
            Self::Ack => 0,
            Self::Controls => CONTROLS_SIZE,
            Self::ReqControls => 0,
        }
    }
}

pub struct Packet {
    message_type: MsgType,
    payload_size: usize,
    payload: [u8; MAX_PAYLOAD_SIZE], // todo?
    crc: u8,
}

/// Represents channel data in our end-use format.
#[derive(Default)]
pub struct ChannelData {
    /// Aileron, -1. to 1.
    pub roll: f32,
    /// Elevator, -1. to 1.
    pub pitch: f32,
    /// Throttle, 0. to 1., or -1. to 1. depending on if stick auto-centers.
    pub throttle: f32,
    /// Rudder, -1. to 1.
    pub yaw: f32,
    pub arm_status: ArmStatus,
    pub input_mode: InputModeSwitch,
    pub alt_hold: AltHoldSwitch,
    // todo: Auto-recover commanded, auto-TO/land/RTB, obstacle avoidance etc.
}

/// Represents a first-order status of the drone. todo: What grid/reference are we using?
#[derive(Default)]
pub struct Params {
    // todo: Do we want to use this full struct, or store multiple (3+) instantaneous ones?
    pub s_x: f32,
    pub s_y: f32,
    // Note that we only need to specify MSL vs AGL for position; velocity and accel should
    // be equiv for them.
    pub s_z_msl: f32,
    pub s_z_agl: f32,

    pub s_pitch: f32,
    pub s_roll: f32,
    pub s_yaw: f32,

    // Velocity
    pub v_x: f32,
    pub v_y: f32,
    pub v_z: f32,

    pub v_pitch: f32,
    pub v_roll: f32,
    pub v_yaw: f32,

    // Acceleration
    pub a_x: f32,
    pub a_y: f32,
    pub a_z: f32,

    pub a_pitch: f32,
    pub a_roll: f32,
    pub a_yaw: f32,
}


// End C+P

// Code in this section is a reverse of buffer <--> struct conversion in `usb_cfg`.

impl From<[u8; PARAMS_SIZE]> for Params {
    /// 19 f32s x 4 = 76. In the order we have defined in the struct.
    fn from(p: &[u8]) -> Self {
        Params {
            s_x: bytes_to_float(p[0..4]),
            s_y: bytes_to_float(p[0..4]),
            s_z_msl: bytes_to_float(p[0..4]),
            s_z_agl: bytes_to_float(p[0..4]),
        
            s_pitch: bytes_to_float(p[0..4]),
            s_roll: bytes_to_float(p[0..4]),
            s_yaw: bytes_to_float(p[0..4]),

            v_x: bytes_to_float(p[0..4]),
            v_y: bytes_to_float(p[0..4]),
            v_z: bytes_to_float(p[0..4]),
        
            v_pitch: bytes_to_float(p[0..4]),
            v_roll: bytes_to_float(p[0..4]),
            v_yaw: bytes_to_float(p[0..4]),
        
            a_x: bytes_to_float(p[0..4]),
            a_y: bytes_to_float(p[0..4]),
            a_z: bytes_to_float(p[0..4]),
        
            a_pitch: bytes_to_float(p[0..4]),
            a_roll: bytes_to_float(p[0..4]),
            a_yaw: bytes_to_float(p[0..4]),
        }

    }
}


// End code reversed from `quadcopter`.

// todo: Baud cfg?



// pub enum SerialError {};

/// Convert bytes to a float
/// Copy+pasted from `water_monitor::util`
pub fn bytes_to_float(bytes: &[u8]) -> f32 {
    let bytes: [u8; 4] = bytes.try_into().unwrap();
    f32::from_bits(u32::from_be_bytes(bytes))
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
                                ser: serialport::open(&port.port_name)?,
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

    // Only update the readings from the WM if we're past the last updated thresh.
    if (Instant::now() - *last_update) > Duration::new(0, REFRESH_INTERVAL * 1_000_000) {
        if let Err(_) = get_readings() {
            // todo: Is this normal? Seems harmless, but I'd like to
            // todo get to the bottom of it.
            // println!("Problem getting readings; sending old.")
        }

        unsafe { LAST_UPDATE = Some(Instant::now()) };
    }

    let readings = unsafe { &READINGS.as_ref().unwrap() };
    return serde_json::to_string(readings).unwrap_or("Problem taking readings".into());
    // return serde_json::to_string(readings).unwrap_or("Problem taking readings".into());
}

/// Request readings from the Water Monitor over USB/serial. Cache them as a
/// global variable. Requesting the readings directly from the frontend could result in
/// conflicts, where multiple frontends are requesting readings from the WM directly
/// in too short an interval.
fn get_readings() -> Result<(), io::Error> {
    let water_monitor = WaterMonitor::new();

    if let Ok(mut wm) = water_monitor {
        let readings = wm.read_all().unwrap_or_default();
        wm.close();
        // println!("readings: {:?}", &readings);
        unsafe { READINGS = Some(readings) };
        Ok(())
    } else {
        // println!("Can't find water monitor"); // Debugging.
        Err(io::Error::new(
            io::ErrorKind::Other,
            "Can't find the Water Monitor.",
        ))
    }
}

fn main() {
    unsafe { READINGS = Some(Readings::default()) };
    unsafe { LAST_UPDATE = Some(Instant::now()) };

    println!(
        "The AnyLeaf Water Monitor app launched. You can connect by opening `localhost` in a \
    web browser on this computer, or by navigating to `{}` on another device on this network, \
    like your phone.\n",
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
