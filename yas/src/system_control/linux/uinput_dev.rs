//! Direct `/dev/uinput` virtual input devices for the Linux backend.
//!
//! Replaces the ydotoold socket daemon: the scanner creates its own virtual
//! pointer (REL axes + full key range) and virtual touchscreen (ABS axes)
//! through the `evdev` crate. Device access needs an explicit distribution or
//! administrator-provided udev rule; see docs/LINUX.md. No daemon is used.
//!
//! Two devices are created because no single evdev class covers our needs:
//!
//! - **pointer** (`REL_X`/`REL_Y`/wheels + keys): hovering, wheel scrolling
//!   and keyboard injection. It is relative because hover and the wheel need
//!   real pointer semantics, not because uinput cannot do absolute motion — an
//!   absolute *pointer* (the usb-tablet shape) would have its axis values
//!   mapped onto the screen by the compositor's device-to-output assignment and
//!   output scale, which a client cannot set. Relative motion goes through the
//!   compositor's pointer-acceleration and display-scale conversion, so
//!   positioning uses the corner-slam / learned-factor / readback scheme in
//!   `linux_control`.
//! - **touch** (`ABS_X`/`ABS_Y` + `BTN_TOUCH`, `INPUT_PROP_DIRECT`): clicks
//!   land at *exact* absolute coordinates — no slam, no acceleration, no
//!   scale conversion. A touchscreen has no hover, so the relative pointer
//!   above stays responsible for hover-dependent input (the wheel).

use anyhow::{anyhow, Result};
use evdev::{
    uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputEvent, InputId,
    KeyCode, PropType, RelativeAxisCode, UinputAbsSetup,
};

/// evdev event types (uinput takes the raw values; evdev names them
/// SYNCHRONIZATION/KEY/RELATIVE/ABSOLUTE).
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

/// US-QWERTY fallback for characters when no live keymap is available to ask
/// (evdev keycodes are physical-key codes, so char→code is layout knowledge).
pub fn qwerty_evdev_code(c: char) -> Option<KeyCode> {
    Some(match c.to_ascii_lowercase() {
        '1' => KeyCode::KEY_1,
        '2' => KeyCode::KEY_2,
        '3' => KeyCode::KEY_3,
        '4' => KeyCode::KEY_4,
        '5' => KeyCode::KEY_5,
        '6' => KeyCode::KEY_6,
        '7' => KeyCode::KEY_7,
        '8' => KeyCode::KEY_8,
        '9' => KeyCode::KEY_9,
        '0' => KeyCode::KEY_0,
        '-' => KeyCode::KEY_MINUS,
        '=' => KeyCode::KEY_EQUAL,
        'q' => KeyCode::KEY_Q,
        'w' => KeyCode::KEY_W,
        'e' => KeyCode::KEY_E,
        'r' => KeyCode::KEY_R,
        't' => KeyCode::KEY_T,
        'y' => KeyCode::KEY_Y,
        'u' => KeyCode::KEY_U,
        'i' => KeyCode::KEY_I,
        'o' => KeyCode::KEY_O,
        'p' => KeyCode::KEY_P,
        '[' => KeyCode::KEY_LEFTBRACE,
        ']' => KeyCode::KEY_RIGHTBRACE,
        'a' => KeyCode::KEY_A,
        's' => KeyCode::KEY_S,
        'd' => KeyCode::KEY_D,
        'f' => KeyCode::KEY_F,
        'g' => KeyCode::KEY_G,
        'h' => KeyCode::KEY_H,
        'j' => KeyCode::KEY_J,
        'k' => KeyCode::KEY_K,
        'l' => KeyCode::KEY_L,
        ';' => KeyCode::KEY_SEMICOLON,
        '\'' => KeyCode::KEY_APOSTROPHE,
        '`' => KeyCode::KEY_GRAVE,
        '\\' => KeyCode::KEY_BACKSLASH,
        'z' => KeyCode::KEY_Z,
        'x' => KeyCode::KEY_X,
        'c' => KeyCode::KEY_C,
        'v' => KeyCode::KEY_V,
        'b' => KeyCode::KEY_B,
        'n' => KeyCode::KEY_N,
        'm' => KeyCode::KEY_M,
        ',' => KeyCode::KEY_COMMA,
        '.' => KeyCode::KEY_DOT,
        '/' => KeyCode::KEY_SLASH,
        ' ' => KeyCode::KEY_SPACE,
        _ => return None,
    })
}

pub const POINTER_NAME: &str = "GOODScanner virtual pointer";
pub const TOUCH_NAME: &str = "GOODScanner virtual touch";
const VENDOR: u16 = 0x1995;
const POINTER_PRODUCT: u16 = 0x0001;
const TOUCH_PRODUCT: u16 = 0x0002;
/// Nominal touchscreen axis range. The compositor maps the whole range onto
/// the output, so only the ratio matters, not the absolute numbers.
const TOUCH_AXIS_MAX: i32 = 16383;

/// The virtual devices owned by this process.
pub struct UinputDevices {
    pointer: VirtualDevice,
    touch: Option<VirtualDevice>,
}

impl UinputDevices {
    /// Create the virtual devices; fails when `/dev/uinput` is inaccessible.
    pub fn create() -> Result<Self> {
        // The scanner emits arbitrary keycodes resolved from the session
        // keymap, so register the whole keyspace (KEY_CNT entries) — the same
        // thing ydotoold's daemon does with its hand-written key list.
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=0x2ffu16 {
            keys.insert(KeyCode::new(code));
        }

        let mut rel_axes = AttributeSet::<RelativeAxisCode>::new();
        rel_axes.insert(RelativeAxisCode::REL_X);
        rel_axes.insert(RelativeAxisCode::REL_Y);
        rel_axes.insert(RelativeAxisCode::REL_WHEEL);
        rel_axes.insert(RelativeAxisCode::REL_HWHEEL);

        let builder =
            VirtualDevice::builder().map_err(|e| anyhow!("cannot open /dev/uinput: {e}"))?;
        let pointer = builder
            .name(POINTER_NAME)
            .input_id(InputId::new(
                BusType::BUS_VIRTUAL,
                VENDOR,
                POINTER_PRODUCT,
                1,
            ))
            .with_keys(&keys)?
            .with_relative_axes(&rel_axes)?
            .build()?;

        // Best effort: without the touchscreen device clicks fall back to the
        // relative pointer.
        let touch = match create_touch() {
            Ok(dev) => Some(dev),
            Err(e) => {
                log_debug!(
                    "触摸设备不可用，点击将走相对指针: {e}",
                    "touch device unavailable; clicks will use the relative pointer: {e}"
                );
                None
            },
        };

        Ok(Self { pointer, touch })
    }

    /// Whether absolute touchscreen clicks are available.
    pub fn has_touch(&self) -> bool {
        self.touch.is_some()
    }

    /// Relative pointer motion; must be its own report/frame when combined
    /// with [`Self::pointer_slam`] (see `linux_control`).
    pub fn pointer_move_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.pointer.emit(&[
            InputEvent::new(EV_REL, RelativeAxisCode::REL_X.0, dx),
            InputEvent::new(EV_REL, RelativeAxisCode::REL_Y.0, dy),
        ])?;
        Ok(())
    }

    /// Slam the pointer into the top-left corner (clamped by libinput).
    pub fn pointer_slam(&mut self) -> Result<()> {
        self.pointer.emit(&[
            InputEvent::new(EV_REL, RelativeAxisCode::REL_X.0, i32::MIN),
            InputEvent::new(EV_REL, RelativeAxisCode::REL_Y.0, i32::MIN),
        ])?;
        Ok(())
    }

    pub fn pointer_button(&mut self, pressed: bool) -> Result<()> {
        self.pointer.emit(&[InputEvent::new(
            EV_KEY,
            KeyCode::BTN_LEFT.0,
            i32::from(pressed),
        )])?;
        Ok(())
    }

    /// Left-click through the relative pointer.
    pub fn pointer_click(&mut self) -> Result<()> {
        self.pointer
            .emit(&[InputEvent::new(EV_KEY, KeyCode::BTN_LEFT.0, 1)])?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        self.pointer
            .emit(&[InputEvent::new(EV_KEY, KeyCode::BTN_LEFT.0, 0)])?;
        Ok(())
    }

    /// Wheel by `notches` detents. The value is emitted to `REL_WHEEL`
    /// verbatim — `linux_control` owns the scroll-direction convention
    /// (positive there = down, which is a *negative* `REL_WHEEL` value) and
    /// passes the already-negated amount in.
    pub fn wheel(&mut self, notches: i32) -> Result<()> {
        self.pointer.emit(&[InputEvent::new(
            EV_REL,
            RelativeAxisCode::REL_WHEEL.0,
            notches,
        )])?;
        Ok(())
    }

    /// Press or release a key by evdev code.
    pub fn key(&mut self, code: u16, press: bool) -> Result<()> {
        self.pointer
            .emit(&[InputEvent::new(EV_KEY, code, i32::from(press))])?;
        Ok(())
    }

    /// Absolute tap on the touchscreen. `x`/`y` are physical screen pixels.
    pub fn touch_tap(&mut self, x: i32, y: i32, screen_w: i32, screen_h: i32) -> Result<()> {
        let Some(touch) = self.touch.as_mut() else {
            anyhow::bail!("touchscreen device was not created");
        };
        let axis_x = (x.max(0) as i64 * TOUCH_AXIS_MAX as i64 / screen_w.max(1) as i64) as i32;
        let axis_y = (y.max(0) as i64 * TOUCH_AXIS_MAX as i64 / screen_h.max(1) as i64) as i32;
        // Contact: position first, then touch down; libinput synthesizes the
        // pointer click for X11 clients.
        touch.emit(&[
            InputEvent::new(EV_ABS, AbsoluteAxisCode::ABS_X.0, axis_x),
            InputEvent::new(EV_ABS, AbsoluteAxisCode::ABS_Y.0, axis_y),
            InputEvent::new(EV_KEY, KeyCode::BTN_TOUCH.0, 1),
        ])?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        touch.emit(&[InputEvent::new(EV_KEY, KeyCode::BTN_TOUCH.0, 0)])?;
        Ok(())
    }
}

fn create_touch() -> std::io::Result<VirtualDevice> {
    let mut touch_keys = AttributeSet::<KeyCode>::new();
    touch_keys.insert(KeyCode::BTN_TOUCH);
    let mut props = AttributeSet::<PropType>::new();
    props.insert(PropType::DIRECT);

    VirtualDevice::builder()?
        .name(TOUCH_NAME)
        .input_id(InputId::new(BusType::BUS_VIRTUAL, VENDOR, TOUCH_PRODUCT, 1))
        .with_keys(&touch_keys)?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, TOUCH_AXIS_MAX, 0, 0, 0),
        ))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, TOUCH_AXIS_MAX, 0, 0, 0),
        ))?
        .with_properties(&props)?
        .build()
}
