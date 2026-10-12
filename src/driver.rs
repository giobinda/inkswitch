//! Userspace driver for XP-Pen tablets speaking the "UGEE v2" protocol.
//!
//! The tablet is switched to its vendor reporting mode over raw USB; pen, express-key and dial
//! reports are then translated into two uinput devices: a pen the compositor treats as a
//! drawing tablet, and a keyboard (with mouse buttons) that plays the shortcuts of the current
//! profile. Mouse clicks in a shortcut are made with the pen while it is over the tablet, since
//! the desktop's mouse pointer does not follow the pen.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode as Abs, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, RelativeAxisCode, UinputAbsSetup};

use crate::settings::{pressure_curve, Action, Chord, Compiled};

const VENDOR_ID: u16 = 0x28bd;
/// Switches the tablet from mouse emulation to full vendor reports.
const INIT_REPORT: [u8; 10] = [0x02, 0xb0, 0x04, 0, 0, 0, 0, 0, 0, 0];
/// String descriptor holding the tablet's coordinate and pressure ranges.
const PARAMS_STRING: u16 = 100;
const RETRY: Duration = Duration::from_secs(1);
/// How long a tapped shortcut stays down; applications miss key presses of no duration.
const TAP: Duration = Duration::from_millis(12);
/// Pause between events sent through different virtual devices, which keeps their order.
const DEVICE_GAP: Duration = Duration::from_millis(4);

/// State shared between the driver thread and whoever switches profiles.
pub struct Shared {
    profile: Mutex<Arc<Compiled>>,
    /// Kernel name (eventN) of the pen device while a tablet is connected.
    pub pen_sysname: Mutex<Option<String>>,
}

impl Shared {
    pub fn new(profile: Compiled) -> Arc<Self> {
        Arc::new(Shared { profile: Mutex::new(Arc::new(profile)), pen_sysname: Mutex::new(None) })
    }

    pub fn set_profile(&self, profile: Compiled) {
        *self.profile.lock().unwrap() = Arc::new(profile);
    }

    fn profile(&self) -> Arc<Compiled> {
        self.profile.lock().unwrap().clone()
    }
}

#[derive(Debug, PartialEq)]
pub enum Report {
    Pen { tip: bool, button1: bool, button2: bool, eraser: bool, x: i32, y: i32, pressure: i32, tilt_x: i32, tilt_y: i32 },
    OutOfRange,
    /// Express keys as a bitmask and one dial step (+1 clockwise, -1 counter-clockwise).
    Aux { keys: u16, dial: i32 },
}

pub fn parse_report(buf: &[u8]) -> Option<Report> {
    if buf.len() < 8 || buf[0] != 0x02 {
        return None;
    }
    let le16 = |i: usize| i32::from(buf[i]) | i32::from(buf[i + 1]) << 8;
    let status = buf[1];
    match status {
        0xf0 => Some(Report::Aux {
            keys: u16::from(buf[2]) | u16::from(buf[3]) << 8,
            dial: match buf[7] {
                0x01 => 1,
                0x02 => -1,
                _ => 0,
            },
        }),
        0xc0 => Some(Report::OutOfRange),
        // 0xf6 (pen identification) and 0xb0 (reply to the init report) carry nothing we use.
        _ if status & 0xf0 == 0xa0 => {
            // Large tablets carry the bits above 16 of each coordinate at the end of the report.
            let high = |i: usize| buf.get(i).map_or(0, |b| i32::from(*b) << 16);
            let tilt = |i: usize| buf.get(i).map_or(0, |b| i32::from(*b as i8));
            Some(Report::Pen {
                tip: status & 0x01 != 0,
                button1: status & 0x02 != 0,
                button2: status & 0x04 != 0,
                eraser: status & 0x08 != 0,
                x: le16(2) | high(10),
                y: le16(4) | high(11),
                // Pens with 16384 levels put the top bit in the last byte.
                pressure: le16(6) | buf.get(13).map_or(0, |b| i32::from(b & 0x01) << 13),
                tilt_x: tilt(8),
                tilt_y: tilt(9),
            })
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ranges {
    pub max_x: i32,
    pub max_y: i32,
    pub max_pressure: i32,
    pub lines_per_inch: i32,
    /// Number of express keys.
    pub keys: usize,
}

impl Ranges {
    /// Used when the tablet doesn't tell; the pen still covers the whole screen.
    const FALLBACK: Ranges = Ranges { max_x: 65535, max_y: 65535, max_pressure: 8191, lines_per_inch: 5080, keys: 8 };
}

/// Decodes string descriptor 100 (including its two header bytes).
pub fn parse_params(desc: &[u8]) -> Option<Ranges> {
    if desc.len() < 12 {
        return None;
    }
    let le16 = |i: usize| i32::from(desc[i]) | i32::from(desc[i + 1]) << 8;
    let high = |i: usize| desc.get(i).map_or(0, |b| i32::from(*b) << 16);
    let r = Ranges {
        max_x: le16(2) | high(12),
        max_y: le16(4) | high(13),
        max_pressure: le16(8),
        lines_per_inch: le16(10),
        keys: usize::from(desc[6]),
    };
    (r.max_x > 0 && r.max_y > 0 && r.max_pressure > 0).then_some(r)
}

pub struct Tablet {
    pub name: String,
    pub product_id: u16,
    /// sysfs directory of the USB device.
    pub usb_dir: PathBuf,
    /// The interface speaking the vendor protocol, and its two interrupt endpoints.
    pub interface: u32,
    pub endpoint_in: u32,
    pub endpoint_out: u32,
    /// Size of one report; reads have to ask for exactly this much.
    pub report_size: usize,
}

/// Finds a connected XP-Pen tablet and its vendor-protocol interface: the HID interface that
/// has an interrupt endpoint in each direction.
pub fn find_tablet() -> Option<Tablet> {
    let read = |path: PathBuf| fs::read_to_string(path).ok().map(|s| s.trim().to_string());
    let hex = |path: PathBuf| read(path).and_then(|s| u32::from_str_radix(&s, 16).ok());
    for device in fs::read_dir("/sys/bus/usb/devices").ok()?.flatten() {
        let usb_dir = device.path();
        if hex(usb_dir.join("idVendor")) != Some(u32::from(VENDOR_ID)) {
            continue;
        }
        for interface in fs::read_dir(&usb_dir).ok()?.flatten() {
            let dir = interface.path();
            if hex(dir.join("bInterfaceClass")) != Some(3) {
                continue;
            }
            let (mut endpoint_in, mut endpoint_out) = (None, None);
            for endpoint in fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()) {
                if read(endpoint.join("type")).as_deref() != Some("Interrupt") {
                    continue;
                }
                let address = hex(endpoint.join("bEndpointAddress"))?;
                if address & 0x80 != 0 {
                    endpoint_in = Some((address, hex(endpoint.join("wMaxPacketSize"))? as usize));
                } else {
                    endpoint_out = Some(address);
                }
            }
            let (Some((endpoint_in, report_size)), Some(endpoint_out)) = (endpoint_in, endpoint_out) else { continue };
            return Some(Tablet {
                name: read(usb_dir.join("product")).unwrap_or_else(|| "XP-Pen tablet".into()),
                product_id: hex(usb_dir.join("idProduct"))? as u16,
                interface: hex(dir.join("bInterfaceNumber"))?,
                endpoint_in,
                endpoint_out,
                report_size,
                usb_dir,
            });
        }
    }
    None
}

// usbfs ioctls, as on 64-bit Linux.
const USBDEVFS_CONTROL: libc::c_ulong = 0xc018_5500;
const USBDEVFS_BULK: libc::c_ulong = 0xc018_5502;
const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = 0x8004_550f;
const USBDEVFS_IOCTL: libc::c_ulong = 0xc010_5512;
const USBDEVFS_DISCONNECT: libc::c_int = 0x5516;

/// Direct access to the tablet through usbfs.
pub struct Usb {
    file: File,
}

impl Usb {
    pub fn open(tablet: &Tablet) -> Result<Usb> {
        let number = |file: &str| -> Result<u32> { Ok(fs::read_to_string(tablet.usb_dir.join(file))?.trim().parse()?) };
        let node = format!("/dev/bus/usb/{:03}/{:03}", number("busnum")?, number("devnum")?);
        let file = OpenOptions::new().read(true).write(true).open(&node).with_context(|| format!("opening {node}"))?;
        Ok(Usb { file })
    }

    fn ioctl<T>(&self, request: libc::c_ulong, arg: &mut T) -> io::Result<usize> {
        // SAFETY: every caller passes the struct the kernel expects for `request`, and any
        // buffer it points to outlives the call.
        let n = unsafe { libc::ioctl(self.file.as_raw_fd(), request, arg as *mut T) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// Reads a string descriptor, raw (header included).
    pub fn string_descriptor(&self, index: u16) -> io::Result<Vec<u8>> {
        #[repr(C)]
        struct ControlTransfer {
            request_type: u8,
            request: u8,
            value: u16,
            index: u16,
            length: u16,
            timeout_ms: u32,
            data: *mut libc::c_void,
        }
        let mut buf = [0u8; 64];
        let mut transfer = ControlTransfer {
            request_type: 0x80,
            request: 0x06,
            value: 0x0300 | index,
            index: 0x0409,
            length: buf.len() as u16,
            timeout_ms: 500,
            data: buf.as_mut_ptr().cast(),
        };
        let n = self.ioctl(USBDEVFS_CONTROL, &mut transfer)?;
        Ok(buf[..n].to_vec())
    }

    /// Takes the interface for ourselves, away from the kernel's HID driver if it has it.
    pub fn claim(&self, interface: u32) -> io::Result<()> {
        #[repr(C)]
        struct Ioctl {
            interface: libc::c_int,
            code: libc::c_int,
            data: *mut libc::c_void,
        }
        let mut disconnect = Ioctl { interface: interface as libc::c_int, code: USBDEVFS_DISCONNECT, data: std::ptr::null_mut() };
        // Fails when no kernel driver is bound, which is just as good.
        let _ = self.ioctl(USBDEVFS_IOCTL, &mut disconnect);
        let mut interface = interface;
        self.ioctl(USBDEVFS_CLAIMINTERFACE, &mut interface).map(|_| ())
    }

    /// One interrupt transfer in the direction the endpoint address says.
    pub fn transfer(&self, endpoint: u32, data: &mut [u8], timeout_ms: u32) -> io::Result<usize> {
        #[repr(C)]
        struct BulkTransfer {
            endpoint: u32,
            length: u32,
            timeout_ms: u32,
            data: *mut libc::c_void,
        }
        let mut transfer = BulkTransfer { endpoint, length: data.len() as u32, timeout_ms, data: data.as_mut_ptr().cast() };
        self.ioctl(USBDEVFS_BULK, &mut transfer)
    }

    /// Claims the vendor interface and switches the tablet to full reports.
    pub fn start(tablet: &Tablet) -> Result<Usb> {
        let usb = Usb::open(tablet)?;
        usb.claim(tablet.interface).context("claiming the tablet (is another driver using it?)")?;
        let mut init = INIT_REPORT;
        usb.transfer(tablet.endpoint_out, &mut init, 500).context("switching the tablet to pen mode")?;
        Ok(usb)
    }

    /// Waits for the next report; `None` when nothing arrived for a while.
    pub fn read_report(&self, tablet: &Tablet, buf: &mut [u8; 64]) -> io::Result<Option<usize>> {
        match self.transfer(tablet.endpoint_in, &mut buf[..tablet.report_size.min(64)], 1000) {
            Ok(n) => Ok(Some(n)),
            Err(e) if e.raw_os_error() == Some(libc::ETIMEDOUT) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Takes the kernel's own input devices for the tablet away from the compositor, so pen and
/// keys don't arrive twice (once from the kernel, once from us).
fn grab_kernel_devices(usb_dir: &Path) -> Vec<evdev::Device> {
    let mut grabbed = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/input") else { return grabbed };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("event") || !fs::canonicalize(entry.path()).is_ok_and(|p| p.starts_with(usb_dir)) {
            continue;
        }
        match evdev::Device::open(format!("/dev/input/{name}")).and_then(|mut d| d.grab().map(|()| d)) {
            Ok(device) => grabbed.push(device),
            Err(e) => eprintln!("could not take over /dev/input/{name}: {e}"),
        }
    }
    grabbed
}

fn key(code: KeyCode, down: bool) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, i32::from(down))
}

fn abs(axis: Abs, value: i32) -> InputEvent {
    InputEvent::new(EventType::ABSOLUTE.0, axis.0, value)
}

pub fn create_pen(name: &str, product_id: u16, ranges: &Ranges) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in [KeyCode::BTN_TOOL_PEN, KeyCode::BTN_TOOL_RUBBER, KeyCode::BTN_TOUCH, KeyCode::BTN_STYLUS, KeyCode::BTN_STYLUS2] {
        keys.insert(code);
    }
    let per_mm = (f64::from(ranges.lines_per_inch) / 25.4).round() as i32;
    let axis = |code, min, max, resolution| UinputAbsSetup::new(code, AbsInfo::new(0, min, max, 0, 0, resolution));
    let device = VirtualDevice::builder()?
        .name(&format!("{name} Pen"))
        .input_id(InputId::new(BusType::BUS_USB, VENDOR_ID, product_id, 0x100))
        .with_keys(&keys)?
        .with_absolute_axis(&axis(Abs::ABS_X, 0, ranges.max_x, per_mm))?
        .with_absolute_axis(&axis(Abs::ABS_Y, 0, ranges.max_y, per_mm))?
        .with_absolute_axis(&axis(Abs::ABS_PRESSURE, 0, ranges.max_pressure, 0))?
        // Degrees; the resolution is units per radian.
        .with_absolute_axis(&axis(Abs::ABS_TILT_X, -60, 60, 57))?
        .with_absolute_axis(&axis(Abs::ABS_TILT_Y, -60, 60, 57))?
        .build()
        .context("creating the pen device (is /dev/uinput writable?)")?;
    Ok(device)
}

fn create_keyboard(name: &str, product_id: u16) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in 1..=248 {
        keys.insert(KeyCode::new(code));
    }
    for button in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] {
        keys.insert(button);
    }
    // Mouse buttons are only accepted from a device that can also move the pointer.
    let mut axes = AttributeSet::<RelativeAxisCode>::new();
    axes.insert(RelativeAxisCode::REL_X);
    axes.insert(RelativeAxisCode::REL_Y);
    let device = VirtualDevice::builder()?
        .name(&format!("{name} Keys"))
        .input_id(InputId::new(BusType::BUS_USB, VENDOR_ID, product_id, 0x100))
        .with_keys(&keys)?
        .with_relative_axes(&axes)?
        .build()
        .context("creating the shortcut keyboard")?;
    Ok(device)
}

pub fn sysname(device: &mut VirtualDevice) -> Option<String> {
    let node = device.enumerate_dev_nodes_blocking().ok()?.flatten().next()?;
    Some(node.file_name()?.to_string_lossy().into_owned())
}

/// Asks KWin to map the pen to one screen (an empty name restores its default).
pub fn map_to_output(conn: &zbus::blocking::Connection, sysname: &str, output: &str) -> Result<()> {
    conn.call_method(
        Some("org.kde.KWin"),
        format!("/org/kde/KWin/InputDevice/{sysname}"),
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &("org.kde.KWin.InputDevice", "outputName", zbus::zvariant::Value::from(output)),
    )?;
    Ok(())
}

/// Where one part of a held shortcut went, so it is released the same way.
#[derive(Clone, Copy)]
enum Sent {
    /// A key or mouse button on the virtual keyboard.
    Keyboard(KeyCode),
    /// A mouse button made with the pen: its tip or one of its buttons.
    Pen(KeyCode),
}

/// What a physical button currently holds down, so it can be released even if the profile
/// changed in the meantime.
#[derive(Default, Clone)]
enum Held {
    #[default]
    Nothing,
    Shortcut(Vec<Sent>),
    Stylus(KeyCode),
}

/// How a mouse button is made with the pen: touching down for a left click, and the pen
/// buttons that Qt applications (Krita among them) read as middle and right click.
fn pen_equivalent(button: KeyCode) -> Option<KeyCode> {
    match button {
        KeyCode::BTN_LEFT => Some(KeyCode::BTN_TOUCH),
        KeyCode::BTN_MIDDLE => Some(KeyCode::BTN_STYLUS),
        KeyCode::BTN_RIGHT => Some(KeyCode::BTN_STYLUS2),
        _ => None,
    }
}

struct Session {
    pen: VirtualDevice,
    keyboard: VirtualDevice,
    ranges: Ranges,
    /// BTN_TOOL_PEN or BTN_TOOL_RUBBER while the pen is in range.
    tool: Option<KeyCode>,
    /// Whether the pen really touches, and its (curved) pressure, as of the last report.
    tip: bool,
    pressure: i32,
    /// Tip and pen buttons pressed on the user's behalf by shortcuts with mouse clicks in them.
    forced: Vec<KeyCode>,
    express_down: u16,
    express_held: Vec<Held>,
    pen_buttons_down: [bool; 2],
    pen_buttons_held: [Held; 2],
}

impl Session {
    /// Tip and button state of the pen: what it really does plus what shortcuts add.
    fn pen_button_events(&self) -> [InputEvent; 4] {
        let forced = |code| self.forced.contains(&code);
        let stylus = |code| forced(code) || self.pen_buttons_held.iter().any(|h| matches!(h, Held::Stylus(c) if *c == code));
        let touching = self.tip || forced(KeyCode::BTN_TOUCH);
        let pressure = match (self.tip, touching) {
            (true, _) => self.pressure.max(1),
            // A click made on the user's behalf presses at half strength.
            (false, true) => self.ranges.max_pressure / 2,
            (false, false) => 0,
        };
        [
            abs(Abs::ABS_PRESSURE, pressure),
            key(KeyCode::BTN_TOUCH, touching),
            key(KeyCode::BTN_STYLUS, stylus(KeyCode::BTN_STYLUS)),
            key(KeyCode::BTN_STYLUS2, stylus(KeyCode::BTN_STYLUS2)),
        ]
    }

    /// Presses a shortcut, in order, and returns how to let go of it.
    fn down(&mut self, chord: &Chord) -> Result<Vec<Sent>> {
        let mut sent = Vec::new();
        for code in chord {
            match pen_equivalent(*code).filter(|_| self.tool.is_some()) {
                Some(button) => {
                    if !sent.is_empty() {
                        // Two devices are involved; make sure the modifiers arrive first.
                        thread::sleep(DEVICE_GAP);
                    }
                    self.forced.push(button);
                    self.pen.emit(&self.pen_button_events())?;
                    sent.push(Sent::Pen(button));
                }
                None => {
                    self.keyboard.emit(&[key(*code, true)])?;
                    sent.push(Sent::Keyboard(*code));
                }
            }
        }
        Ok(sent)
    }

    fn up(&mut self, sent: &[Sent]) -> Result<()> {
        for part in sent.iter().rev() {
            match part {
                Sent::Keyboard(code) => self.keyboard.emit(&[key(*code, false)])?,
                Sent::Pen(button) => {
                    if let Some(i) = self.forced.iter().position(|b| b == button) {
                        self.forced.remove(i);
                    }
                    if self.tool.is_some() {
                        self.pen.emit(&self.pen_button_events())?;
                    }
                    thread::sleep(DEVICE_GAP);
                }
            }
        }
        Ok(())
    }

    fn tap(&mut self, chord: &Chord) -> Result<()> {
        let sent = self.down(chord)?;
        thread::sleep(TAP);
        self.up(&sent)
    }

    /// Performs an action that has no button to be held by, such as a dial step.
    fn tap_action(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::Chord(chord) => self.tap(chord),
            Action::Sequence(chords) => chords.iter().try_for_each(|chord| self.tap(chord)),
            Action::None | Action::Stylus1 | Action::Stylus2 => Ok(()),
        }
    }

    /// Starts an action and returns what has to be undone when the button comes back up.
    fn press(&mut self, action: &Action) -> Result<Held> {
        Ok(match action {
            Action::None => Held::Nothing,
            Action::Stylus1 => Held::Stylus(KeyCode::BTN_STYLUS),
            Action::Stylus2 => Held::Stylus(KeyCode::BTN_STYLUS2),
            Action::Chord(chord) => Held::Shortcut(self.down(chord)?),
            Action::Sequence(chords) => {
                for chord in chords {
                    self.tap(chord)?;
                }
                Held::Nothing
            }
        })
    }

    fn release(&mut self, held: Held) -> Result<()> {
        match held {
            Held::Shortcut(sent) => self.up(&sent),
            Held::Nothing | Held::Stylus(_) => Ok(()),
        }
    }

    fn leave_range(&mut self) -> Result<()> {
        self.tip = false;
        if let Some(tool) = self.tool.take() {
            self.pen.emit(&[
                abs(Abs::ABS_PRESSURE, 0),
                key(KeyCode::BTN_TOUCH, false),
                key(KeyCode::BTN_STYLUS, false),
                key(KeyCode::BTN_STYLUS2, false),
                key(tool, false),
            ])?;
        }
        Ok(())
    }

    fn handle(&mut self, report: Report, profile: &Compiled) -> Result<()> {
        match report {
            Report::OutOfRange => self.leave_range()?,
            Report::Pen { tip, button1, button2, eraser, x, y, pressure, tilt_x, tilt_y } => {
                let tool = if eraser { KeyCode::BTN_TOOL_RUBBER } else { KeyCode::BTN_TOOL_PEN };
                if self.tool != Some(tool) {
                    self.leave_range()?;
                }
                let buttons = [(button1, &profile.pen_button1), (button2, &profile.pen_button2)];
                for (i, (down, action)) in buttons.into_iter().enumerate() {
                    if down == self.pen_buttons_down[i] {
                        continue;
                    }
                    self.pen_buttons_down[i] = down;
                    if down {
                        self.pen_buttons_held[i] = self.press(action)?;
                    } else {
                        let held = std::mem::take(&mut self.pen_buttons_held[i]);
                        self.release(held)?;
                    }
                }
                // The extra pressure bit only means pressure on pens with more than 8192 levels.
                let pressure = if self.ranges.max_pressure > 0x1fff { pressure } else { pressure & 0x1fff };
                let raw = pressure.clamp(0, self.ranges.max_pressure) as f32 / self.ranges.max_pressure as f32;
                self.tip = tip;
                self.pressure = (pressure_curve(profile.pressure, raw) * self.ranges.max_pressure as f32).round() as i32;
                let mut events = vec![
                    abs(Abs::ABS_X, x.clamp(0, self.ranges.max_x)),
                    abs(Abs::ABS_Y, y.clamp(0, self.ranges.max_y)),
                    abs(Abs::ABS_TILT_X, tilt_x.clamp(-60, 60)),
                    abs(Abs::ABS_TILT_Y, tilt_y.clamp(-60, 60)),
                    key(tool, true),
                ];
                events.extend(self.pen_button_events());
                self.pen.emit(&events)?;
                self.tool = Some(tool);
            }
            Report::Aux { keys, dial } => {
                for i in 0..self.express_held.len() {
                    let down = keys & (1 << i) != 0;
                    if down == (self.express_down & (1 << i) != 0) {
                        continue;
                    }
                    if down {
                        let action = profile.keys.get(i).unwrap_or(&Action::None);
                        self.express_held[i] = self.press(action)?;
                    } else {
                        let held = std::mem::take(&mut self.express_held[i]);
                        self.release(held)?;
                    }
                }
                self.express_down = keys;
                match dial {
                    1 => self.tap_action(&profile.dial_cw)?,
                    -1 => self.tap_action(&profile.dial_ccw)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

/// Serves one tablet until it is unplugged (which surfaces as a read error).
fn serve(tablet: &Tablet, shared: &Shared, on_connect: &dyn Fn(&str, &Tablet, &Ranges), on_aux: &dyn Fn(u16, i32)) -> Result<()> {
    let usb = Usb::start(tablet)?;
    let ranges = match usb.string_descriptor(PARAMS_STRING).map(|d| parse_params(&d)) {
        Ok(Some(ranges)) => ranges,
        other => {
            eprintln!("the tablet did not report its ranges ({other:?}); using defaults");
            Ranges::FALLBACK
        }
    };
    let _grabbed = grab_kernel_devices(&tablet.usb_dir);

    let mut session = Session {
        pen: create_pen(&tablet.name, tablet.product_id, &ranges)?,
        keyboard: create_keyboard(&tablet.name, tablet.product_id)?,
        ranges,
        tool: None,
        tip: false,
        pressure: 0,
        forced: Vec::new(),
        express_down: 0,
        express_held: vec![Held::Nothing; 16],
        pen_buttons_down: [false; 2],
        pen_buttons_held: Default::default(),
    };
    let pen_sysname = sysname(&mut session.pen);
    *shared.pen_sysname.lock().unwrap() = pen_sysname.clone();
    on_connect(pen_sysname.as_deref().unwrap_or(""), tablet, &session.ranges);

    let mut buf = [0u8; 64];
    loop {
        let Some(n) = usb.read_report(tablet, &mut buf)? else { continue };
        if let Some(report) = parse_report(&buf[..n]) {
            if let Report::Aux { keys, dial } = report {
                on_aux(keys, dial);
            }
            session.handle(report, &shared.profile())?;
        }
    }
}

/// Runs forever: waits for a tablet, serves it, and starts over when it goes away.
/// `status` is called with a line describing the connection whenever it changes, `on_aux`
/// with the express keys held (as a bitmask) and the dial step on every such report.
pub fn run(shared: Arc<Shared>, status: impl Fn(&str), on_connect: impl Fn(&str, &Tablet, &Ranges), on_aux: impl Fn(u16, i32)) -> ! {
    status("not connected");
    loop {
        let Some(tablet) = find_tablet() else {
            thread::sleep(RETRY);
            continue;
        };
        eprintln!("tablet found: {}", tablet.name);
        status(&format!("connected: {}", tablet.name));
        if let Err(e) = serve(&tablet, &shared, &on_connect, &on_aux) {
            // Unplugging ends up here too.
            eprintln!("tablet session ended: {e:#}");
        }
        *shared.pen_sysname.lock().unwrap() = None;
        status("not connected");
        thread::sleep(RETRY);
    }
}

/// Prints what is known about a connected tablet and, if `seconds` > 0, its raw reports.
pub fn probe(seconds: u64) -> Result<()> {
    let Some(tablet) = find_tablet() else { bail!("no XP-Pen tablet is connected") };
    println!("tablet:   {} (28bd:{:04x})", tablet.name, tablet.product_id);
    println!("usb:      {} interface {}", tablet.usb_dir.display(), tablet.interface);
    let usb = Usb::open(&tablet)?;
    match usb.string_descriptor(PARAMS_STRING) {
        Ok(desc) => {
            println!("params:   {}", hex(&desc));
            println!("ranges:   {:?}", parse_params(&desc));
        }
        Err(e) => println!("params:   unavailable: {e}"),
    }
    if seconds == 0 {
        return Ok(());
    }
    drop(usb);
    let usb = Usb::start(&tablet)?;
    println!("reports for {seconds} s (move the pen, press keys, turn the dial):");
    let end = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut buf = [0u8; 64];
    let mut last = Vec::new();
    while std::time::Instant::now() < end {
        let Some(n) = usb.read_report(&tablet, &mut buf)? else { continue };
        // Pen movement repeats nearly identical reports; print only when the layout changes.
        let shape: Vec<u8> = buf[..n].iter().enumerate().map(|(i, b)| if (2..10).contains(&i) { 0 } else { *b }).collect();
        if shape != last {
            println!("  {}  {:?}", hex(&buf[..n]), parse_report(&buf[..n]));
            last = shape;
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pen_reports() {
        let hover = [0x02, 0xa0, 0x10, 0x27, 0x20, 0x4e, 0x00, 0x00, 0x05, 0xfb, 0x01, 0x00];
        assert_eq!(
            parse_report(&hover),
            Some(Report::Pen { tip: false, button1: false, button2: false, eraser: false, x: 0x1_2710, y: 0x4e20, pressure: 0, tilt_x: 5, tilt_y: -5 })
        );
        let drawing = [0x02, 0xa3, 0, 0, 0, 0, 0xff, 0x1f, 0, 0];
        assert!(matches!(parse_report(&drawing), Some(Report::Pen { tip: true, button1: true, button2: false, pressure: 8191, .. })));
        // Recorded from an Artist 15.6 Pro V2: pressing hard sets the top pressure bit.
        let hard = [0x02, 0xa1, 0x51, 0x99, 0xda, 0x57, 0xb0, 0x1f, 0x21, 0x01, 0, 0, 0, 0x01];
        assert!(matches!(parse_report(&hard), Some(Report::Pen { x: 0x9951, y: 0x57da, pressure: 16304, tilt_x: 33, tilt_y: 1, .. })));
        // Pen identification, sent when the pen comes into range.
        assert_eq!(parse_report(&[0x02, 0xf6, 0x12, 0x50, 0xb2, 0x08, 0x12, 0x26, 0x19, 0, 1, 1, 1, 0]), None);
        assert_eq!(parse_report(&[0x02, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0]), Some(Report::OutOfRange));
        assert_eq!(parse_report(&[0x07, 0xa0, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn aux_reports() {
        assert_eq!(parse_report(&[0x02, 0xf0, 0x05, 0, 0, 0, 0, 0, 0, 0]), Some(Report::Aux { keys: 0b101, dial: 0 }));
        assert_eq!(parse_report(&[0x02, 0xf0, 0, 0, 0, 0, 0, 0x02, 0, 0]), Some(Report::Aux { keys: 0, dial: -1 }));
    }

    #[test]
    fn params() {
        // As reported by an Artist 15.6 Pro V2.
        let desc = [0x0e, 0x03, 0x67, 0x0a, 0xff, 0x95, 0x08, 0x00, 0xff, 0x3f, 0xd8, 0x13, 0x01, 0x00];
        assert_eq!(parse_params(&desc), Some(Ranges { max_x: 68199, max_y: 38399, max_pressure: 16383, lines_per_inch: 5080, keys: 8 }));
        assert_eq!(parse_params(&[4, 3, 0, 0]), None);
    }
}
