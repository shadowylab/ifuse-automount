// Copyright (c) 2025 Yuki Kishimoto
// Distributed under the MIT software license

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use std::{fmt, fs, io, thread};

use rusb::{
    Context, Device, DeviceDescriptor, DeviceHandle, Hotplug, HotplugBuilder, Language,
    Registration, UsbContext,
};

const TIMEOUT: Duration = Duration::from_secs(5);

const APPLE_VENDOR_ID: u16 = 0x05AC;

const APPLE_PRODUCT_IDS: [u16; 25] = [
    0x1290, // iPhone
    0x1291, // iPod Touch 1.Gen
    0x1292, // iPhone 3G
    0x1293, // iPod Touch 2.Gen
    0x1294, // iPhone 3GS
    0x1296, // iPod Touch 3.Gen (8GB)
    0x1297, // iPhone 4
    0x1299, // iPod Touch 3.Gen
    0x129a, // iPad
    0x129c, // iPhone 4(CDMA)
    0x129d, // iPhone
    0x129e, // iPod Touch 4.Gen
    0x129f, // iPad 2
    0x12a0, // iPhone 4S
    0x12a1, // iPhone
    0x12a2, // iPad 2 (3G; 64GB)
    0x12a3, // iPad 2 (CDMA)
    0x12a4, // iPad 3 (wifi)
    0x12a5, // iPad 3 (CDMA)
    0x12a6, // iPad 3 (3G, 16 GB)
    0x12a8, // iPhone 5/5C/5S/6/SE/7/8/X/XR
    0x12a9, // iPad 2
    0x12aa, // iPod Touch 5.Gen [A1421]
    0x12ab, // iPad
    0x12ac, // iPhone
];

#[derive(Debug)]
enum Error {
    Io(io::Error),
    Usb(rusb::Error),
    CantMount(String),
    IfuseNotInstalled,
    DeviceNotFound,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Usb(e) => write!(f, "{e}"),
            Self::CantMount(e) => write!(f, "Can't mount device: {e}"),
            Self::IfuseNotInstalled => write!(f, "ifuse not installed"),
            Self::DeviceNotFound => write!(f, "Device not found"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rusb::Error> for Error {
    fn from(e: rusb::Error) -> Self {
        Self::Usb(e)
    }
}

enum Action {
    Mount,
    Unmount,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct DeviceAddr {
    bus: u8,
    addr: u8,
}

#[derive(Clone)]
struct Handler {
    base_path: PathBuf,
    /// Devices: bus address and serial number
    devices: HashMap<DeviceAddr, String>,
}

impl Handler {
    #[inline]
    fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            devices: HashMap::new(),
        }
    }

    fn spawn(mut self, rx: mpsc::Receiver<(Device<Context>, Action)>) {
        thread::spawn(move || loop {
            match rx.recv() {
                Ok((device, action)) => {
                    if let Err(e) = self.handle_device(device, action) {
                        eprintln!("{e}");
                    }
                }
                Err(e) => eprintln!("{e}"),
            }
        });
    }

    fn handle_device<T>(&mut self, device: Device<T>, action: Action) -> Result<(), Error>
    where
        T: UsbContext,
    {
        // Check again if ifuse is installed
        if !is_ifuse_installed() {
            return Err(Error::IfuseNotInstalled);
        }

        // Wait a little before proceeding
        thread::sleep(Duration::from_millis(500));

        // Get device descriptor
        let descriptor: DeviceDescriptor = device.device_descriptor()?;

        let vendor_id: u16 = descriptor.vendor_id();
        let product_id: u16 = descriptor.product_id();

        // Check if it's an apple device
        if !is_apple_device(vendor_id, product_id) {
            return Ok(());
        }

        // Get device address
        let addr: DeviceAddr = DeviceAddr {
            bus: device.bus_number(),
            addr: device.address(),
        };

        match action {
            Action::Mount => {
                println!("Opening device: vendor_id={vendor_id}, product_id={product_id}");

                let serial_number: String = {
                    // Open device
                    let handle: DeviceHandle<T> = device.open()?;

                    // Reset state
                    handle.reset()?;

                    let languages: Vec<Language> = handle.read_languages(TIMEOUT)?;

                    if languages.is_empty() {
                        return Err(Error::CantMount(String::from("Languages empty")));
                    }

                    // Read serial number
                    let language: Language = languages[0];
                    let serial_number: String =
                        handle.read_serial_number_string(language, &descriptor, TIMEOUT)?;

                    // Return serial number
                    serial_number
                };

                println!("Found an Apple device: serial_number={serial_number}");

                let path: PathBuf = self.base_path.join(&serial_number);

                // Create directory
                println!("Creating directory: {}", path.display());
                fs::create_dir_all(&path)?;

                // Mount device with ifuse
                println!("Mounting device at {}", path.display());
                ifuse_mount(path)?;

                // TODO: schedule for a retry if `ifuse_mount` fails

                // Insert into devices
                self.devices.insert(addr, serial_number);
            }
            Action::Unmount => {
                println!("Unmounting device: vendor_id={vendor_id}, product_id={product_id}");
                match self.devices.remove(&addr) {
                    Some(serial_number) => {
                        let path: PathBuf = self.base_path.join(&serial_number);
                        println!("Unmounting device from {}", path.display());
                        ifuse_unmount(path)?;
                    }
                    None => return Err(Error::DeviceNotFound),
                }
            }
        }

        Ok(())
    }
}

struct HotPlugHandler<T>
where
    T: UsbContext,
{
    tx: mpsc::Sender<(Device<T>, Action)>,
}

// Send device and action with the mpsc channel because this method mustn't block.
// If this method blocks, "resources busy" error will start appearing.
impl<T> Hotplug<T> for HotPlugHandler<T>
where
    T: UsbContext,
{
    fn device_arrived(&mut self, device: Device<T>) {
        if let Err(e) = self.tx.send((device, Action::Mount)) {
            eprintln!("{e}");
        }
    }

    fn device_left(&mut self, device: Device<T>) {
        if let Err(e) = self.tx.send((device, Action::Unmount)) {
            eprintln!("{e}");
        }
    }
}

#[inline]
fn is_apple_device(vendor_id: u16, product_id: u16) -> bool {
    APPLE_VENDOR_ID == vendor_id && APPLE_PRODUCT_IDS.contains(&product_id)
}

fn is_ifuse_installed() -> bool {
    let output = Command::new("ifuse")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(output, Ok(status) if status.success())
}

fn ifuse_mount<P>(path: P) -> Result<(), Error>
where
    P: AsRef<Path>,
{
    // Run command
    // `ifuse /path/where/to/mount`
    let output: Output = Command::new("ifuse")
        .arg(path.as_ref())
        .stdout(Stdio::null())
        .output()?;

    // Check status
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(Error::CantMount(err.to_string()));
    }

    Ok(())
}

fn ifuse_unmount<P>(path: P) -> Result<(), Error>
where
    P: AsRef<Path>,
{
    // Run command
    // `fusermount -u /path/to/mounted/device`
    let output: Output = Command::new("fusermount")
        .arg("-u")
        .arg(path.as_ref())
        .stdout(Stdio::null())
        .output()?;

    // Check status
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(Error::CantMount(err.to_string()));
    }

    Ok(())
}

fn main() -> Result<(), Error> {
    // Check if supported
    if !rusb::has_hotplug() {
        panic!("libusb hotplug api unsupported");
    }

    // Check if ifuse is installed
    if !is_ifuse_installed() {
        return Err(Error::IfuseNotInstalled);
    }

    // Compose path
    let runtime_dir: PathBuf = dirs::runtime_dir().expect("home dir not found");
    let base_path: PathBuf = runtime_dir.join("ifuse-automount");

    let (tx, rx) = mpsc::channel();
    let hotplug_handler = HotPlugHandler { tx };

    // Opens a new libusb context
    let context: Context = Context::new()?;

    // Build handler and spawn it
    Handler::new(base_path).spawn(rx);

    // The registration is canceled on drop
    let _guard: Registration<Context> = HotplugBuilder::new()
        .enumerate(true)
        .register(&context, Box::new(hotplug_handler))?;

    // Wait for events
    loop {
        context.handle_events(None)?;
    }
}
