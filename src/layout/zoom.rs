use std::time::Duration;

use niri_config::animations::{ZoomFocalPanAnim, ZoomLevelChangeAnim};
use niri_config::{Zoom, ZoomMovementMode};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, Clock};
use crate::niri::OutputState;
use crate::rubber_band::RubberBand;
use crate::utils::output_size;
use crate::utils::view::OutputViewport;

const ZOOM_CHANGE_EPSILON: f64 = 1e-4;

#[derive(Debug)]
enum ZoomTransition {
    Level(LevelAnimation),
    Pan(FocalPan),
    Gesture(ZoomGesture),
    GestureReturn(GestureReturn),
}

impl ZoomTransition {
    fn level_animation(&self) -> Option<&Animation> {
        match self {
            Self::Level(level) => Some(&level.animation),
            Self::GestureReturn(gesture) => Some(&gesture.animation),
            _ => None,
        }
    }
}

/// Ordinary level animation owns tracking; an independent pan cannot coexist with it.
#[derive(Debug)]
struct LevelAnimation {
    animation: Animation,
    tracking: TrackedCorrection,
}

/// Active cumulative pinch input. Unlike workspace swipes, scale is supplied cumulatively
/// and release has no fling, so neither a SwipeTracker nor another animation is needed here.
#[derive(Debug)]
struct ZoomGesture {
    start_level: f64,
    focal: GestureFocal,
}

impl OutputState {
    pub fn zoom(&self) -> &OutputZoomState {
        &self.zoom
    }

    /// The obligatory compositor boundary for all zoom actions, including deferred callbacks.
    pub(crate) fn update_zoom(&mut self, action: ZoomAction) -> bool {
        self.zoom.update(action)
    }
}

impl ZoomGesture {
    /// Input supplies cumulative scale, so every sample is a fresh, nonmutating computation.
    fn sample_level(&self, scale: f64, sensitivity: f64, max_level: f64) -> Option<f64> {
        if !scale.is_finite() || scale <= 0. || !sensitivity.is_finite() || sensitivity < 0. {
            return None;
        }
        let log = sensitivity * scale.ln();
        if !log.is_finite() {
            return None;
        }
        let level = RubberBand {
            stiffness: 0.5,
            limit: 0.05,
        }
        .clamp(0., max_level.ln(), self.start_level.ln() + log)
        .exp()
        .max(1.);
        if !level.is_finite() {
            return None;
        }
        Some(level)
    }
}

/// Release moves the active gesture's anchor here; no second focal owner survives alongside it.
#[derive(Debug)]
struct GestureReturn {
    animation: Animation,
    focal: GestureFocal,
}

/// A pan's policy is independent of its interpolation. Mode restoration deliberately
/// keeps the content pointer fixed; explicit CursorFollow unlock pins its screen tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanPurpose {
    Unlock,
    ModeChange,
}

impl PanPurpose {
    fn pointer_effect(self, mode: ZoomMovementMode) -> PointerEffect {
        if self == Self::ModeChange || (self == Self::Unlock && mode == ZoomMovementMode::Centered)
        {
            PointerEffect::KeepContent
        } else {
            PointerEffect::PreserveScreen
        }
    }
}

/// Independent focal pan; physical pointer handoff stops it at the displayed geometry.
#[derive(Debug)]
struct FocalPan {
    animation: PointAnimation,
    purpose: PanPurpose,
}

/// Content/screen relationship owned by either active pinch or its coordinated return.
#[derive(Debug)]
struct GestureFocal {
    content_anchor: Point<f64, Logical>,
    screen_midpoint: Point<f64, Logical>,
}

impl GestureFocal {
    fn constrain(&mut self, bounds: Rectangle<f64, Logical>) {
        self.content_anchor = self.content_anchor.constrain(bounds);
        self.screen_midpoint = self.screen_midpoint.constrain(bounds);
    }
}

/// Every displayed-geometry commit declares whether the actual seat needs reprojection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerEffect {
    KeepContent,
    PreserveScreen,
}

/// Closed, ephemeral requests at the compositor/zoom boundary.
pub(crate) enum ZoomAction {
    Advance,
    Resize(Size<f64, Logical>),
    Configure(Zoom, ZoomLevelChangeAnim, ZoomFocalPanAnim),
    PointerMove(Point<f64, Logical>),
    PointerHandoff(Point<f64, Logical>, ZoomLevelChangeAnim),
    SetLevel(f64, ZoomLevelChangeAnim),
    ToggleLock(ZoomLevelChangeAnim, ZoomFocalPanAnim),
    BeginGesture(Point<f64, Logical>),
    UpdateGesture(f64, f64, Point<f64, Logical>),
    EndGesture(bool, ZoomLevelChangeAnim),
    Interrupt,
}

/// Old presentation needed to preserve the actual seat's screen position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ZoomPointerChange {
    pub(crate) old_view: OutputViewport,
    pub(crate) old_size: Size<f64, Logical>,
}

/// Transition-local focal continuity, driven by the level curve rather than another animation.
#[derive(Debug, Clone, Copy)]
struct TrackedCorrection {
    offset: Point<f64, Logical>,
    start_progress: f64,
}

impl TrackedCorrection {
    fn weight(self, progress: f64, level_done: bool) -> f64 {
        if level_done {
            return 0.;
        }
        if progress == self.start_progress {
            return 1.;
        }
        if progress >= 1. {
            return 0.;
        }
        let remaining = 1. - self.start_progress;
        if remaining <= ZOOM_CHANGE_EPSILON {
            // No unstable division in the last fraction of the level curve: preserve the
            // reseed sample above, then finish with ordinary geometry.
            return 0.;
        }
        (1. - progress) / remaining
    }
}

/// Displayed values are committed on ticks (or gesture input), never sampled by view().
/// Animation presets are supplied by callers; the cloned clock shares Niri's clock.
#[derive(Debug)]
pub struct OutputZoomState {
    displayed: OutputViewport,
    locked: bool,
    max_level: f64,
    movement_mode: ZoomMovementMode,
    // Mouse-only, output-local content. Synthetic seat reprojection only records it.
    cursor: Point<f64, Logical>,
    output_size: Size<f64, Logical>,
    clock: Clock,
    transition: Option<ZoomTransition>,
    /// Earliest unacknowledged pointer change since the last seat reconciliation.
    pub(crate) pending_zoom_pointer: Option<ZoomPointerChange>,
}

impl OutputZoomState {
    pub fn new(output: &Output, clock: Clock, zoom: Zoom) -> Self {
        let output_size = output_size(output);
        let center = Point::from((output_size.w / 2., output_size.h / 2.));
        Self {
            displayed: OutputViewport::new(center, 1.),
            locked: false,
            max_level: valid_max(zoom.max_zoom),
            movement_mode: zoom.movement_mode,
            cursor: center,
            output_size,
            clock,
            transition: None,
            pending_zoom_pointer: None,
        }
    }

    pub fn view(&self) -> OutputViewport {
        self.displayed
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Capture old presentation once; no mutation caller can omit the pointer-effect obligation.
    fn update(&mut self, action: ZoomAction) -> bool {
        let old = ZoomPointerChange {
            old_view: self.displayed,
            old_size: self.output_size,
        };
        let (result, policy) = self.apply_action(action);
        if policy == PointerEffect::PreserveScreen
            && (old.old_view != self.displayed || old.old_size != self.output_size)
        {
            self.pending_zoom_pointer.get_or_insert(old);
        }
        result
    }

    fn apply_action(&mut self, action: ZoomAction) -> (bool, PointerEffect) {
        let mut policy = PointerEffect::KeepContent;
        let result = match action {
            ZoomAction::Advance => {
                policy = self.advance_animations();
                true
            }
            ZoomAction::Resize(size) => {
                policy = PointerEffect::PreserveScreen;
                self.set_output_size(size)
            }
            ZoomAction::Configure(zoom, level, focal) => self.set_config(&zoom, &level, &focal),
            ZoomAction::PointerMove(position) => {
                self.set_cursor(position);
                true
            }
            ZoomAction::PointerHandoff(position, config) => {
                self.handoff_pointer(position, config);
                true
            }
            ZoomAction::SetLevel(level, config) => {
                self.set_level(level, config);
                true
            }
            ZoomAction::ToggleLock(level, focal) => {
                self.toggle_lock(level, focal);
                true
            }
            ZoomAction::BeginGesture(screen) => self.begin_gesture(screen),
            ZoomAction::UpdateGesture(scale, sensitivity, screen) => {
                let accepted = self.update_gesture(scale, sensitivity, screen);
                if accepted {
                    policy = PointerEffect::PreserveScreen;
                }
                accepted
            }
            ZoomAction::EndGesture(cancelled, config) => self.end_gesture(cancelled, config),
            ZoomAction::Interrupt => {
                self.interrupt_pointer_driver();
                true
            }
        };
        (result, policy)
    }

    pub fn is_gesturing(&self) -> bool {
        matches!(self.transition, Some(ZoomTransition::Gesture(_)))
    }

    pub fn is_transitioning(&self) -> bool {
        self.transition.is_some()
    }

    pub fn are_animations_ongoing(&self) -> bool {
        matches!(
            self.transition,
            Some(
                ZoomTransition::Level(_)
                    | ZoomTransition::Pan(_)
                    | ZoomTransition::GestureReturn(_)
            )
        )
    }

    /// The only displayed-geometry writer after construction; no queue or seat callbacks.
    fn commit_displayed(&mut self, next: OutputViewport, size: Size<f64, Logical>) -> bool {
        if !next.factor.is_finite()
            || next.factor < 1.
            || !finite_point(next.focal)
            || !size.w.is_finite()
            || !size.h.is_finite()
            || size.w < 0.
            || size.h < 0.
        {
            return false;
        }
        let bounds = Rectangle::from_size(size);
        let next = OutputViewport {
            focal: next.focal.constrain(bounds),
            factor: next.factor,
        };
        self.displayed = next;
        self.output_size = size;
        true
    }

    pub(crate) fn record_cursor(&mut self, pos: Point<f64, Logical>) {
        if finite_point(pos) {
            self.cursor = pos.constrain(Rectangle::from_size(self.output_size));
        }
    }

    /// Invert the current mode against committed geometry, using the actual seat, not its cache.
    /// This is only a proposed output-local content position; State owns legal seat dispatch.
    pub(crate) fn pointer_handoff_target(
        &self,
        actual: Point<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        if self.locked || self.displayed.factor <= 1. || !finite_point(actual) {
            return None;
        }
        // Ordinary tracked level animations already own a compatible pointer. In particular,
        // its displayed focal may lag behind a real cursor move until the next animation tick;
        // do not mistake that frame-local lag for a stale touch anchor and warp the move away.
        if let Some(ZoomTransition::Level(level)) = &self.transition {
            if !different(level.tracking.offset, (0., 0.).into()) {
                return None;
            }
        }
        let compatible = !different(
            self.focal_for_cursor_at(
                self.displayed.factor,
                actual,
                self.tracked_anchor_for(actual),
            ),
            self.displayed.focal,
        );
        Some(match self.movement_mode {
            ZoomMovementMode::CursorFollow => self.displayed.focal,
            ZoomMovementMode::Centered if compatible => actual,
            // OnEdge derives its anchor by clamping the real cursor to the visible viewport.
            // Keep the seat where it is so pre-input handoff cannot change the legacy edge anchor.
            ZoomMovementMode::OnEdge => actual,
            _ => self.displayed.screen_to_content(Point::from((
                self.output_size.w / 2.,
                self.output_size.h / 2.,
            ))),
        })
    }

    /// Record an already-dispatched compatible seat position, never write the displayed view.
    /// Scalar animations retain their exact curve/target/timeline; touch focal ownership ends.
    fn handoff_pointer(&mut self, actual: Point<f64, Logical>, config: ZoomLevelChangeAnim) {
        if !finite_point(actual) {
            return;
        }
        self.record_cursor(actual);
        let transition = self.transition.take();
        self.transition = match transition {
            Some(ZoomTransition::Level(mut level)) => {
                // OnEdge already has zero correction and uses a fresh live anchor each tick.
                // Keep its initial/reseed guard rather than inventing another start sample.
                if self.movement_mode != ZoomMovementMode::OnEdge {
                    level.tracking = self.tracking_for(&level.animation);
                }
                Some(ZoomTransition::Level(level))
            }
            Some(ZoomTransition::GestureReturn(gesture)) => {
                let tracking = self.tracking_for(&gesture.animation);
                Some(ZoomTransition::Level(LevelAnimation {
                    animation: gesture.animation,
                    tracking,
                }))
            }
            Some(ZoomTransition::Gesture(_)) => {
                // Commit the current legal scalar, or settle only its rubber-band excess.
                if self.displayed.factor > self.max_level {
                    let animation = Animation::new(
                        self.clock.clone(),
                        self.displayed.factor,
                        self.max_level,
                        0.,
                        config.0,
                    );
                    let tracking = self.tracking_for(&animation);
                    Some(ZoomTransition::Level(LevelAnimation {
                        animation,
                        tracking,
                    }))
                } else {
                    None
                }
            }
            Some(ZoomTransition::Pan(_)) | None => None,
        };
    }

    fn advance_animations(&mut self) -> PointerEffect {
        let old = self.view();
        // OnEdge captures the OLD displayed geometry on every tick, not the start geometry.
        let anchor = self.tracked_anchor();
        let mut next = old;
        let mut done = false;
        let mut pointer_effect = PointerEffect::KeepContent;
        match &self.transition {
            Some(ZoomTransition::Level(level)) => {
                done = level.animation.is_done();
                next.factor = self.sample_level_animation(&level.animation, done);
                if !self.locked {
                    let correction = level.tracking;
                    let progress = level_progress_at(&level.animation, next.factor);
                    let focal = self.focal_for_at(next.factor, anchor);
                    if self.movement_mode == ZoomMovementMode::OnEdge {
                        // Match the established live-anchor behavior: OnEdge recomputes on every
                        // tick, even when the level sample itself has not advanced.
                        next.focal = focal;
                    } else {
                        next.focal =
                            focal + correction.offset.upscale(correction.weight(progress, done));
                    }
                }
            }
            Some(ZoomTransition::Pan(pan)) if !self.locked => {
                pointer_effect = pan.purpose.pointer_effect(self.movement_mode);
                done = pan.animation.is_done();
                next.focal = if done {
                    pan.animation.to
                } else {
                    pan.animation.value_at(self.clock.now())
                };
            }
            Some(ZoomTransition::Gesture(gesture)) => {
                pointer_effect = self.advance_gesture_focal(&mut next, &gesture.focal);
            }
            Some(ZoomTransition::GestureReturn(gesture)) => {
                done = gesture.animation.is_done();
                next.factor = self.sample_level_animation(&gesture.animation, done);
                pointer_effect = self.advance_gesture_focal(&mut next, &gesture.focal);
            }
            _ => (),
        }
        // Commit the final tracked/gesture focal and its pointer policy BEFORE owner sweep.
        self.commit_displayed(next, self.output_size);
        if done {
            self.transition = None;
        }
        pointer_effect
    }

    /// The live gesture and its coordinated return share one focal step: the touched
    /// anchor tracks the sampled level unless locking freezes the focal, and both
    /// preserve the seat tip through the resulting pointer policy.
    fn advance_gesture_focal(
        &self,
        next: &mut OutputViewport,
        focal: &GestureFocal,
    ) -> PointerEffect {
        if !self.locked {
            next.focal = self.gesture_focal_at(next.factor, focal);
        }
        PointerEffect::PreserveScreen
    }

    fn sample_level_animation(&self, animation: &Animation, done: bool) -> f64 {
        if done {
            animation.to()
        } else {
            animation.value_at(self.clock.now()).clamp(
                animation.from().min(animation.to()),
                animation.from().max(animation.to()),
            )
        }
        .max(1.)
    }

    fn set_config(
        &mut self,
        zoom: &Zoom,
        level_anim: &ZoomLevelChangeAnim,
        focal_anim: &ZoomFocalPanAnim,
    ) -> bool {
        let old = self.view();
        self.max_level = valid_max(zoom.max_zoom);
        // Reconcile the destination, not just the currently displayed animation sample.
        let target = self
            .transition
            .as_ref()
            .and_then(ZoomTransition::level_animation)
            .map_or(self.displayed.factor, Animation::to);
        if self.is_gesturing() && !self.locked {
            self.end_gesture(false, *level_anim);
        }
        if !self.locked && target > self.max_level {
            self.set_level(target, *level_anim);
        }
        self.set_movement_mode(zoom.movement_mode, *focal_anim);
        old != self.view() || self.is_transitioning()
    }

    fn set_output_size(&mut self, size: Size<f64, Logical>) -> bool {
        if self.output_size == size {
            return false;
        }
        if !self.commit_displayed(self.displayed, size) {
            return false;
        }
        let bounds = Rectangle::from_size(size);
        self.cursor = self.cursor.constrain(bounds);
        match &mut self.transition {
            Some(ZoomTransition::Pan(pan)) => {
                pan.animation.from = pan.animation.from.constrain(bounds);
                pan.animation.to = pan.animation.to.constrain(bounds);
            }
            Some(ZoomTransition::Gesture(gesture)) => gesture.focal.constrain(bounds),
            Some(ZoomTransition::GestureReturn(gesture)) => gesture.focal.constrain(bounds),
            _ => (),
        }
        true
    }

    fn set_level(&mut self, level: f64, config: ZoomLevelChangeAnim) {
        if self.locked || !level.is_finite() {
            return;
        }
        let target = level.clamp(1., self.max_level);
        // Even an equal-level request interrupts a gesture/pan without changing this frame.
        self.transition = None;
        if (self.displayed.factor - target).abs() <= ZOOM_CHANGE_EPSILON {
            return;
        }
        let animation = Animation::new(
            self.clock.clone(),
            self.displayed.factor,
            target,
            0.,
            config.0,
        );
        let tracking = self.tracking_for(&animation);
        self.transition = Some(ZoomTransition::Level(LevelAnimation {
            animation,
            tracking,
        }));
    }

    fn tracking_for(&self, animation: &Animation) -> TrackedCorrection {
        let offset = if self.movement_mode == ZoomMovementMode::OnEdge {
            Point::from((0., 0.))
        } else {
            self.displayed.focal - self.focal_for_at(self.displayed.factor, self.tracked_anchor())
        };
        TrackedCorrection {
            offset,
            start_progress: level_progress_at(animation, self.displayed.factor),
        }
    }

    fn reseed_tracked(&mut self) {
        if let Some(ZoomTransition::Level(level)) = &self.transition {
            let tracking = self.tracking_for(&level.animation);
            if let Some(ZoomTransition::Level(level)) = &mut self.transition {
                level.tracking = tracking;
            }
        }
    }

    /// Only real pointer movement calls this. Reappearance at identical coordinates is a no-op.
    fn set_cursor(&mut self, pos: Point<f64, Logical>) {
        if !finite_point(pos) || pos == self.cursor {
            return;
        }
        self.record_cursor(pos);
        if self.locked || self.displayed.factor <= 1. {
            return;
        }
        if self.owns_level() {
            if self.movement_mode == ZoomMovementMode::OnEdge
                && matches!(&self.transition, Some(ZoomTransition::Level(_)))
            {
                // OnEdge updates focal from the current viewport as the cursor moves, even while
                // a level animation owns the zoom factor. Preserve that established behavior.
                let next = OutputViewport {
                    focal: self.focal_for_at(self.displayed.factor, self.tracked_anchor()),
                    factor: self.displayed.factor,
                };
                self.commit_displayed(next, self.output_size);
                self.reseed_tracked();
            }
            return;
        }
        let next = OutputViewport {
            focal: self.focal_for_at(self.displayed.factor, self.tracked_anchor()),
            factor: self.displayed.factor,
        };
        self.transition = None;
        self.commit_displayed(next, self.output_size);
    }

    fn owns_level(&self) -> bool {
        matches!(
            self.transition,
            Some(
                ZoomTransition::Level(_)
                    | ZoomTransition::Gesture(_)
                    | ZoomTransition::GestureReturn(_)
            )
        )
    }

    fn unlock_target(&self) -> Point<f64, Logical> {
        if self.movement_mode == ZoomMovementMode::CursorFollow {
            // The screen-pinned endpoint must be S, not the old content C: at F=S, C=S.
            self.clamp_focal(self.view().content_to_screen(self.cursor))
        } else {
            self.focal_for_at(self.displayed.factor, self.tracked_anchor())
        }
    }

    fn start_pan(
        &mut self,
        target: Point<f64, Logical>,
        purpose: PanPurpose,
        config: ZoomFocalPanAnim,
    ) {
        self.transition = if different(target, self.displayed.focal) {
            Some(ZoomTransition::Pan(FocalPan {
                animation: PointAnimation::new(
                    self.clock.clone(),
                    self.displayed.focal,
                    self.clamp_focal(target),
                    config.0,
                ),
                purpose,
            }))
        } else {
            None
        };
    }

    fn set_movement_mode(&mut self, mode: ZoomMovementMode, config: ZoomFocalPanAnim) {
        if self.movement_mode == mode {
            return;
        }
        self.movement_mode = mode;
        if self.locked {
            return;
        }
        if self.owns_level() {
            self.reseed_tracked();
            return;
        }
        self.start_pan(
            self.focal_for_at(self.displayed.factor, self.tracked_anchor()),
            PanPurpose::ModeChange,
            config,
        );
    }

    fn toggle_lock(&mut self, level_config: ZoomLevelChangeAnim, focal_config: ZoomFocalPanAnim) {
        if !self.locked {
            if self.is_gesturing() {
                self.end_gesture(false, level_config);
            }
            self.locked = true;
            // Existing level animation continues, but focal is frozen, including gesture settling.
            if !self.owns_level() {
                self.transition = None;
            }
        } else {
            self.locked = false;
            let target = self
                .transition
                .as_ref()
                .and_then(ZoomTransition::level_animation)
                .map(Animation::to);
            // Lock froze focal, not the legal animation's destination or elapsed timeline.
            if target.is_some_and(|target| target <= self.max_level) {
                self.reseed_tracked(); // Does not replace GestureReturn's retained anchor.
                return;
            }
            // Reconcile the active destination, even when the displayed level is still legal.
            self.set_level(target.unwrap_or(self.displayed.factor), level_config);
            if self.transition.is_none() {
                self.start_pan(self.unlock_target(), PanPurpose::Unlock, focal_config);
            }
        }
    }

    fn begin_gesture(&mut self, screen_midpoint: Point<f64, Logical>) -> bool {
        if self.locked || !finite_point(screen_midpoint) || self.is_gesturing() {
            return false;
        }
        let content_anchor = self.view().screen_to_content(screen_midpoint);
        self.transition = Some(ZoomTransition::Gesture(ZoomGesture {
            start_level: self.displayed.factor,
            focal: GestureFocal {
                content_anchor,
                screen_midpoint,
            },
        }));
        true
    }

    /// Cumulative scale, absolute SCREEN-local midpoint. Touch never writes the mouse field.
    fn update_gesture(
        &mut self,
        scale: f64,
        sensitivity: f64,
        screen: Point<f64, Logical>,
    ) -> bool {
        if !finite_point(screen) {
            return false;
        }
        let Some(ZoomTransition::Gesture(gesture)) = &self.transition else {
            return false;
        };
        let Some(level) = gesture.sample_level(scale, sensitivity, self.max_level) else {
            return false;
        };
        let focal = GestureFocal {
            content_anchor: gesture.focal.content_anchor,
            screen_midpoint: screen,
        };
        let next = OutputViewport {
            focal: self.gesture_focal_at(level, &focal),
            factor: level,
        };
        if !self.commit_displayed(next, self.output_size) {
            return false;
        }
        if let Some(ZoomTransition::Gesture(gesture)) = &mut self.transition {
            gesture.focal = focal;
        }
        true
    }

    pub fn gesture_midpoint(&self) -> Option<Point<f64, Logical>> {
        match &self.transition {
            Some(ZoomTransition::Gesture(gesture)) => Some(gesture.focal.screen_midpoint),
            Some(ZoomTransition::GestureReturn(gesture)) => Some(gesture.focal.screen_midpoint),
            _ => None,
        }
    }

    fn end_gesture(&mut self, cancelled: bool, config: ZoomLevelChangeAnim) -> bool {
        let gesture = match self.transition.take() {
            Some(ZoomTransition::Gesture(gesture)) => gesture,
            other => {
                self.transition = other;
                return false;
            }
        };
        let target = if cancelled {
            gesture.start_level
        } else {
            self.displayed.factor
        }
        .clamp(1., self.max_level);
        if (target - self.displayed.factor).abs() > ZOOM_CHANGE_EPSILON {
            // Coordinated GestureReturn keeps the touched content anchor and pins the mouse.
            self.transition = Some(ZoomTransition::GestureReturn(GestureReturn {
                animation: Animation::new(
                    self.clock.clone(),
                    self.displayed.factor,
                    target,
                    0.,
                    config.0,
                ),
                focal: gesture.focal,
            }));
        }
        true
    }

    /// Stop a pointer-affecting driver when a manipulation grab/constraint becomes active.
    /// No rebase is sent through such a grab; the displayed view is retained.
    fn interrupt_pointer_driver(&mut self) {
        if matches!(
            self.transition,
            Some(
                ZoomTransition::Pan(_)
                    | ZoomTransition::Gesture(_)
                    | ZoomTransition::GestureReturn(_)
            )
        ) {
            self.transition = None;
        }
    }

    fn gesture_focal_at(&self, level: f64, focal: &GestureFocal) -> Point<f64, Logical> {
        if level <= 1. {
            return self.clamp_focal(self.displayed.focal);
        }
        // S = L*C + (1-L)*F => F = (L*C-S)/(L-1).
        self.clamp_focal(Point::from((
            (level * focal.content_anchor.x - focal.screen_midpoint.x) / (level - 1.),
            (level * focal.content_anchor.y - focal.screen_midpoint.y) / (level - 1.),
        )))
    }

    fn clamp_focal(&self, focal: Point<f64, Logical>) -> Point<f64, Logical> {
        focal.constrain(Rectangle::from_size(self.output_size))
    }

    fn tracked_anchor(&self) -> Option<Point<f64, Logical>> {
        self.tracked_anchor_for(self.cursor)
    }

    fn tracked_anchor_for(&self, cursor: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
        if self.movement_mode != ZoomMovementMode::OnEdge {
            return None;
        }
        let viewport = self.view().visible_viewport(self.output_size);
        let delta = cursor.constrain(viewport) - viewport.loc;
        Some(Point::from((
            delta.x / viewport.size.w.max(0.0001),
            delta.y / viewport.size.h.max(0.0001),
        )))
    }

    fn focal_for_at(&self, level: f64, anchor: Option<Point<f64, Logical>>) -> Point<f64, Logical> {
        self.focal_for_cursor_at(level, self.cursor, anchor)
    }

    fn focal_for_cursor_at(
        &self,
        level: f64,
        cursor: Point<f64, Logical>,
        anchor: Option<Point<f64, Logical>>,
    ) -> Point<f64, Logical> {
        if self.movement_mode == ZoomMovementMode::CursorFollow || level <= 1. {
            return self.clamp_focal(cursor);
        }
        let anchor = if self.movement_mode == ZoomMovementMode::OnEdge {
            anchor.unwrap_or(Point::from((0.5, 0.5)))
        } else {
            Point::from((0.5, 0.5))
        };
        let loc = cursor
            - Point::from((
                self.output_size.w / level * anchor.x,
                self.output_size.h / level * anchor.y,
            ));
        self.clamp_focal(loc.upscale(level / (level - 1.).max(0.0001)))
    }
}

fn level_progress_at(animation: &Animation, level: f64) -> f64 {
    let delta = animation.to() - animation.from();
    if delta.abs() <= ZOOM_CHANGE_EPSILON {
        return 1.;
    }
    ((level - animation.from()) / delta).clamp(0., 1.)
}

fn valid_max(max: f64) -> f64 {
    if max.is_finite() {
        max.max(1.)
    } else {
        10.
    }
}
fn finite_point(p: Point<f64, Logical>) -> bool {
    p.x.is_finite() && p.y.is_finite()
}

fn different(a: Point<f64, Logical>, b: Point<f64, Logical>) -> bool {
    (a.x - b.x).abs() > ZOOM_CHANGE_EPSILON || (a.y - b.y).abs() > ZOOM_CHANGE_EPSILON
}

/// One progress curve for both coordinates, bounded even for spring overshoot.
#[derive(Debug, Clone)]
struct PointAnimation {
    from: Point<f64, Logical>,
    to: Point<f64, Logical>,
    progress: Animation,
}

impl PointAnimation {
    fn new(
        clock: Clock,
        from: Point<f64, Logical>,
        to: Point<f64, Logical>,
        config: niri_config::Animation,
    ) -> Self {
        Self {
            from,
            to,
            progress: Animation::new(clock, 0., 1., 0., config),
        }
    }

    fn value_at(&self, now: Duration) -> Point<f64, Logical> {
        let t = self.progress.value_at(now).clamp(0., 1.);
        Point::from((
            self.from.x + (self.to.x - self.from.x) * t,
            self.from.y + (self.to.y - self.from.y) * t,
        ))
    }

    fn is_done(&self) -> bool {
        self.progress.is_done()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use approx::assert_relative_eq;
    use niri_config::animations::{Curve, EasingParams, Kind};
    use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
    use smithay::utils::Size;

    use super::*;

    impl OutputZoomState {
        fn verify_invariants(&self) {
            let valid_size = |size: Size<f64, Logical>| {
                size.w.is_finite() && size.h.is_finite() && size.w >= 0. && size.h >= 0.
            };
            let valid_view = |view: OutputViewport, size: Size<f64, Logical>| {
                valid_size(size)
                    && view.factor.is_finite()
                    && view.factor >= 1.
                    && finite_point(view.focal)
                    && view.focal.constrain(Rectangle::from_size(size)) == view.focal
            };
            let valid_animation =
                |animation: &Animation| animation.from().is_finite() && animation.to().is_finite();

            assert!(self.max_level.is_finite() && self.max_level >= 1.);
            assert!(valid_view(self.displayed, self.output_size));
            let bounds = Rectangle::from_size(self.output_size);
            assert!(finite_point(self.cursor) && self.cursor.constrain(bounds) == self.cursor);

            match &self.transition {
                Some(ZoomTransition::Level(level)) => {
                    assert!(valid_animation(&level.animation));
                    assert!(finite_point(level.tracking.offset));
                    assert!((0. ..=1.).contains(&level.tracking.start_progress));
                }
                Some(ZoomTransition::Pan(pan)) => {
                    assert!(finite_point(pan.animation.from) && finite_point(pan.animation.to));
                }
                Some(ZoomTransition::Gesture(gesture)) => {
                    assert!(gesture.start_level.is_finite() && gesture.start_level >= 1.);
                    assert!(finite_point(gesture.focal.content_anchor));
                    assert!(finite_point(gesture.focal.screen_midpoint));
                }
                Some(ZoomTransition::GestureReturn(gesture)) => {
                    assert!(valid_animation(&gesture.animation));
                    assert!(finite_point(gesture.focal.content_anchor));
                    assert!(finite_point(gesture.focal.screen_midpoint));
                }
                None => (),
            }

            if let Some(pending) = &self.pending_zoom_pointer {
                assert!(valid_view(pending.old_view, pending.old_size));
            }
        }
    }

    fn test_output() -> Output {
        let output = Output::new(
            "test".into(),
            PhysicalProperties {
                size: Size::from((1000, 500)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: Size::from((1000, 500)),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
        output
    }

    fn zoom_state(mode: ZoomMovementMode, clock: Clock) -> OutputZoomState {
        OutputZoomState::new(
            &test_output(),
            clock,
            Zoom {
                movement_mode: mode,
                ..Default::default()
            },
        )
    }

    fn easing(duration_ms: u32) -> niri_config::Animation {
        niri_config::Animation {
            off: false,
            kind: Kind::Easing(EasingParams {
                duration_ms,
                curve: Curve::Linear,
            }),
        }
    }

    fn anim_off() -> niri_config::Animation {
        niri_config::Animation {
            off: true,
            kind: Kind::Easing(EasingParams {
                duration_ms: 0,
                curve: Curve::Linear,
            }),
        }
    }

    #[test]
    fn level_transition_derives_focal_from_anchor_at_new_level() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut state = zoom_state(ZoomMovementMode::OnEdge, clock.clone());

        state.set_level(2.0, ZoomLevelChangeAnim(anim_off()));
        state.advance_animations();
        assert_relative_eq!(state.view().factor, 2.0);
        assert_relative_eq!(state.view().focal.x, 500., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 250., epsilon = 1e-6);

        state.set_cursor(Point::from((600., 250.)));

        state.set_level(3.0, ZoomLevelChangeAnim(easing(100)));
        assert!(state.is_transitioning());

        clock.set_unadjusted(Duration::from_millis(100));
        state.advance_animations();

        assert!(!state.are_animations_ongoing());
        assert_relative_eq!(state.view().factor, 3.0);
        assert_relative_eq!(state.view().focal.x, 550., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 250., epsilon = 1e-6);
        state.verify_invariants();
    }

    #[test]
    fn pan_meets_target_and_level_change_replaces_pan() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut state = zoom_state(ZoomMovementMode::Centered, clock.clone());

        state.set_level(2.0, ZoomLevelChangeAnim(anim_off()));
        state.advance_animations();
        state.set_cursor(Point::from((700., 350.)));
        assert_relative_eq!(state.view().focal.x, 900.);
        assert_relative_eq!(state.view().focal.y, 450.);

        // Moving to CursorFollow starts a real pan from (900, 450) to the cursor (700, 350).
        state.set_movement_mode(
            ZoomMovementMode::CursorFollow,
            ZoomFocalPanAnim(easing(100)),
        );

        state.advance_animations();
        assert_relative_eq!(state.view().focal.x, 900.);
        assert_relative_eq!(state.view().focal.y, 450.);

        clock.set_unadjusted(Duration::from_millis(50));
        state.advance_animations();
        assert_relative_eq!(state.view().focal.x, 800., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 400., epsilon = 1e-6);

        // Starting a level change cancels the pan and replaces it with a tracked level
        // animation; a pan would still be running at t=100.
        state.set_level(3.0, ZoomLevelChangeAnim(easing(100)));

        clock.set_unadjusted(Duration::from_millis(100));
        state.advance_animations();
        assert_relative_eq!(state.view().factor, 2.5, epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.x, 750., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 375., epsilon = 1e-6);

        clock.set_unadjusted(Duration::from_millis(150));
        state.advance_animations();

        assert!(!state.are_animations_ongoing());
        assert_relative_eq!(state.view().factor, 3.0);
        assert_relative_eq!(state.view().focal.x, 700., epsilon = f64::EPSILON);
        assert_relative_eq!(state.view().focal.y, 350., epsilon = f64::EPSILON);
        state.verify_invariants();
    }

    #[test]
    fn mode_change_during_level_animation_reseeds_without_view_jump() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut state = zoom_state(ZoomMovementMode::OnEdge, clock.clone());

        state.set_level(2.0, ZoomLevelChangeAnim(anim_off()));
        state.advance_animations();
        state.set_cursor(Point::from((600., 250.)));

        state.set_level(3.0, ZoomLevelChangeAnim(easing(100)));
        let view_before = state.view();

        // The level animation owns the mode change; the tracked correction is reseeded
        // so the displayed view does not jump.
        state.set_movement_mode(ZoomMovementMode::Centered, ZoomFocalPanAnim(easing(100)));
        assert!(state.is_transitioning());
        assert_eq!(state.view(), view_before);

        clock.set_unadjusted(Duration::from_millis(50));
        state.advance_animations();
        assert_relative_eq!(state.view().factor, 2.5, epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.x, 1700. / 3., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 250., epsilon = 1e-6);

        clock.set_unadjusted(Duration::from_millis(100));
        state.advance_animations();

        assert!(!state.are_animations_ongoing());
        assert_relative_eq!(state.view().factor, 3.0);
        assert_relative_eq!(state.view().focal.x, 650., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 250., epsilon = 1e-6);
        state.verify_invariants();
    }

    #[test]
    fn locked_zoom_freezes_and_unlock_reclamps_after_max_level_decrease() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut state = OutputZoomState::new(
            &test_output(),
            clock.clone(),
            Zoom {
                movement_mode: ZoomMovementMode::Centered,
                max_zoom: 10.0,
                ..Default::default()
            },
        );

        state.set_level(8.0, ZoomLevelChangeAnim(anim_off()));
        state.advance_animations();
        assert_relative_eq!(state.view().factor, 8.0);
        let frozen = state.view();

        state.toggle_lock(
            ZoomLevelChangeAnim(anim_off()),
            ZoomFocalPanAnim(anim_off()),
        );
        assert!(state.locked());
        assert!(!state.are_animations_ongoing());

        // While locked: level requests are ignored, the cursor never moves the view.
        state.set_level(3.0, ZoomLevelChangeAnim(anim_off()));
        state.set_cursor(Point::from((200., 100.)));
        state.advance_animations();
        assert_eq!(state.view(), frozen);

        // Lowering the max zoom cannot move a locked level above the new max.
        let zoom = Zoom {
            movement_mode: ZoomMovementMode::Centered,
            max_zoom: 5.0,
            ..Default::default()
        };
        let changed = state.set_config(
            &zoom,
            &ZoomLevelChangeAnim(anim_off()),
            &ZoomFocalPanAnim(anim_off()),
        );
        assert!(!changed);
        assert_eq!(state.view(), frozen);

        // Unlocking re-clamps the level to the new max before the zoom resumes.
        state.toggle_lock(
            ZoomLevelChangeAnim(anim_off()),
            ZoomFocalPanAnim(anim_off()),
        );
        assert!(state.are_animations_ongoing());

        state.advance_animations();
        assert!(!state.are_animations_ongoing());
        assert!(!state.locked());
        assert_relative_eq!(state.view().factor, 5.0);
        assert_relative_eq!(state.view().focal.x, 125., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 62.5, epsilon = 1e-6);
        state.verify_invariants();
    }

    #[test]
    fn gesture_update_commits_pointer_change_and_cancel_returns_to_start_level() {
        let clock = Clock::with_time(Duration::ZERO);
        let mut state = zoom_state(ZoomMovementMode::Centered, clock.clone());

        assert!(state.update(ZoomAction::BeginGesture(Point::from((500., 250.)))));
        assert!(state.is_gesturing());

        // A gesture update commits the displayed level and records the seat's
        // reprojection obligation against the old view.
        assert!(state.update(ZoomAction::UpdateGesture(
            2.0,
            1.0,
            Point::from((500., 250.))
        )));
        assert_relative_eq!(state.view().factor, 2.0);
        assert_relative_eq!(state.view().focal.x, 500., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 250., epsilon = 1e-6);
        {
            let pending = state.pending_zoom_pointer.as_ref().unwrap();
            assert_relative_eq!(pending.old_view.factor, 1.0);
            assert_eq!(pending.old_view.focal, Point::from((500., 250.)));
            assert_eq!(pending.old_size, Size::from((1000., 500.)));
        }

        // A moved screen midpoint is preserved while the touched content anchor tracks;
        // the earliest pointer change stays pending, later ones coalesce into it.
        assert!(state.update(ZoomAction::UpdateGesture(
            3.0,
            1.0,
            Point::from((600., 250.))
        )));
        assert_relative_eq!(state.view().factor, 3.0);
        assert_relative_eq!(state.view().focal.x, 450., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 250., epsilon = 1e-6);
        assert_eq!(state.gesture_midpoint(), Some(Point::from((600., 250.))));
        {
            let pending = state.pending_zoom_pointer.as_ref().unwrap();
            assert_relative_eq!(pending.old_view.factor, 1.0);
        }

        // Cancellation returns the gesture to its start level through a return animation.
        assert!(state.update(ZoomAction::EndGesture(
            true,
            ZoomLevelChangeAnim(anim_off())
        )));
        assert!(!state.is_gesturing());
        assert!(state.are_animations_ongoing());
        assert_eq!(state.gesture_midpoint(), Some(Point::from((600., 250.))));

        assert!(state.update(ZoomAction::Advance));
        assert!(!state.are_animations_ongoing());
        assert_eq!(state.gesture_midpoint(), None);
        assert_relative_eq!(state.view().factor, 1.0);
        assert_relative_eq!(state.view().focal.x, 450., epsilon = 1e-6);
        assert_relative_eq!(state.view().focal.y, 250., epsilon = 1e-6);

        // The initial obligation remains pending for the seat to consume.
        {
            let pending = state.pending_zoom_pointer.as_ref().unwrap();
            assert_relative_eq!(pending.old_view.factor, 1.0);
        }
        state.verify_invariants();
    }
}
