// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use libc::*;
use libc::{c_char, c_int};
use ndk::event::{MotionAction, MotionEvent};
use parking_lot::Mutex;
use std::mem;
use std::thread;
use std::{io::Write};
use uinput_sys::*;
use tokio::net::UnixListener;
use tokio::io::AsyncWriteExt;
use std::sync::mpsc::SyncSender;
use once_cell::sync::Lazy;
use log::{info, error, warn};
use anyhow::{Result, anyhow};

const KEY_BACK: i32 = 158;
const KEY_ENTER: i32 = 28;
const FF_MAX: u16 = 0x7f;
const TOUCH_PATH: &'static str = "/data/data/io.twoyi/rootfs/dev/input/touch";
const TOUCH_DEVICE_NAME: &'static str = "vtouch";
const TOUCH_DEVICE_UNIQUE_ID: &'static str = "<vtouch 0>";
const KEY_DEVICE_NAME: &'static str = "vkey";
const KEY_DEVICE_UNIQUE_ID: &'static str = "<keyboard 0>";
const KEY_PATH: &'static str = "/data/data/io.twoyi/rootfs/dev/input/key0";

#[repr(C)]
#[derive(Clone, Copy)]
struct device_info {
    name: [c_char; 80],
    driver_version: c_int,
    id: input_id,
    physical_location: [c_char; 80],
    unique_id: [c_char; 80],
    key_bitmask: [u8; (KEY_MAX as usize + 1) / 8],
    abs_bitmask: [u8; (ABS_MAX as usize + 1) / 8],
    rel_bitmask: [u8; (REL_MAX as usize + 1) / 8],
    sw_bitmask: [u8; (SW_MAX as usize + 1) / 8],
    led_bitmask: [u8; (LED_MAX as usize + 1) / 8],
    ff_bitmask: [u8; (FF_MAX as usize + 1) / 8],
    prop_bitmask: [u8; (INPUT_PROP_MAX as usize + 1) / 8],
    abs_max: [u32; ABS_CNT as usize],
    abs_min: [u32; ABS_CNT as usize],
}

unsafe fn any_as_u8_slice<T: Sized>(p: &T) -> &[u8] {
    ::std::slice::from_raw_parts((p as *const T) as *const u8, ::std::mem::size_of::<T>())
}

fn copy_to_cstr<const COUNT: usize>(data: &str, arr: &mut [u8; COUNT]) {
    let cstr = std::ffi::CString::new(data).expect("create cstring failed");
    let bytes = cstr.as_bytes_with_nul();
    let mut len = bytes.len();
    if len >= COUNT { len = COUNT; }
    arr[..len].copy_from_slice(bytes);
}

const MAX_POINTERS: usize = 5;
static INPUT_SENDER: Lazy<Mutex<Option<SyncSender<input_event>>>> = Lazy::new(|| Mutex::new(None));
static KEY_SENDER: Lazy<Mutex<Option<SyncSender<input_event>>>> = Lazy::new(|| Mutex::new(None));

pub fn start_input_system(width: i32, height: i32) -> Result<()> {
    thread::spawn(move || {
        if let Err(e) = touch_server(width, height) { error!("Touch server error: {:?}", e); }
    });
    thread::spawn(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Err(e) = key_server().await { error!("Key server error: {:?}", e); }
        });
    });
    Ok(())
}

pub fn input_event_write(tx: &SyncSender<input_event>, kind: i32, code: i32, val: i32) -> Result<()> {
    let mut tp = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut tp); }
    let ev = input_event {
        kind: kind as u16, code: code as u16, value: val,
        time: timeval { tv_sec: tp.tv_sec, tv_usec: (tp.tv_nsec / 1000) as i64 },
    };
    if tx.try_send(ev).is_err() { warn!("Input channel full, dropping event"); }
    Ok(())
}

pub fn handle_touch(ev: MotionEvent) -> Result<()> {
    let action = ev.action();
    let pointer = ev.pointer_at_index(ev.pointer_index());
    let pointer_id = pointer.pointer_id();
    let x = pointer.x() as i32;
    let y = pointer.y() as i32;
    let pressure = pointer.pressure() as i32;

    static G_INPUT_MT: Lazy<Mutex<[i32; MAX_POINTERS]>> = Lazy::new(|| Mutex::new([0i32; MAX_POINTERS]));
    if let Some(mut mt) = G_INPUT_MT.try_lock() {
        match action {
            MotionAction::Down | MotionAction::PointerDown => mt[pointer_id as usize] = 1,
            MotionAction::Up | MotionAction::PointerUp | MotionAction::Cancel => mt[pointer_id as usize] = 0,
            _ => (),
        }
    }

    if let Some(sender_lock) = INPUT_SENDER.try_lock() {
        if let Some(ref tx) = *sender_lock {
            match action {
                MotionAction::Down | MotionAction::PointerDown | MotionAction::Move => {
                    input_event_write(tx, EV_ABS, ABS_MT_SLOT, pointer_id)?;
                    if action != MotionAction::Move {
                        input_event_write(tx, EV_ABS, ABS_MT_TRACKING_ID, pointer_id + 1)?;
                        if pointer_id == 0 { input_event_write(tx, EV_KEY, BTN_TOUCH, 1)?; }
                    }
                    input_event_write(tx, EV_ABS, ABS_MT_POSITION_X, x)?;
                    input_event_write(tx, EV_ABS, ABS_MT_POSITION_Y, y)?;
                    input_event_write(tx, EV_ABS, ABS_MT_PRESSURE, pressure)?;
                    input_event_write(tx, EV_SYN, SYN_REPORT, 0)?;
                },
                MotionAction::Up | MotionAction::PointerUp | MotionAction::Cancel => {
                    input_event_write(tx, EV_ABS, ABS_MT_SLOT, pointer_id)?;
                    input_event_write(tx, EV_ABS, ABS_MT_TRACKING_ID, -1)?;
                    if pointer_id == 0 { input_event_write(tx, EV_KEY, BTN_TOUCH, 0)?; }
                    input_event_write(tx, EV_SYN, SYN_REPORT, 0)?;
                },
                _ => {}
            }
        }
    }
    Ok(())
}

fn generate_touch_device(width: i32, height: i32) -> device_info {
    let mut info: device_info = unsafe { mem::zeroed() };  // Tambah : device_info
    info.driver_version = 0x1;
    info.id = input_id { product: 0x1, version: 0, vendor: 0, bustype: 0 };
    copy_to_cstr(TOUCH_DEVICE_NAME, &mut info.name);
    copy_to_cstr(TOUCH_PATH, &mut info.physical_location);
    copy_to_cstr(TOUCH_DEVICE_UNIQUE_ID, &mut info.unique_id);
    info.prop_bitmask[0] = INPUT_PROP_BUTTONPAD as u8;
    for &bit in &[ABS_MT_SLOT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_PRESSURE] {
        set_bit(&mut info.abs_bitmask, bit as usize);
    }
    info.abs_min[ABS_MT_POSITION_X as usize] = 0;
    info.abs_max[ABS_MT_POSITION_X as usize] = width as u32;
    info.abs_min[ABS_MT_POSITION_Y as usize] = 0;
    info.abs_max[ABS_MT_POSITION_Y as usize] = height as u32;
    info.abs_min[ABS_MT_SLOT as usize] = 0;
    info.abs_max[ABS_MT_SLOT as usize] = (MAX_POINTERS - 1) as u32;
    info.abs_min[ABS_MT_PRESSURE as usize] = 0;
    info.abs_max[ABS_MT_PRESSURE as usize] = 255;
    info
}

fn set_bit(bitmask: &mut [u8], bit: usize) {
    let byte = bit / 8;
    let offset = bit % 8;
    bitmask[byte] |= 1 << offset;
}

#[allow(unreachable_code)]
fn touch_server(width: i32, height: i32) -> Result<()> {
    let device = generate_touch_device(width, height);
    loop {
        let _ = std::fs::remove_file(TOUCH_PATH);
        let listener = std::os::unix::net::UnixListener::bind(TOUCH_PATH)
            .map_err(|e| anyhow!("Bind touch socket failed: {:?}", e))?;
        unsafe {
            if let Ok(path_cstr) = std::ffi::CString::new(TOUCH_PATH) {
                libc::chmod(path_cstr.as_ptr(), 0o777);
            }
        }
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                info!("Game input connected!");
                let _ = stream.set_nonblocking(true);
                let _ = stream.write_all(unsafe { any_as_u8_slice(&device) });
                let (tx, rx) = std::sync::mpsc::sync_channel::<input_event>(100);
                *INPUT_SENDER.lock() = Some(tx);
                loop {
                    match rx.recv() {
                        Ok(ev) => {
                            let data = unsafe { any_as_u8_slice(&ev) };
                            if let Err(e) = stream.write_all(data) {
                                if e.kind() != std::io::ErrorKind::WouldBlock {
                                    error!("Broken pipe, reconnecting...");
                                    break;
                                }
                            }
                        },
                        Err(_) => break,
                    }
                }
                *INPUT_SENDER.lock() = None;
                break;
            }
        }
    }
    Ok(())
}

fn generate_key_device() -> device_info {
    let mut info: device_info = unsafe { mem::zeroed() };  // Tambah : device_info
    info.driver_version = 0x1;
    info.id = input_id { product: 0x1, version: 0, vendor: 0, bustype: 0 };
    copy_to_cstr(KEY_DEVICE_NAME, &mut info.name);
    copy_to_cstr(KEY_PATH, &mut info.physical_location);
    copy_to_cstr(KEY_DEVICE_UNIQUE_ID, &mut info.unique_id);
    for &bit in &[KEY_BACK, KEY_ENTER] {
        set_bit(&mut info.key_bitmask, bit as usize);
    }
    info
}

pub fn send_key_code(keycode: i32) -> Result<()> {
    if let Some(ref tx) = *KEY_SENDER.lock() {
        input_event_write(tx, EV_KEY, keycode, 1)?;
        input_event_write(tx, EV_SYN, SYN_REPORT, 0)?;
        input_event_write(tx, EV_KEY, keycode, 0)?;
        input_event_write(tx, EV_SYN, SYN_REPORT, 0)?;
    }
    Ok(())
}

async fn key_server() -> Result<()> {
    let device = generate_key_device();
    let _ = std::fs::remove_file(KEY_PATH);
    let listener = UnixListener::bind(KEY_PATH)?;
    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                info!("key client connected!");
                let _ = stream.write_all(unsafe { any_as_u8_slice(&device) }).await;
                let (tx, rx) = std::sync::mpsc::sync_channel::<input_event>(10);
                *KEY_SENDER.lock() = Some(tx);
                tokio::spawn(async move {
                    loop {
                        match rx.recv() {
                            Ok(ev) => {
                                let data = unsafe { any_as_u8_slice(&ev) };
                                if stream.write_all(data).await.is_err() { break; }
                            },
                            Err(_) => break,
                        }
                    }
                });
            }
            Err(_) => {
                error!("key server accept error!");
                break;
            }
        }
    }
    Ok(())
}