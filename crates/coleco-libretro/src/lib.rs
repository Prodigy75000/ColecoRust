// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! libretro C ABI front-end for ColecoRust.
//!
//! **Always the HLE BIOS.** The core never asks the frontend for a BIOS file
//! and never reads one: a cartridge and nothing else. A real dump is a
//! development oracle for the dev harnesses, not something a player supplies.
//!
//! Lifted in shape from MegaRust's `md-libretro` (origin/main 3e97ca7), which
//! carries two lessons worth keeping: declare the pixel format that matches
//! the framebuffer (XRGB8888, value 1, not 2), and stamp the build into the
//! binary so a device's core can be named.
//!
//! Struct layouts and constants are from the libretro API header, a published
//! C interface and not emulator source.
//!
//! # Controls
//!
//! A ColecoVision controller is a joystick, two fire buttons and a twelve-key
//! keypad. A RetroPad has no keypad, so the keys most games need sit on the
//! spare buttons, and a keyboard, when the frontend has one, reaches all twelve:
//!
//! | ColecoVision | RetroPad | Keyboard |
//! |---|---|---|
//! | joystick | D-pad | |
//! | left fire | B | |
//! | right fire | A | |
//! | keypad 1, 2 | START, SELECT | 1, 2 |
//! | keypad 3, 4 | X, Y | 3, 4 |
//! | keypad `*`, `#` | L, R | `*` (or keypad `*`), `#` |
//! | keypad 5, 6, 7, 8 | L2, R2, L3, R3 | 5 to 8 |
//! | keypad 9, 0 | | 9, 0 |
//!
//! START is keypad 1 because "press 1" is how nearly every game in the corpus
//! starts ("skill 1, one player"). When two keypad buttons are held, the lower
//! key in the table wins.

#![allow(clippy::missing_safety_doc)]

use coleco_core::machine::{Coleco, Firmware, Pad};
use coleco_core::vdp::{HEIGHT, WIDTH};
use std::ffi::{c_char, c_uint, c_void};

pub const RETRO_API_VERSION: u32 = 1;

const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: c_uint = 10;
const RETRO_ENVIRONMENT_SET_INPUT_DESCRIPTORS: c_uint = 11;
/// `36 | RETRO_ENVIRONMENT_EXPERIMENTAL`.
const RETRO_ENVIRONMENT_SET_MEMORY_MAPS: c_uint = 36 | 0x10000;
/// `retro_pixel_format`: 0 = 0RGB1555, 1 = XRGB8888, 2 = RGB565. The whole
/// enum is written out because writing only the one value is how MegaRust
/// came to ship 2, which a frontend accepts and then draws wrong.
const RETRO_PIXEL_FORMAT_XRGB8888: c_uint = 1;

const RETRO_REGION_NTSC: c_uint = 0;
const RETRO_MEMORY_SYSTEM_RAM: c_uint = 2;
const RETRO_MEMDESC_SYSTEM_RAM: u64 = 1 << 2;

const RETRO_DEVICE_JOYPAD: c_uint = 1;
const RETRO_DEVICE_KEYBOARD: c_uint = 3;

// libretro's joypad ids, in its own order.
const JOY_B: c_uint = 0;
const JOY_Y: c_uint = 1;
const JOY_SELECT: c_uint = 2;
const JOY_START: c_uint = 3;
const JOY_UP: c_uint = 4;
const JOY_DOWN: c_uint = 5;
const JOY_LEFT: c_uint = 6;
const JOY_RIGHT: c_uint = 7;
const JOY_A: c_uint = 8;
const JOY_X: c_uint = 9;
const JOY_L: c_uint = 10;
const JOY_R: c_uint = 11;
const JOY_L2: c_uint = 12;
const JOY_R2: c_uint = 13;
const JOY_L3: c_uint = 14;
const JOY_R3: c_uint = 15;

/// Keypad keys on RetroPad buttons, in priority order: key numbers are 0-9,
/// 10 for `*`, 11 for `#`.
const KEYPAD_BUTTONS: [(c_uint, u8); 10] = [
    (JOY_START, 1),
    (JOY_SELECT, 2),
    (JOY_X, 3),
    (JOY_Y, 4),
    (JOY_L, 10),
    (JOY_R, 11),
    (JOY_L2, 5),
    (JOY_R2, 6),
    (JOY_L3, 7),
    (JOY_R3, 8),
];

/// libretro keyboard codes (`retro_key`): '0'-'9' are their ASCII codes, `*`
/// on the numeric keypad is 268, and `#` has no key of its own, so it is
/// shift-3 on most layouts; `retro_key` 35 is RETROK_HASH.
const KEYBOARD: [(c_uint, u8); 13] = [
    (48, 0),
    (49, 1),
    (50, 2),
    (51, 3),
    (52, 4),
    (53, 5),
    (54, 6),
    (55, 7),
    (56, 8),
    (57, 9),
    (42, 10),
    (268, 10),
    (35, 11),
];

/// `strings -a libcolecorust_libretro.so | grep build=` names the commit a
/// device's core was built from (see `build.rs`).
#[used]
static BUILD_STAMP: &[u8] = concat!("COLECORUST build=", env!("COLECORUST_BUILD_ID"), "\0").as_bytes();

#[repr(C)]
pub struct RetroSystemInfo {
    pub library_name: *const c_char,
    pub library_version: *const c_char,
    pub valid_extensions: *const c_char,
    pub need_fullpath: bool,
    pub block_extract: bool,
}

#[repr(C)]
pub struct RetroGameGeometry {
    pub base_width: c_uint,
    pub base_height: c_uint,
    pub max_width: c_uint,
    pub max_height: c_uint,
    pub aspect_ratio: f32,
}

#[repr(C)]
pub struct RetroSystemTiming {
    pub fps: f64,
    pub sample_rate: f64,
}

#[repr(C)]
pub struct RetroSystemAvInfo {
    pub geometry: RetroGameGeometry,
    pub timing: RetroSystemTiming,
}

#[repr(C)]
pub struct RetroGameInfo {
    pub path: *const c_char,
    pub data: *const c_void,
    pub size: usize,
    pub meta: *const c_char,
}

#[repr(C)]
pub struct RetroInputDescriptor {
    pub port: c_uint,
    pub device: c_uint,
    pub index: c_uint,
    pub id: c_uint,
    pub description: *const c_char,
}

#[repr(C)]
pub struct RetroMemoryDescriptor {
    pub flags: u64,
    pub ptr: *mut c_void,
    pub offset: usize,
    pub start: usize,
    pub select: usize,
    pub disconnect: usize,
    pub len: usize,
    pub addrspace: *const c_char,
}

#[repr(C)]
pub struct RetroMemoryMap {
    pub descriptors: *const RetroMemoryDescriptor,
    pub num_descriptors: c_uint,
}

type EnvironmentFn = unsafe extern "C" fn(c_uint, *mut c_void) -> bool;
type VideoRefreshFn = unsafe extern "C" fn(*const c_void, c_uint, c_uint, usize);
type AudioSampleFn = unsafe extern "C" fn(i16, i16);
type AudioSampleBatchFn = unsafe extern "C" fn(*const i16, usize) -> usize;
type InputPollFn = unsafe extern "C" fn();
type InputStateFn = unsafe extern "C" fn(c_uint, c_uint, c_uint, c_uint) -> i16;

/// Everything between `retro_load_game` and `retro_unload_game`. A libretro
/// core is a singleton by contract. Boxed so the RAM pointer handed to the
/// frontend's memory map stays put.
struct Core {
    machine: Box<Coleco>,
    /// The frame's audio as stereo frames, alive across the callback.
    stereo: Vec<i16>,
}

static mut CORE: Option<Core> = None;
static mut ENVIRON: Option<EnvironmentFn> = None;
static mut VIDEO: Option<VideoRefreshFn> = None;
static mut AUDIO_BATCH: Option<AudioSampleBatchFn> = None;
static mut INPUT_POLL: Option<InputPollFn> = None;
static mut INPUT_STATE: Option<InputStateFn> = None;

#[allow(static_mut_refs)]
fn core() -> Option<&'static mut Core> {
    unsafe { CORE.as_mut() }
}

#[no_mangle]
pub extern "C" fn retro_api_version() -> c_uint {
    RETRO_API_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_environment(cb: EnvironmentFn) {
    ENVIRON = Some(cb);
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_video_refresh(cb: VideoRefreshFn) {
    VIDEO = Some(cb);
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_audio_sample(_cb: AudioSampleFn) {}

#[no_mangle]
pub unsafe extern "C" fn retro_set_audio_sample_batch(cb: AudioSampleBatchFn) {
    AUDIO_BATCH = Some(cb);
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_input_poll(cb: InputPollFn) {
    INPUT_POLL = Some(cb);
}

#[no_mangle]
pub unsafe extern "C" fn retro_set_input_state(cb: InputStateFn) {
    INPUT_STATE = Some(cb);
}

#[no_mangle]
pub extern "C" fn retro_init() {}

#[no_mangle]
pub extern "C" fn retro_deinit() {
    unsafe { CORE = None };
}

#[no_mangle]
pub unsafe extern "C" fn retro_get_system_info(info: *mut RetroSystemInfo) {
    if info.is_null() {
        return;
    }
    (*info).library_name = c"ColecoRust".as_ptr();
    (*info).library_version = c"0.1.0".as_ptr();
    (*info).valid_extensions = c"col|cv|rom|bin".as_ptr();
    (*info).need_fullpath = false;
    (*info).block_extract = false;
}

/// Frames per second: the Z80 clock over 228 cycles a line and 262 lines.
fn fps() -> f64 {
    coleco_core::CPU_HZ as f64 / (coleco_core::machine::CYCLES_PER_LINE as f64 * 262.0)
}

#[no_mangle]
pub unsafe extern "C" fn retro_get_system_av_info(info: *mut RetroSystemAvInfo) {
    if info.is_null() {
        return;
    }
    (*info).geometry = RetroGameGeometry {
        base_width: WIDTH as c_uint,
        base_height: HEIGHT as c_uint,
        max_width: WIDTH as c_uint,
        max_height: HEIGHT as c_uint,
        aspect_ratio: 4.0 / 3.0,
    };
    (*info).timing = RetroSystemTiming { fps: fps(), sample_rate: coleco_core::psg::SAMPLE_RATE as f64 };
}

#[no_mangle]
pub extern "C" fn retro_set_controller_port_device(_port: c_uint, _device: c_uint) {}

#[no_mangle]
pub unsafe extern "C" fn retro_reset() {
    let Some(c) = core() else { return };
    // A console reset is a power cycle here: the HLE boots the cartridge again.
    if let Ok(m) = Coleco::new(Firmware::Hle, c.machine.cartridge()) {
        *c.machine = m;
    }
}

#[no_mangle]
pub unsafe extern "C" fn retro_load_game(game: *const RetroGameInfo) -> bool {
    if game.is_null() || (*game).data.is_null() || (*game).size == 0 {
        return false;
    }
    let rom = std::slice::from_raw_parts((*game).data as *const u8, (*game).size);
    let Ok(machine) = Coleco::new(Firmware::Hle, rom) else { return false };

    if let Some(env) = ENVIRON {
        let mut fmt = RETRO_PIXEL_FORMAT_XRGB8888;
        if !env(RETRO_ENVIRONMENT_SET_PIXEL_FORMAT, &mut fmt as *mut _ as *mut c_void) {
            return false;
        }
    }
    CORE = Some(Core { machine: Box::new(machine), stereo: Vec::new() });
    if let Some(env) = ENVIRON {
        set_input_descriptors(env);
        set_memory_maps(env);
    }
    true
}

unsafe fn set_input_descriptors(env: EnvironmentFn) {
    let mut d = Vec::new();
    for port in 0..2 {
        let named: [(c_uint, &'static std::ffi::CStr); 16] = [
            (JOY_UP, c"Up"),
            (JOY_DOWN, c"Down"),
            (JOY_LEFT, c"Left"),
            (JOY_RIGHT, c"Right"),
            (JOY_B, c"Left fire"),
            (JOY_A, c"Right fire"),
            (JOY_START, c"Keypad 1"),
            (JOY_SELECT, c"Keypad 2"),
            (JOY_X, c"Keypad 3"),
            (JOY_Y, c"Keypad 4"),
            (JOY_L, c"Keypad *"),
            (JOY_R, c"Keypad #"),
            (JOY_L2, c"Keypad 5"),
            (JOY_R2, c"Keypad 6"),
            (JOY_L3, c"Keypad 7"),
            (JOY_R3, c"Keypad 8"),
        ];
        for (id, name) in named {
            d.push(RetroInputDescriptor { port, device: RETRO_DEVICE_JOYPAD, index: 0, id, description: name.as_ptr() });
        }
    }
    d.push(RetroInputDescriptor { port: 0, device: 0, index: 0, id: 0, description: std::ptr::null() });
    env(RETRO_ENVIRONMENT_SET_INPUT_DESCRIPTORS, d.as_mut_ptr() as *mut c_void);
}

/// The 1 KB of work RAM, where the CPU sees it (`$6000`, mirrored to
/// `$7FFF`). RetroAchievements needs this or hangs on "waiting for core
/// memory map"; the RAM is also SYSTEM_RAM through `retro_get_memory_data`.
unsafe fn set_memory_maps(env: EnvironmentFn) {
    let Some(c) = core() else { return };
    let desc = [RetroMemoryDescriptor {
        flags: RETRO_MEMDESC_SYSTEM_RAM,
        ptr: c.machine.bus.ram.as_mut_ptr() as *mut c_void,
        offset: 0,
        start: 0x6000,
        // Decode A15-A13 against $6000 and ignore A12-A10: the eight mirrors.
        select: 0xe000,
        disconnect: 0x1c00,
        len: coleco_core::WORK_RAM,
        addrspace: std::ptr::null(),
    }];
    let mut map = RetroMemoryMap { descriptors: desc.as_ptr(), num_descriptors: 1 };
    env(RETRO_ENVIRONMENT_SET_MEMORY_MAPS, &mut map as *mut _ as *mut c_void);
}

#[no_mangle]
pub unsafe extern "C" fn retro_load_game_special(_ty: c_uint, _info: *const RetroGameInfo, _num: usize) -> bool {
    false
}

#[no_mangle]
pub extern "C" fn retro_unload_game() {
    unsafe { CORE = None };
}

#[no_mangle]
pub extern "C" fn retro_get_region() -> c_uint {
    RETRO_REGION_NTSC
}

/// One controller as the frontend reports it.
unsafe fn read_pad(state: InputStateFn, port: c_uint) -> Pad {
    let held = |id| state(port, RETRO_DEVICE_JOYPAD, 0, id) != 0;
    let mut key = KEYPAD_BUTTONS.iter().find(|&&(id, _)| held(id)).map(|&(_, k)| k);
    if port == 0 && key.is_none() {
        key = KEYBOARD.iter().find(|&&(code, _)| state(0, RETRO_DEVICE_KEYBOARD, 0, code) != 0).map(|&(_, k)| k);
    }
    Pad {
        up: held(JOY_UP),
        down: held(JOY_DOWN),
        left: held(JOY_LEFT),
        right: held(JOY_RIGHT),
        fire_left: held(JOY_B),
        fire_right: held(JOY_A),
        key,
    }
}

#[no_mangle]
pub unsafe extern "C" fn retro_run() {
    if let Some(poll) = INPUT_POLL {
        poll();
    }
    let Some(c) = core() else { return };
    if let Some(state) = INPUT_STATE {
        for port in 0..2 {
            c.machine.bus.pads[port as usize] = read_pad(state, port);
        }
    }
    c.machine.run_frame();
    if let Some(video) = VIDEO {
        video(c.machine.framebuffer().as_ptr() as *const c_void, WIDTH as c_uint, HEIGHT as c_uint, WIDTH * 4);
    }
    // The chip is mono; the frontend takes stereo frames. A frame's worth
    // arrives even in silence (zeros), or a frontend starves its own timing.
    let mono = c.machine.take_audio();
    c.stereo.clear();
    c.stereo.extend(mono.iter().flat_map(|&s| [s, s]));
    if let Some(audio) = AUDIO_BATCH {
        if !c.stereo.is_empty() {
            audio(c.stereo.as_ptr(), c.stereo.len() / 2);
        }
    }
}

// ---- save states ----

#[no_mangle]
pub extern "C" fn retro_serialize_size() -> usize {
    core().map_or(0, |c| c.machine.save_state().len())
}

#[no_mangle]
pub unsafe extern "C" fn retro_serialize(data: *mut c_void, size: usize) -> bool {
    let Some(c) = core() else { return false };
    let snap = c.machine.save_state();
    if data.is_null() || size < snap.len() {
        return false;
    }
    std::ptr::copy_nonoverlapping(snap.as_ptr(), data as *mut u8, snap.len());
    true
}

#[no_mangle]
pub unsafe extern "C" fn retro_unserialize(data: *const c_void, size: usize) -> bool {
    let Some(c) = core() else { return false };
    if data.is_null() {
        return false;
    }
    let len = c.machine.save_state().len().min(size);
    c.machine.load_state(std::slice::from_raw_parts(data as *const u8, len)).is_ok()
}

// ---- memory interface ----

#[no_mangle]
pub extern "C" fn retro_get_memory_data(id: c_uint) -> *mut c_void {
    match (core(), id) {
        (Some(c), RETRO_MEMORY_SYSTEM_RAM) => c.machine.bus.ram.as_mut_ptr() as *mut c_void,
        _ => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn retro_get_memory_size(id: c_uint) -> usize {
    match (core(), id) {
        (Some(_), RETRO_MEMORY_SYSTEM_RAM) => coleco_core::WORK_RAM,
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn retro_cheat_reset() {}

#[no_mangle]
pub unsafe extern "C" fn retro_cheat_set(_index: c_uint, _enabled: bool, _code: *const c_char) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The core is a process-wide singleton, so these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    static mut PIXEL_FORMAT: c_uint = 99;
    static mut FRAMES: u32 = 0;
    static mut LAST_DIMS: (c_uint, c_uint, usize) = (0, 0, 0);
    static mut AUDIO_FRAMES: usize = 0;
    static mut MAP_START: usize = 0;
    static mut PRESS_START: bool = false;

    unsafe extern "C" fn env(cmd: c_uint, data: *mut c_void) -> bool {
        match cmd {
            RETRO_ENVIRONMENT_SET_PIXEL_FORMAT => {
                PIXEL_FORMAT = *(data as *const c_uint);
                true
            }
            RETRO_ENVIRONMENT_SET_MEMORY_MAPS => {
                let map = &*(data as *const RetroMemoryMap);
                MAP_START = (*map.descriptors).start;
                true
            }
            _ => false,
        }
    }
    unsafe extern "C" fn video(_: *const c_void, w: c_uint, h: c_uint, pitch: usize) {
        FRAMES += 1;
        LAST_DIMS = (w, h, pitch);
    }
    unsafe extern "C" fn audio(_: *const i16, frames: usize) -> usize {
        AUDIO_FRAMES += frames;
        frames
    }
    unsafe extern "C" fn poll() {}
    unsafe extern "C" fn input(_port: c_uint, device: c_uint, _index: c_uint, id: c_uint) -> i16 {
        i16::from(device == RETRO_DEVICE_JOYPAD && id == JOY_START && PRESS_START)
    }

    /// A cartridge that puts the keypad key on port 1 into RAM every frame:
    /// header $55AA, start at $8100, which spins reading port $FC into $7000
    /// after selecting keypad mode.
    fn cart() -> Vec<u8> {
        let mut c = vec![0u8; 0x200];
        c[0..2].copy_from_slice(&[0x55, 0xaa]);
        c[0x0a..0x0c].copy_from_slice(&[0x00, 0x81]);
        c[0x100..0x10a].copy_from_slice(&[
            0xd3, 0x80, // OUT ($80),A: keypad mode
            0xdb, 0xfc, // IN A,($FC)
            0x32, 0x00, 0x70, // LD ($7000),A
            0x18, 0xf7, // JR back to the OUT
            0x00,
        ]);
        c
    }

    unsafe fn load() {
        retro_set_environment(env);
        retro_set_video_refresh(video);
        retro_set_audio_sample_batch(audio);
        retro_set_input_poll(poll);
        retro_set_input_state(input);
        let rom = cart();
        let info = RetroGameInfo { path: std::ptr::null(), data: rom.as_ptr() as *const c_void, size: rom.len(), meta: std::ptr::null() };
        assert!(retro_load_game(&info), "loads with no BIOS anywhere");
    }

    #[test]
    fn a_frame_is_256x192_xrgb8888_with_a_frames_worth_of_audio() {
        let _g = SERIAL.lock().unwrap();
        unsafe {
            FRAMES = 0;
            AUDIO_FRAMES = 0;
            load();
            assert_eq!(PIXEL_FORMAT, 1, "XRGB8888 is 1, not 2");
            assert_eq!(MAP_START, 0x6000, "RAM mapped for RetroAchievements");
            for _ in 0..60 {
                retro_run();
            }
            assert_eq!(FRAMES, 60);
            assert_eq!(LAST_DIMS, (256, 192, 1024));
            // 60 frames at 59.92 fps is just over a second of 44.1 kHz.
            let expect = (44_100.0 * 60.0 / fps()) as usize;
            assert!(AUDIO_FRAMES.abs_diff(expect) < 50, "{AUDIO_FRAMES} stereo frames against {expect}");
            assert_eq!(retro_get_memory_size(RETRO_MEMORY_SYSTEM_RAM), 1024);
            retro_unload_game();
        }
    }

    /// START on the RetroPad is keypad 1: the cartridge reads the keypad and
    /// sees key 1's code.
    #[test]
    fn start_is_keypad_1() {
        let _g = SERIAL.lock().unwrap();
        unsafe {
            load();
            PRESS_START = true;
            retro_run();
            PRESS_START = false;
            let ram = retro_get_memory_data(RETRO_MEMORY_SYSTEM_RAM) as *const u8;
            // Key 1's code, active low, with the right fire released.
            assert_eq!(*ram & 0x0f, 0x0d);
            retro_run();
            assert_eq!(*ram & 0x0f, 0x0f, "released");
            retro_unload_game();
        }
    }

    #[test]
    fn a_state_round_trips_through_the_abi() {
        let _g = SERIAL.lock().unwrap();
        unsafe {
            load();
            for _ in 0..10 {
                retro_run();
            }
            let size = retro_serialize_size();
            assert!(size > 16_384, "VRAM alone is 16 KB");
            let mut a = vec![0u8; size];
            assert!(retro_serialize(a.as_mut_ptr() as *mut c_void, size));
            for _ in 0..10 {
                retro_run();
            }
            assert!(retro_unserialize(a.as_ptr() as *const c_void, size));
            let mut b = vec![0u8; size];
            assert!(retro_serialize(b.as_mut_ptr() as *mut c_void, size));
            assert_eq!(a, b);
            retro_unload_game();
        }
    }
}
