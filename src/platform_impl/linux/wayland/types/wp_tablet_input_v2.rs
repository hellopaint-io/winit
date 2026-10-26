//! Handling of zwp_tablet_v2, the graphics tablet protocol.
//!
//! Compositors are not required to turn tablet tool input into `wl_pointer`
//! events for a client that never binds this protocol, and KWin no longer
//! does, so without it a pen does nothing at all.
//!
//! A tool in contact is reported the way Windows reports pen input: as a
//! `Touch` with `TouchTool::Pen` and the pen's pressure. A hovering tool moves
//! the cursor. Upstream reports both through the 0.31 pointer events
//! (`PointerKind::TabletTool`), which this version doesn't have.

use std::sync::Mutex;

use sctk::compositor::SurfaceData;
use sctk::globals::GlobalData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{
    delegate_dispatch, event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2::ZwpTabletManagerV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_dial_v2::ZwpTabletPadDialV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::{
    self, ZwpTabletPadGroupV2,
};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::{self, ZwpTabletPadV2};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_seat_v2::{self, ZwpTabletSeatV2};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2::{
    ButtonState, Event as ToolEvent, ZwpTabletToolV2,
};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_v2::{self, ZwpTabletV2};

use crate::dpi::LogicalPosition;
use crate::event::{Force, Touch, TouchPhase, TouchTool, WindowEvent};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId};

/// The linux input codes of a stylus's side buttons.
const BTN_STYLUS: u32 = 0x14b;
const BTN_STYLUS2: u32 = 0x14c;
const BTN_STYLUS3: u32 = 0x149;

/// Keeps tool touch ids clear of `wl_touch` ids, which are small integers.
const TOOL_TOUCH_ID_OFFSET: u64 = 1 << 32;

/// Tablet manager.
#[derive(Debug, Clone)]
pub struct TabletManager {
    manager: ZwpTabletManagerV2,
}

impl TabletManager {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        // Version 2 only adds pad dials and the tablet's bus type.
        let manager = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self { manager })
    }

    pub fn get_tablet_seat(
        &self,
        seat: &WlSeat,
        queue_handle: &QueueHandle<WinitState>,
    ) -> ZwpTabletSeatV2 {
        self.manager.get_tablet_seat(seat, queue_handle, GlobalData)
    }
}

#[derive(Debug, Default)]
pub struct TabletToolData {
    inner: Mutex<TabletToolState>,
}

#[derive(Debug, Default)]
struct TabletToolState {
    /// The surface the tool is in proximity of.
    surface: Option<WlSurface>,

    /// Position relative to `surface`.
    position: LogicalPosition<f64>,

    force: Option<Force>,

    /// Whether the tip is touching the tablet.
    in_contact: bool,

    /// Side buttons currently held.
    buttons_down: u8,

    /// What happened since the last `frame`.
    pending: PendingFrame,
}

#[derive(Debug, Default)]
struct PendingFrame {
    proximity_in: Option<WlSurface>,
    proximity_out: bool,
    down: bool,
    up: bool,
    moved: bool,
    pressure_changed: bool,
}

impl TabletToolState {
    /// The window of the surface the tool is over, if it is a window's main
    /// surface rather than one of its decorations.
    fn window_id(&self) -> Option<wayland::WindowId> {
        let surface = self.surface.as_ref()?;
        let data = surface.data::<SurfaceData>()?;
        data.parent_surface().is_none().then(|| wayland::make_wid(surface))
    }
}

fn device_id() -> crate::event::DeviceId {
    crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId))
}

fn scale_factor(state: &mut WinitState, window_id: wayland::WindowId) -> Option<f64> {
    state.windows.get_mut().get(&window_id).map(|window| window.lock().unwrap().scale_factor())
}

impl Dispatch<ZwpTabletManagerV2, GlobalData, WinitState> for TabletManager {
    fn event(
        _: &mut WinitState,
        _: &ZwpTabletManagerV2,
        _: <ZwpTabletManagerV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("no events defined for zwp_tablet_manager_v2");
    }
}

impl Dispatch<ZwpTabletSeatV2, GlobalData, WinitState> for TabletManager {
    event_created_child!(WinitState, ZwpTabletSeatV2, [
        zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (ZwpTabletV2, GlobalData),
        zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (ZwpTabletToolV2, TabletToolData::default()),
        zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (ZwpTabletPadV2, GlobalData),
    ]);

    fn event(
        _: &mut WinitState,
        _: &ZwpTabletSeatV2,
        _: <ZwpTabletSeatV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpTabletToolV2, TabletToolData, WinitState> for TabletManager {
    fn event(
        state: &mut WinitState,
        tool: &ZwpTabletToolV2,
        event: <ZwpTabletToolV2 as Proxy>::Event,
        data: &TabletToolData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        let mut data = data.inner.lock().unwrap();
        let touch_id = TOOL_TOUCH_ID_OFFSET + tool.id().protocol_id() as u64;

        match event {
            ToolEvent::ProximityIn { surface, .. } => data.pending.proximity_in = Some(surface),
            ToolEvent::ProximityOut => data.pending.proximity_out = true,
            ToolEvent::Down { .. } => data.pending.down = true,
            ToolEvent::Up => data.pending.up = true,
            ToolEvent::Motion { x, y } => {
                data.position = LogicalPosition::new(x, y);
                data.pending.moved = true;
            },
            ToolEvent::Pressure { pressure } => {
                data.force = Some(Force::Normalized(pressure as f64 / u16::MAX as f64));
                data.pending.pressure_changed = true;
            },
            ToolEvent::Button { button, state: WEnum::Value(button_state), .. } => {
                let bit = match button {
                    BTN_STYLUS => 1,
                    BTN_STYLUS2 => 2,
                    BTN_STYLUS3 => 4,
                    _ => return,
                };
                match button_state {
                    ButtonState::Pressed => data.buttons_down |= bit,
                    ButtonState::Released => data.buttons_down &= !bit,
                    _ => (),
                }
            },
            ToolEvent::Frame { .. } => {
                let pending = std::mem::take(&mut data.pending);
                if let Some(surface) = &pending.proximity_in {
                    data.surface = Some(surface.clone());
                }

                let Some(window_id) = data.window_id() else {
                    if pending.proximity_out {
                        data.surface = None;
                    }
                    return;
                };
                let Some(scale_factor) = scale_factor(state, window_id) else {
                    return;
                };

                let device_id = device_id();
                let position = data.position.to_physical(scale_factor);
                let touch = |phase, data: &TabletToolState| {
                    WindowEvent::Touch(Touch {
                        device_id,
                        phase,
                        location: position,
                        force: data.force,
                        // Windows reports the barrel button on the touch, so
                        // pen input looks the same on both.
                        secondary_button: data.buttons_down != 0,
                        tool: TouchTool::Pen,
                        id: touch_id,
                    })
                };
                let sink = &mut state.events_sink;

                if pending.proximity_in.is_some() {
                    sink.push_window_event(WindowEvent::CursorEntered { device_id }, window_id);
                }

                if pending.down && !data.in_contact {
                    data.in_contact = true;
                    sink.push_window_event(touch(TouchPhase::Started, &data), window_id);
                } else if data.in_contact && (pending.moved || pending.pressure_changed) {
                    sink.push_window_event(touch(TouchPhase::Moved, &data), window_id);
                } else if !data.in_contact && pending.moved {
                    sink.push_window_event(
                        WindowEvent::CursorMoved { device_id, position },
                        window_id,
                    );
                }

                // The compositor sends `up` before `proximity_out`, but a tool
                // can't be in contact once it's gone either way.
                if data.in_contact && (pending.up || pending.proximity_out) {
                    data.in_contact = false;
                    sink.push_window_event(touch(TouchPhase::Ended, &data), window_id);
                }

                if pending.proximity_out {
                    sink.push_window_event(WindowEvent::CursorLeft { device_id }, window_id);
                    data.surface = None;
                    data.force = None;
                    data.buttons_down = 0;
                }
            },
            ToolEvent::Removed => {
                if let Some(window_id) = data.window_id().filter(|_| data.in_contact) {
                    let scale_factor = scale_factor(state, window_id).unwrap_or(1.0);
                    state.events_sink.push_window_event(
                        WindowEvent::Touch(Touch {
                            device_id: device_id(),
                            phase: TouchPhase::Cancelled,
                            location: data.position.to_physical(scale_factor),
                            force: None,
                            secondary_button: false,
                            tool: TouchTool::Pen,
                            id: touch_id,
                        }),
                        window_id,
                    );
                }
                tool.destroy();
            },
            _ => (),
        }
    }
}

impl Dispatch<ZwpTabletV2, GlobalData, WinitState> for TabletManager {
    fn event(
        _: &mut WinitState,
        tablet: &ZwpTabletV2,
        event: <ZwpTabletV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        if let zwp_tablet_v2::Event::Removed = event {
            tablet.destroy();
        }
    }
}

// Pads (the buttons, rings and strips on the tablet itself) aren't reported,
// but the compositor creates their objects regardless, so they need handlers.

impl Dispatch<ZwpTabletPadV2, GlobalData, WinitState> for TabletManager {
    event_created_child!(WinitState, ZwpTabletPadV2, [
        zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (ZwpTabletPadGroupV2, GlobalData),
    ]);

    fn event(
        _: &mut WinitState,
        pad: &ZwpTabletPadV2,
        event: <ZwpTabletPadV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        if let zwp_tablet_pad_v2::Event::Removed = event {
            pad.destroy();
        }
    }
}

impl Dispatch<ZwpTabletPadGroupV2, GlobalData, WinitState> for TabletManager {
    event_created_child!(WinitState, ZwpTabletPadGroupV2, [
        zwp_tablet_pad_group_v2::EVT_RING_OPCODE => (ZwpTabletPadRingV2, GlobalData),
        zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => (ZwpTabletPadStripV2, GlobalData),
        zwp_tablet_pad_group_v2::EVT_DIAL_OPCODE => (ZwpTabletPadDialV2, GlobalData),
    ]);

    fn event(
        _: &mut WinitState,
        _: &ZwpTabletPadGroupV2,
        _: <ZwpTabletPadGroupV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpTabletPadRingV2, GlobalData, WinitState> for TabletManager {
    fn event(
        _: &mut WinitState,
        _: &ZwpTabletPadRingV2,
        _: <ZwpTabletPadRingV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpTabletPadStripV2, GlobalData, WinitState> for TabletManager {
    fn event(
        _: &mut WinitState,
        _: &ZwpTabletPadStripV2,
        _: <ZwpTabletPadStripV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpTabletPadDialV2, GlobalData, WinitState> for TabletManager {
    fn event(
        _: &mut WinitState,
        _: &ZwpTabletPadDialV2,
        _: <ZwpTabletPadDialV2 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

delegate_dispatch!(WinitState: [ZwpTabletManagerV2: GlobalData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletSeatV2: GlobalData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletToolV2: TabletToolData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletV2: GlobalData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletPadV2: GlobalData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletPadGroupV2: GlobalData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletPadRingV2: GlobalData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletPadStripV2: GlobalData] => TabletManager);
delegate_dispatch!(WinitState: [ZwpTabletPadDialV2: GlobalData] => TabletManager);
