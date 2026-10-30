use std::collections::HashMap;

use smithay::backend::input::TouchSlot;
use smithay::output::Output;
use smithay::utils::{Logical, Point};

use super::ZOOM_TOUCH_PINCH_FINGERS;

/// Physical input ownership is independent of an output's zoom motion.
#[derive(Debug, Default)]
pub struct ZoomInputState {
    touchpad: TouchpadRoute,
    touchscreen: TouchscreenSequence,
}

#[derive(Debug, Clone, Default)]
pub(super) enum TouchpadRoute {
    #[default]
    Idle,
    Forwarded,
    Consumed {
        output: Output,
    },
}

#[derive(Debug, Clone)]
pub(super) enum PinchSource {
    Touchpad,
    Touchscreen { initial_distance: f64 },
}

#[derive(Debug, Default)]
struct TouchscreenSequence {
    /// Global screen positions, not the inverse-transformed positions sent to clients.
    contacts: HashMap<TouchSlot, Point<f64, Logical>>,
    phase: TouchscreenPhase,
}

#[derive(Debug, Default)]
enum TouchscreenPhase {
    #[default]
    Eligible,
    Pending {
        output: Output,
        initial_distance: f64,
        initial_midpoint: Point<f64, Logical>,
        last_distance: f64,
        last_midpoint: Point<f64, Logical>,
    },
    Consumed {
        output: Output,
        initial_distance: f64,
    },
    Draining {
        retained_owner: Option<Output>,
    },
}

/// End zoom motion now; release routing ownership only when the contacts are empty.
pub(super) struct TouchscreenEnd {
    pub output: Output,
    pub released: bool,
}

impl ZoomInputState {
    pub(super) fn touchpad_route(&self) -> TouchpadRoute {
        self.touchpad.clone()
    }

    /// The single admission point for consumed ownership across both sources.
    /// A failed touchscreen admission stays rejected until the contact sequence ends.
    pub(super) fn begin_pinch(
        &mut self,
        output: &Output,
        source: PinchSource,
        begin_motion: impl FnOnce() -> bool,
    ) -> bool {
        let available = !self.has_consumed_owner();
        match source {
            PinchSource::Touchpad => {
                if !matches!(self.touchpad, TouchpadRoute::Idle) {
                    return false;
                }
                if matches!(self.touchscreen.phase, TouchscreenPhase::Pending { .. }) {
                    self.drain_touchscreen();
                }
                if available && begin_motion() {
                    self.touchpad = TouchpadRoute::Consumed {
                        output: output.clone(),
                    };
                    true
                } else {
                    self.touchpad = TouchpadRoute::Forwarded;
                    false
                }
            }
            PinchSource::Touchscreen { initial_distance } => {
                if !matches!(
                    &self.touchscreen.phase,
                    TouchscreenPhase::Pending {
                        output: candidate_output,
                        ..
                    } if candidate_output == output
                ) || self.touchscreen.contacts.len() != ZOOM_TOUCH_PINCH_FINGERS
                {
                    return false;
                }
                if available && begin_motion() {
                    self.touchscreen.phase = TouchscreenPhase::Consumed {
                        output: output.clone(),
                        initial_distance,
                    };
                    true
                } else {
                    self.drain_touchscreen();
                    false
                }
            }
        }
    }

    pub(super) fn forward_touchpad_begin(&mut self) {
        if matches!(self.touchpad, TouchpadRoute::Idle) {
            self.touchpad = TouchpadRoute::Forwarded;
        }
    }

    pub(super) fn end_touchpad(&mut self) -> TouchpadRoute {
        std::mem::take(&mut self.touchpad)
    }

    pub(super) fn has_consumed_owner(&self) -> bool {
        matches!(self.touchpad, TouchpadRoute::Consumed { .. })
            || self.touchscreen.owner().is_some()
    }

    /// Pair order is irrelevant: consumers use a symmetric distance/midpoint and check both.
    pub(super) fn touchscreen_pair(&self) -> Option<[Point<f64, Logical>; 2]> {
        if matches!(self.touchscreen.phase, TouchscreenPhase::Draining { .. })
            || self.touchscreen.contacts.len() != ZOOM_TOUCH_PINCH_FINGERS
        {
            return None;
        }
        let mut positions = self.touchscreen.contacts.values().copied();
        Some([positions.next()?, positions.next()?])
    }

    pub(super) fn touchscreen_gesture(&self) -> Option<(Output, f64)> {
        match &self.touchscreen.phase {
            TouchscreenPhase::Consumed {
                output,
                initial_distance,
            } => Some((output.clone(), *initial_distance)),
            _ => None,
        }
    }

    pub(super) fn touchscreen_candidate(
        &self,
    ) -> Option<(Output, f64, Point<f64, Logical>, f64, Point<f64, Logical>)> {
        match &self.touchscreen.phase {
            TouchscreenPhase::Pending {
                output,
                initial_distance,
                initial_midpoint,
                last_distance,
                last_midpoint,
            } => Some((
                output.clone(),
                *initial_distance,
                *initial_midpoint,
                *last_distance,
                *last_midpoint,
            )),
            _ => None,
        }
    }

    pub(super) fn begin_touchscreen_candidate(
        &mut self,
        output: &Output,
        initial_distance: f64,
        initial_midpoint: Point<f64, Logical>,
    ) {
        if matches!(self.touchscreen.phase, TouchscreenPhase::Eligible)
            && self.touchscreen.contacts.len() == ZOOM_TOUCH_PINCH_FINGERS
            && initial_distance.is_finite()
            && initial_midpoint.x.is_finite()
            && initial_midpoint.y.is_finite()
        {
            self.touchscreen.phase = TouchscreenPhase::Pending {
                output: output.clone(),
                initial_distance,
                initial_midpoint,
                last_distance: initial_distance,
                last_midpoint: initial_midpoint,
            };
        }
    }

    pub(super) fn update_touchscreen_candidate(
        &mut self,
        distance: f64,
        midpoint: Point<f64, Logical>,
    ) {
        if let TouchscreenPhase::Pending {
            last_distance,
            last_midpoint,
            ..
        } = &mut self.touchscreen.phase
        {
            *last_distance = distance;
            *last_midpoint = midpoint;
        }
    }

    pub(super) fn touch_down(
        &mut self,
        slot: TouchSlot,
        screen: Point<f64, Logical>,
    ) -> Option<TouchscreenEnd> {
        self.touchscreen.contacts.insert(slot, screen);
        if self.touchscreen.contacts.len() > ZOOM_TOUCH_PINCH_FINGERS {
            self.drain_touchscreen()
        } else {
            None
        }
    }

    pub(super) fn touch_motion(&mut self, slot: TouchSlot, screen: Point<f64, Logical>) {
        if let Some(contact) = self.touchscreen.contacts.get_mut(&slot) {
            *contact = screen;
        }
    }

    pub(super) fn touch_up(&mut self, slot: TouchSlot) -> Option<TouchscreenEnd> {
        self.touchscreen.contacts.remove(&slot);
        if self.touchscreen.contacts.is_empty() {
            self.reset_touchscreen()
        } else if self.touchscreen.contacts.len() < ZOOM_TOUCH_PINCH_FINGERS {
            self.drain_touchscreen()
        } else {
            None
        }
    }

    pub(super) fn cancel_touchscreen(&mut self) -> Option<TouchscreenEnd> {
        self.touchscreen.contacts.clear();
        self.reset_touchscreen()
    }

    /// Rejection or interrupted motion cannot make remaining contacts eligible again.
    pub(super) fn drain_touchscreen(&mut self) -> Option<TouchscreenEnd> {
        if self.touchscreen.contacts.is_empty() {
            return self.reset_touchscreen();
        }
        if matches!(self.touchscreen.phase, TouchscreenPhase::Draining { .. }) {
            return None;
        }
        let retained_owner = self.touchscreen.owner().cloned();
        self.touchscreen.phase = TouchscreenPhase::Draining {
            retained_owner: retained_owner.clone(),
        };
        retained_owner.map(|output| TouchscreenEnd {
            output,
            released: false,
        })
    }

    fn reset_touchscreen(&mut self) -> Option<TouchscreenEnd> {
        debug_assert!(self.touchscreen.contacts.is_empty());
        let owner = self.touchscreen.owner().cloned();
        self.touchscreen.phase = TouchscreenPhase::Eligible;
        owner.map(|output| TouchscreenEnd {
            output,
            released: true,
        })
    }
}

impl TouchscreenSequence {
    fn owner(&self) -> Option<&Output> {
        match &self.phase {
            TouchscreenPhase::Consumed { output, .. } => Some(output),
            TouchscreenPhase::Draining { retained_owner } => retained_owner.as_ref(),
            TouchscreenPhase::Eligible | TouchscreenPhase::Pending { .. } => None,
        }
    }
}
