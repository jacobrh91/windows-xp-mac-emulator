//! DirectInput: the mouse (relative movement, for games that grab it while dragging) and the
//! keyboard (a scan code state array), both fed from the emulated desktop's input.

use std::collections::HashMap;

use crate::cpu::{Emu, Res};
use crate::win32::{Args, Handler};

const DI_OK: u32 = 0;
const DIERR_NOTACQUIRED: u32 = 0x8007_000C;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DIERR_DEVICENOTREG: u32 = 0x8004_0154;
/// First dword of GUID_SysMouse and GUID_SysKeyboard (the rest is the same).
const GUID_SYS_MOUSE: u32 = 0x6F1D_2B60;
const GUID_SYS_KEYBOARD: u32 = 0x6F1D_2B61;

const DIRECT_INPUT: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("CreateDevice", 4),
    ("EnumDevices", 5),
    ("GetDeviceStatus", 2),
    ("RunControlPanel", 3),
    ("Initialize", 3),
];

const DEVICE: &[(&str, usize)] = &[
    ("QueryInterface", 3),
    ("AddRef", 1),
    ("Release", 1),
    ("GetCapabilities", 2),
    ("EnumObjects", 4),
    ("GetProperty", 3),
    ("SetProperty", 3),
    ("Acquire", 1),
    ("Unacquire", 1),
    ("GetDeviceState", 3),
    ("GetDeviceData", 5),
    ("SetDataFormat", 2),
    ("SetEventNotification", 2),
    ("SetCooperativeLevel", 3),
    ("GetObjectInfo", 4),
    ("GetDeviceInfo", 2),
    ("RunControlPanel", 3),
    ("Initialize", 4),
];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Mouse,
    Keyboard,
}

struct Device {
    kind: Kind,
    acquired: bool,
    /// Where the mouse was when last read: its movement since is what is reported.
    last: (i32, i32),
}

#[derive(Default)]
pub struct DInput {
    vtables: HashMap<&'static str, u32>,
    devices: HashMap<u32, Device>,
}

fn new_object(e: &mut Emu, iface: &'static str, methods: &[(&'static str, usize)]) -> Res<u32> {
    let vt = match e.win.dinput.vtables.get(iface) {
        Some(&v) => v,
        None => {
            let v = e.heap_alloc(4 * methods.len() as u32)?;
            for (i, (m, _)) in methods.iter().enumerate() {
                let stub = e.win.resolve("dinput", &format!("{iface}::{m}"));
                e.mem.write(v + 4 * i as u32, 4, stub)?;
            }
            e.win.dinput.vtables.insert(iface, v);
            v
        }
    };
    let ptr = e.heap_alloc(16)?;
    e.mem.write(ptr, 4, vt)?;
    Ok(ptr)
}

pub fn lookup(dll: &str, name: &str) -> Option<(usize, Handler)> {
    if dll != "dinput" {
        return None;
    }
    if name == "DirectInputCreateA" {
        return Some((4, direct_input_create));
    }
    let (iface, method) = name.split_once("::")?;
    let methods = match iface {
        "IDirectInput" => DIRECT_INPUT,
        "IDirectInputDevice" => DEVICE,
        _ => return None,
    };
    let (_, argc) = *methods.iter().find(|(m, _)| *m == method)?;
    let handler: Handler = match method {
        "QueryInterface" => |_, _| Ok(E_NOINTERFACE),
        "AddRef" | "Release" => |_, _| Ok(1),
        "CreateDevice" => create_device,
        "Acquire" => acquire,
        "Unacquire" => |e, a| {
            if let Some(d) = e.win.dinput.devices.get_mut(&a[0]) {
                d.acquired = false;
            }
            Ok(DI_OK)
        },
        "GetDeviceState" => get_device_state,
        "GetDeviceData" => |e, a| {
            if a[3] != 0 {
                e.mem.write(a[3], 4, 0)?; // no buffered events
            }
            Ok(DI_OK)
        },
        _ => |_, _| Ok(DI_OK),
    };
    Some((argc, handler))
}

fn direct_input_create(e: &mut Emu, a: &Args) -> Res<u32> {
    let di = new_object(e, "IDirectInput", DIRECT_INPUT)?;
    e.mem.write(a[2], 4, di)?;
    Ok(DI_OK)
}

fn create_device(e: &mut Emu, a: &Args) -> Res<u32> {
    let kind = match e.mem.read(a[1], 4)? {
        GUID_SYS_MOUSE => Kind::Mouse,
        GUID_SYS_KEYBOARD => Kind::Keyboard,
        _ => return Ok(DIERR_DEVICENOTREG),
    };
    let dev = new_object(e, "IDirectInputDevice", DEVICE)?;
    e.win.dinput.devices.insert(
        dev,
        Device {
            kind,
            acquired: false,
            last: (0, 0),
        },
    );
    e.mem.write(a[2], 4, dev)?;
    Ok(DI_OK)
}

fn acquire(e: &mut Emu, a: &Args) -> Res<u32> {
    let cursor = e.wm.cursor;
    if let Some(d) = e.win.dinput.devices.get_mut(&a[0]) {
        d.acquired = true;
        d.last = cursor;
    }
    Ok(DI_OK)
}

/// (this, size, out): DIMOUSESTATE (x, y, z movement and four buttons) or the keyboard's 256
/// bytes, with 0x80 for what is held.
fn get_device_state(e: &mut Emu, a: &Args) -> Res<u32> {
    let cursor = e.wm.cursor;
    let Some(d) = e.win.dinput.devices.get_mut(&a[0]).filter(|d| d.acquired) else {
        return Ok(DIERR_NOTACQUIRED);
    };
    let kind = d.kind;
    let moved = (cursor.0 - d.last.0, cursor.1 - d.last.1);
    d.last = cursor;
    let mut state = vec![0u8; a[1] as usize];
    match kind {
        Kind::Mouse if state.len() >= 16 => {
            state[0..4].copy_from_slice(&moved.0.to_le_bytes());
            state[4..8].copy_from_slice(&moved.1.to_le_bytes());
            let buttons = e.wm.buttons;
            for (i, mask) in [1, 2, 0x10].into_iter().enumerate() {
                state[12 + i] = if buttons & mask != 0 { 0x80 } else { 0 };
            }
        }
        Kind::Keyboard if state.len() >= 256 => {
            for vk in 0..256u32 {
                if e.wm.key_down[vk as usize]
                    && let Some((scan, extended)) = crate::gui::keyboard::scan_code(vk)
                {
                    state[(scan | if extended { 0x80 } else { 0 }) as usize] = 0x80;
                }
            }
        }
        _ => {}
    }
    e.mem.write_bytes(a[2], &state)?;
    Ok(DI_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(e: &mut Emu, name: &str, args: &[u32]) -> u32 {
        let (_, h) = lookup("dinput", name).unwrap();
        let mut a: Args = [0; crate::win32::MAX_ARGS];
        a[..args.len()].copy_from_slice(args);
        h(e, &a).unwrap()
    }

    fn device(e: &mut Emu, guid: u32) -> u32 {
        e.mem.map(0x10000, 0x1000);
        call(e, "DirectInputCreateA", &[0, 0x500, 0x10000, 0]);
        let di = e.mem.read(0x10000, 4).unwrap();
        e.mem.write(0x10010, 4, guid).unwrap();
        assert_eq!(
            call(e, "IDirectInput::CreateDevice", &[di, 0x10010, 0x10004, 0]),
            DI_OK
        );
        e.mem.read(0x10004, 4).unwrap()
    }

    #[test]
    fn mouse_reports_movement_since_it_was_last_read() {
        let mut e = Emu::new();
        let m = device(&mut e, GUID_SYS_MOUSE);
        let state = [m, 16, 0x10100];
        assert_eq!(
            call(&mut e, "IDirectInputDevice::GetDeviceState", &state),
            DIERR_NOTACQUIRED
        );
        e.wm.cursor = (100, 100);
        call(&mut e, "IDirectInputDevice::Acquire", &[m]);
        e.wm.cursor = (110, 95);
        e.wm.buttons = 2;
        assert_eq!(
            call(&mut e, "IDirectInputDevice::GetDeviceState", &state),
            DI_OK
        );
        assert_eq!(e.mem.read(0x10100, 4).unwrap(), 10);
        assert_eq!(e.mem.read(0x10104, 4).unwrap() as i32, -5);
        assert_eq!(e.mem.read(0x1010c, 2).unwrap(), 0x8000); // the right button
        call(&mut e, "IDirectInputDevice::GetDeviceState", &state);
        assert_eq!(e.mem.read(0x10100, 4).unwrap(), 0);
    }

    #[test]
    fn keyboard_reports_held_keys_by_scan_code() {
        let mut e = Emu::new();
        let k = device(&mut e, GUID_SYS_KEYBOARD);
        call(&mut e, "IDirectInputDevice::Acquire", &[k]);
        e.wm.key_down[0x41] = true; // A
        call(
            &mut e,
            "IDirectInputDevice::GetDeviceState",
            &[k, 256, 0x10100],
        );
        assert_eq!(e.mem.read(0x10100 + 0x1E, 1).unwrap(), 0x80);
        assert_eq!(e.mem.read(0x10100 + 0x1F, 1).unwrap(), 0);
    }
}
