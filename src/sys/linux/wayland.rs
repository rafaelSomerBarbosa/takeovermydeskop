//! The Wayland protocols the Hyprland backend works through: virtual pointer and keyboard devices
//! for input, and screencopy for screenshots. None of them needs root or a helper program.
//!
//! The protocols are shared by wlroots-style compositors, but Wayland has no way to ask where the
//! pointer is, which the emergency stop needs. That part comes from the compositor's own IPC.

use super::keys;
use crate::input::{Button, Wheel, pause};
use anyhow::{Context, Result, anyhow, bail};
use image::RgbImage;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, memfd_create};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_keyboard::KeymapFormat;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

/// How long a compositor gets to hand over a frame. It only draws when something changes, or when
/// asked to, and a monitor that is off may never answer.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Time for the focused app to take in a new keymap before the keys that need it.
const KEYMAP_SETTLE_MS: u64 = 20;
/// What one notch of a wheel scrolls, in the units of `wl_pointer.axis`.
const NOTCH: f64 = 15.0;

pub struct Wayland {
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    screencopy: ZwlrScreencopyManagerV1,
    shm: WlShm,
    started: Instant,
    /// Keys we're holding down. Key events don't change the modifier state of a virtual keyboard,
    /// so it has to be told separately.
    held: Vec<u32>,
    /// The characters on the virtual keyboard's spare keys.
    slots: Vec<char>,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    frame: Frame,
}

struct Output {
    /// The registry's name for this global, to notice when it goes away.
    global: u32,
    proxy: WlOutput,
    /// The connector, like "HDMI-A-1".
    name: String,
}

/// What the compositor has said so far about the frame being captured.
#[derive(Default)]
struct Frame {
    buffer: Option<BufferInfo>,
    /// A format offered that we can't read, to name in the error.
    unsupported: Option<wl_shm::Format>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Clone, Copy)]
struct BufferInfo {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

impl Wayland {
    pub fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env().context("couldn't connect to the Wayland compositor")?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection).context("couldn't read the compositor's globals")?;
        let qh = queue.handle();
        let missing = |name: &'static str| move |_| anyhow!("the compositor doesn't offer {name}");
        let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(missing("wl_shm"))?;
        let seat: WlSeat = globals.bind(&qh, 1..=1, ()).map_err(missing("wl_seat"))?;
        let screencopy: ZwlrScreencopyManagerV1 =
            globals.bind(&qh, 3..=3, ()).map_err(missing("wlr-screencopy (zwlr_screencopy_manager_v1)"))?;
        let pointers: ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 2..=2, ()).map_err(missing("wlr-virtual-pointer (zwlr_virtual_pointer_manager_v1)"))?;
        let keyboards: ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(missing("virtual-keyboard (zwp_virtual_keyboard_manager_v1)"))?;

        let mut state = State::default();
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                state.add_output(globals.registry(), global.name, global.version, &qh);
            }
        }
        let pointer = pointers.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboards.create_virtual_keyboard(&seat, &qh, ());
        queue.roundtrip(&mut state).context("lost the connection to the compositor")?;

        let mut wayland = Self { queue, state, pointer, keyboard, screencopy, shm, started: Instant::now(), held: Vec::new(), slots: Vec::new() };
        wayland.upload_keymap()?;
        Ok(wayland)
    }

    /// Moves the pointer to (x, y) of the `width`x`height` box around all monitors. The round trip
    /// makes sure the compositor has moved it before anyone asks it where the pointer is.
    pub fn move_pointer(&mut self, x: u32, y: u32, width: u32, height: u32) -> Result<()> {
        self.pointer.motion_absolute(self.now(), x, y, width, height);
        self.pointer.frame();
        self.sync()
    }

    pub fn button(&mut self, button: Button, down: bool) -> Result<()> {
        // BTN_LEFT, BTN_RIGHT and BTN_MIDDLE of linux/input-event-codes.h.
        let code = match button {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
        };
        let state = if down { ButtonState::Pressed } else { ButtonState::Released };
        self.pointer.button(self.now(), code, state);
        self.pointer.frame();
        self.sync()
    }

    pub fn wheel(&mut self, wheel: Wheel) -> Result<()> {
        let (axis, direction) = match wheel {
            Wheel::Up => (Axis::VerticalScroll, -1),
            Wheel::Down => (Axis::VerticalScroll, 1),
            Wheel::Left => (Axis::HorizontalScroll, -1),
            Wheel::Right => (Axis::HorizontalScroll, 1),
        };
        self.pointer.axis_source(AxisSource::Wheel);
        self.pointer.axis_discrete(self.now(), axis, NOTCH * direction as f64, direction);
        self.pointer.frame();
        self.sync()
    }

    pub fn send_keys(&mut self, codes: &[u32], up: bool) -> Result<()> {
        for &code in codes {
            let before = self.modifiers();
            if up {
                self.held.retain(|&held| held != code);
            } else {
                self.held.push(code);
            }
            self.keyboard.key(self.now(), code, !up as u32);
            let after = self.modifiers();
            if after != before {
                self.keyboard.modifiers(after, 0, 0, 0);
            }
        }
        self.sync()
    }

    /// Types `c`, which comes first in `rest`, the text still to type.
    pub fn type_char(&mut self, c: char, rest: &str) -> Result<()> {
        if let Some((code, shift)) = keys::us_key(c) {
            if shift {
                self.send_keys(&[keys::SHIFT], false)?;
            }
            let typed = self.tap(code);
            if shift {
                self.send_keys(&[keys::SHIFT], true)?;
            }
            return typed;
        }
        if c.is_control() {
            bail!("can't type the control character {c:?}");
        }
        let code = self.slot_for(c, rest)?;
        self.tap(code)
    }

    fn tap(&mut self, code: u32) -> Result<()> {
        self.send_keys(&[code], false)?;
        self.send_keys(&[code], true)
    }

    /// The spare key that types `c`. Every keymap change makes the focused app reload its keymap,
    /// so when `c` isn't on one yet, this loads as many of the characters coming up as fit.
    fn slot_for(&mut self, c: char, rest: &str) -> Result<u32> {
        if !self.slots.contains(&c) {
            self.slots.clear();
            for next in rest.chars().filter(|&next| keys::us_key(next).is_none() && !next.is_control()) {
                if self.slots.len() == keys::SLOTS {
                    break;
                }
                if !self.slots.contains(&next) {
                    self.slots.push(next);
                }
            }
            self.upload_keymap()?;
        }
        let index = self.slots.iter().position(|&slot| slot == c).context("the character didn't get a key")?;
        Ok(keys::FIRST_SLOT + index as u32)
    }

    fn upload_keymap(&mut self) -> Result<()> {
        // The compositor reads this as a C string, so the terminator counts.
        let keymap = keys::keymap(&self.slots) + "\0";
        let mut file = File::from(memfd_create("takeovermydeskop-keymap", MemfdFlags::CLOEXEC)?);
        file.write_all(keymap.as_bytes())?;
        self.keyboard.keymap(KeymapFormat::XkbV1 as u32, file.as_fd(), keymap.len() as u32);
        self.sync()?;
        pause(KEYMAP_SETTLE_MS);
        Ok(())
    }

    /// A screenshot of the output named `name`, in its real pixels, with the mouse pointer.
    pub fn capture(&mut self, name: &str) -> Result<RgbImage> {
        self.sync()?;
        let output = self.state.outputs.iter().find(|o| o.name == name).with_context(|| format!("the compositor has no output {name}"))?;
        let qh = self.queue.handle();
        self.state.frame = Frame::default();
        let frame = self.screencopy.capture_output(1, &output.proxy, &qh, ());
        let captured = self.read_frame(&frame, &qh);
        frame.destroy();
        captured
    }

    fn read_frame(&mut self, frame: &ZwlrScreencopyFrameV1, qh: &QueueHandle<State>) -> Result<RgbImage> {
        self.wait_for_frame(|frame| frame.buffer_done)?;
        let info = match (self.state.frame.buffer, self.state.frame.unsupported) {
            (Some(info), _) => info,
            (None, Some(format)) => bail!("can't read the screen's pixel format ({format:?})"),
            (None, None) => bail!("the compositor offered no way to capture the screen"),
        };
        let size = info.stride as u64 * info.height as u64;
        let file = File::from(memfd_create("takeovermydeskop-frame", MemfdFlags::CLOEXEC)?);
        file.set_len(size)?;
        let pool = self.shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, info.width as i32, info.height as i32, info.stride as i32, info.format, qh, ());
        frame.copy(&buffer);
        let copied = self.wait_for_frame(|frame| frame.ready);
        buffer.destroy();
        pool.destroy();
        copied?;

        let mut pixels = vec![0; size as usize];
        file.read_exact_at(&mut pixels, 0)?;
        decode(&pixels, info, self.state.frame.y_invert)
    }

    /// Waits for `done`, or for the compositor to give up on the frame.
    fn wait_for_frame(&mut self, done: impl Fn(&Frame) -> bool) -> Result<()> {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        loop {
            self.queue.dispatch_pending(&mut self.state).context("lost the connection to the compositor")?;
            if self.state.frame.failed {
                bail!("the compositor refused to capture the screen; if its permission system is on, allow this program");
            }
            if done(&self.state.frame) {
                return Ok(());
            }
            self.queue.flush().context("lost the connection to the compositor")?;
            let Some(guard) = self.queue.prepare_read() else { continue };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("the compositor didn't deliver a frame; the monitor may be off");
            }
            let timeout = Timespec { tv_sec: left.as_secs() as _, tv_nsec: left.subsec_nanos() as _ };
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            if poll(&mut fds, Some(&timeout))? > 0 {
                guard.read().context("lost the connection to the compositor")?;
            }
        }
    }

    fn sync(&mut self) -> Result<()> {
        self.queue.roundtrip(&mut self.state).context("lost the connection to the compositor")?;
        Ok(())
    }

    fn modifiers(&self) -> u32 {
        self.held.iter().fold(0, |mask, &code| mask | keys::modifier_mask(code))
    }

    /// Timestamp for an event, in milliseconds from an arbitrary start.
    fn now(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
}

/// Turns a captured buffer into an image. The wl_shm formats name 32-bit little-endian words, so
/// Xrgb8888 is blue, green, red, unused in memory.
fn decode(pixels: &[u8], info: BufferInfo, y_invert: bool) -> Result<RgbImage> {
    let (red, blue) = match info.format {
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => (2, 0),
        wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => (0, 2),
        other => bail!("can't read the screen's pixel format ({other:?})"),
    };
    let mut rgb = Vec::with_capacity(info.width as usize * info.height as usize * 3);
    for y in 0..info.height {
        let row = if y_invert { info.height - 1 - y } else { y };
        let start = row as usize * info.stride as usize;
        for pixel in pixels[start..start + info.width as usize * 4].chunks_exact(4) {
            rgb.extend_from_slice(&[pixel[red], pixel[1], pixel[blue]]);
        }
    }
    RgbImage::from_raw(info.width, info.height, rgb).context("captured buffer has the wrong size")
}

impl State {
    fn add_output(&mut self, registry: &WlRegistry, global: u32, version: u32, qh: &QueueHandle<State>) {
        // Version 4 is the first that tells its connector's name.
        let proxy = registry.bind::<WlOutput, _, _>(global, version.min(4), qh, global);
        self.outputs.push(Output { global, proxy, name: String::new() });
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                state.add_output(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => state.outputs.retain(|output| output.global != name),
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event
            && let Some(output) = state.outputs.iter_mut().find(|output| output.global == *global)
        {
            output.name = name;
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::{Event, Flags};
        let frame = &mut state.frame;
        match event {
            Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                if matches!(
                    format,
                    wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 | wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888
                ) {
                    frame.buffer.get_or_insert(BufferInfo { format, width, height, stride });
                } else {
                    frame.unsupported = Some(format);
                }
            }
            Event::BufferDone => frame.buffer_done = true,
            Event::Flags { flags: WEnum::Value(flags) } => frame.y_invert = flags.contains(Flags::YInvert),
            Event::Ready { .. } => frame.ready = true,
            Event::Failed => frame.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel_at(format: wl_shm::Format, bytes: [u8; 4]) -> [u8; 3] {
        let info = BufferInfo { format, width: 1, height: 1, stride: 4 };
        decode(&bytes, info, false).unwrap().get_pixel(0, 0).0
    }

    #[test]
    fn reads_both_channel_orders() {
        assert_eq!(pixel_at(wl_shm::Format::Xrgb8888, [1, 2, 3, 0]), [3, 2, 1]);
        assert_eq!(pixel_at(wl_shm::Format::Abgr8888, [1, 2, 3, 0]), [1, 2, 3]);
    }

    #[test]
    fn skips_row_padding_and_flips_inverted_frames() {
        // Two rows of one pixel, each padded to 8 bytes.
        let pixels = [10, 10, 10, 0, 99, 99, 99, 99, 20, 20, 20, 0, 99, 99, 99, 99];
        let info = BufferInfo { format: wl_shm::Format::Xrgb8888, width: 1, height: 2, stride: 8 };
        let upright = decode(&pixels, info, false).unwrap();
        assert_eq!(upright.get_pixel(0, 0).0, [10, 10, 10]);
        let flipped = decode(&pixels, info, true).unwrap();
        assert_eq!(flipped.get_pixel(0, 0).0, [20, 20, 20]);
    }
}
