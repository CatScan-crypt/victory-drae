//! Nintendo Switch Pro Controller (057E:2009), read as a HID device: it is
//! no XInput pad. Buttons go by position (Nintendo B is the console's A).
//! Steam, while running, has the pad sending full reports (0x30) already;
//! over Bluetooth it sends simple ones (0x3F) by itself; over USB with
//! neither we send the USB handshake.

use super::*;

/// Report byte, bit, button: full reports (0x30, and 0x21/0x31 that start
/// the same).
const FULL: [(usize, u8, u32); 14] = [
    (3, 0x04, A), // B
    (3, 0x08, B), // A
    (3, 0x01, X), // Y
    (3, 0x02, Y), // X
    (3, 0x40, RIGHT_SHOULDER),
    (4, 0x01, VIEW), // minus
    (4, 0x02, MENU), // plus
    (4, 0x04, RIGHT_STICK),
    (4, 0x08, LEFT_STICK),
    (5, 0x01, DOWN),
    (5, 0x02, UP),
    (5, 0x04, RIGHT),
    (5, 0x08, LEFT),
    (5, 0x40, LEFT_SHOULDER),
];
const FULL_ZR: (usize, u8) = (3, 0x80);
const FULL_ZL: (usize, u8) = (5, 0x80);

/// The same for simple reports (0x3F).
const SIMPLE: [(usize, u8, u32); 10] = [
    (1, 0x01, A), // B
    (1, 0x02, B), // A
    (1, 0x04, X), // Y
    (1, 0x08, Y), // X
    (1, 0x10, LEFT_SHOULDER),
    (1, 0x20, RIGHT_SHOULDER),
    (2, 0x01, VIEW),
    (2, 0x02, MENU),
    (2, 0x04, LEFT_STICK),
    (2, 0x08, RIGHT_STICK),
];
const SIMPLE_ZL: (usize, u8) = (1, 0x40);
const SIMPLE_ZR: (usize, u8) = (1, 0x80);
/// Hat values 0-7 from up, clockwise; 8 is none.
const HAT: [u32; 8] = [
    UP,
    UP | RIGHT,
    RIGHT,
    DOWN | RIGHT,
    DOWN,
    DOWN | LEFT,
    LEFT,
    UP | LEFT,
];

/// Full-report sticks are 12 bits around 2048; without the factory
/// calibration (asking for it would be a write) this is about their reach.
const STICK_CENTER: f32 = 2048.0;
const STICK_REACH: f32 = 1600.0;

fn bits(report: &[u8], table: &[(usize, u8, u32)]) -> u32 {
    table
        .iter()
        .filter(|&&(byte, bit, _)| report[byte] & bit != 0)
        .fold(0, |all, &(_, _, button)| all | button)
}

fn held(report: &[u8], (byte, bit): (usize, u8)) -> f32 {
    if report[byte] & bit != 0 { 1.0 } else { 0.0 }
}

/// A pad reading from an input report, `None` for other reports.
pub(super) fn parse(report: &[u8]) -> Option<PadState> {
    if report.len() < 12 {
        return None;
    }
    match report[0] {
        0x30 | 0x21 | 0x31 => {
            let stick = |at: usize| {
                let x = u16::from(report[at]) | u16::from(report[at + 1] & 0xF) << 8;
                let y = u16::from(report[at + 1] >> 4) | u16::from(report[at + 2]) << 4;
                let axis = |v: u16| ((f32::from(v) - STICK_CENTER) / STICK_REACH).clamp(-1.0, 1.0);
                (axis(x), axis(y))
            };
            let (left_x, left_y) = stick(6);
            let (right_x, right_y) = stick(9);
            Some(PadState {
                buttons: bits(report, &FULL),
                left_trigger: held(report, FULL_ZL),
                right_trigger: held(report, FULL_ZR),
                left_x,
                left_y,
                right_x,
                right_y,
            })
        }
        0x3F => {
            // 16 bits around 0x8000, down positive.
            let axis = |at: usize| {
                let v = f32::from(u16::from_le_bytes([report[at], report[at + 1]]));
                ((v - 32768.0) / 32767.0).clamp(-1.0, 1.0)
            };
            Some(PadState {
                buttons: bits(report, &SIMPLE)
                    | HAT.get(usize::from(report[3])).copied().unwrap_or(0),
                left_trigger: held(report, SIMPLE_ZL),
                right_trigger: held(report, SIMPLE_ZR),
                left_x: axis(4),
                left_y: -axis(6),
                right_x: axis(8),
                right_y: -axis(10),
            })
        }
        _ => None,
    }
}

#[cfg(windows)]
pub(super) use host::Pros;

#[cfg(windows)]
mod host {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
        CM_Get_Device_Interface_ListW, CR_SUCCESS,
    };
    use windows_sys::core::GUID;

    use super::*;

    /// `GUID_DEVINTERFACE_HID`.
    const HID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);
    /// USB and Bluetooth spellings of 057E:2009 in interface paths.
    const IDS: [&str; 2] = ["vid_057e&pid_2009", "vid&0002057e_pid&2009"];
    /// Looking for a pad when there is none, this often.
    const RESCAN: Duration = Duration::from_secs(2);
    /// No report this long after opening: the USB pad waits for a handshake.
    const QUIET: Duration = Duration::from_secs(1);
    /// HID output report length over USB.
    const OUTPUT: usize = 64;

    struct Pro {
        path: String,
        latest: Arc<Mutex<Option<PadState>>>,
        alive: Arc<AtomicBool>,
        opened: Instant,
        woken: bool,
    }

    /// The first Switch Pro Controller found, read on a thread of its own.
    #[derive(Default)]
    pub(in super::super) struct Pros {
        pro: Option<Pro>,
        next_scan: Option<Instant>,
    }

    impl Pros {
        pub(in super::super) fn state(&mut self) -> Option<PadState> {
            if self
                .pro
                .as_ref()
                .is_some_and(|p| !p.alive.load(Ordering::Acquire))
            {
                let pro = self.pro.take()?;
                tracing::info!(path = %pro.path, "Switch Pro controller gone");
            }
            if self.pro.is_none() && self.next_scan.is_none_or(|at| Instant::now() >= at) {
                self.next_scan = Some(Instant::now() + RESCAN);
                self.pro = find().and_then(open);
            }
            let pro = self.pro.as_mut()?;
            let latest = *lock(&pro.latest);
            if latest.is_none() && !pro.woken && pro.opened.elapsed() >= QUIET {
                pro.woken = true;
                tracing::info!(path = %pro.path, "Switch Pro controller quiet, sending the USB handshake");
                wake(&pro.path);
            }
            latest
        }
    }

    fn find() -> Option<String> {
        let mut len = 0;
        // SAFETY: plain queries into a buffer of the size asked for.
        let list = unsafe {
            if CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &HID,
                std::ptr::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            ) != CR_SUCCESS
            {
                return None;
            }
            let mut list = vec![0_u16; len as usize];
            if CM_Get_Device_Interface_ListW(
                &HID,
                std::ptr::null(),
                list.as_mut_ptr(),
                len,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            ) != CR_SUCCESS
            {
                return None;
            }
            list
        };
        list.split(|&c| c == 0)
            .map(String::from_utf16_lossy)
            .find(|path| {
                let lower = path.to_ascii_lowercase();
                IDS.iter().any(|id| lower.contains(id))
            })
    }

    fn open(path: String) -> Option<Pro> {
        let file = File::open(&path)
            .inspect_err(|e| tracing::warn!(%path, %e, "Switch Pro controller: cannot open"))
            .ok()?;
        tracing::info!(%path, "Switch Pro controller");
        let latest = Arc::new(Mutex::new(None));
        let alive = Arc::new(AtomicBool::new(true));
        let (out, flag) = (latest.clone(), alive.clone());
        std::thread::Builder::new()
            .name("switch-pro".into())
            .spawn(move || read(file, &out, &flag))
            .ok()?;
        Some(Pro {
            path,
            latest,
            alive,
            opened: Instant::now(),
            woken: false,
        })
    }

    /// Reads reports until the pad goes away.
    fn read(mut file: File, latest: &Mutex<Option<PadState>>, alive: &AtomicBool) {
        // Larger than any of its input reports (64 over USB, 362 over
        // Bluetooth).
        let mut report = [0; 512];
        while let Ok(n) = file.read(&mut report) {
            if let Some(pad) = parse(&report[..n]) {
                *lock(latest) = Some(pad);
            }
        }
        *lock(latest) = None;
        alive.store(false, Ordering::Release);
    }

    /// The USB handshake, no timeout, then full reports.
    fn wake(path: &str) {
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(path) else {
            return;
        };
        let full_reports = [
            0x01, 0, 0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40, 0x03, 0x30,
        ];
        for command in [&[0x80, 0x02][..], &[0x80, 0x04], &full_reports] {
            let mut report = [0; OUTPUT];
            report[..command.len()].copy_from_slice(command);
            // A Bluetooth pad takes other lengths; it needs none of this.
            let _ = file.write_all(&report);
        }
    }

    /// With a pad plugged in: `cargo test -p victory-hle real_switch_pro -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a Switch Pro Controller"]
    fn real_switch_pro() {
        let mut pros = Pros::default();
        let until = Instant::now() + Duration::from_secs(2);
        let mut last = None;
        while Instant::now() < until {
            last = pros.state().or(last);
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("{last:?}");
        assert!(last.is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_reports_convert_by_position() {
        // At rest, then B, plus, D-pad left, ZL, ZR, left stick full left
        // and up.
        let rest = [
            0x30, 0xC7, 0x91, 0x00, 0x80, 0x00, 0x00, 0x08, 0x80, 0x00, 0x08, 0x80,
        ];
        assert_eq!(parse(&rest), Some(PadState::default()));
        let mut pressed = rest;
        pressed[3] = 0x04 | 0x80;
        pressed[4] = 0x80 | 0x02;
        pressed[5] = 0x08 | 0x80;
        pressed[6..9].copy_from_slice(&[0x00, 0xF0, 0xFF]);
        let pad = parse(&pressed).unwrap();
        assert_eq!(pad.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                pad.left_trigger,
                pad.right_trigger,
                pad.left_x,
                pad.left_y,
                pad.right_x
            ),
            (1.0, 1.0, -1.0, 1.0, 0.0)
        );
    }

    #[test]
    fn simple_reports_convert_with_the_hat() {
        let mut report = [0_u8; 12];
        report[0] = 0x3F;
        report[1] = 0x02 | 0x40; // A, ZL
        report[3] = 1; // up-right
        for at in [4, 6, 8, 10] {
            report[at..at + 2].copy_from_slice(&0x8000_u16.to_le_bytes());
        }
        report[6..8].copy_from_slice(&0_u16.to_le_bytes()); // left stick up
        let pad = parse(&report).unwrap();
        assert_eq!(pad.buttons, B | UP | RIGHT);
        assert_eq!((pad.left_trigger, pad.left_y, pad.left_x), (1.0, 1.0, 0.0));
    }

    #[test]
    fn other_reports_are_no_readings() {
        assert_eq!(parse(&[0x81, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse(&[0x30, 0]), None);
    }
}
