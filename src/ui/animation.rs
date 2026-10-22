//! The shared frame clock, and the tween helper views animate with.
//!
//! One `Entity<Animator>` exists per window. Views that need per-frame motion
//! (a highlight fading in, lyrics scrolling) plug into it with [`animate`]: it
//! installs the observer, steps the view, re-renders only while something is
//! actually moving, and asks for another frame while the animation is live.
//! When nothing asks for a frame the clock parks on a channel instead of waking
//! on a timer, so an idle UI draws (and burns) exactly nothing.
//!
//! Anything that *starts* an animation must ask for a frame too — otherwise
//! the clock stays parked and the first tick never arrives. Views keep a
//! [`WakeHandle`] (it needs no `Context`) and call it from the listener or
//! render path where the motion begins.
//!
//! There are exactly three cadences, and nothing in between:
//!
//! - [`SLOW_FPS`] (1) — coarse readouts: the transport's time labels, the
//!   waveform progress, the live bitrate. Deliberately *not* driven by this
//!   clock; they update on the playback tick instead (see `PlaybackState`),
//!   which keeps a playing track from re-rendering the UI every frame.
//! - [`SMOOTH_FPS`] (144) — continuous animation. A view uses [`animate`], which
//!   installs the observer, steps it, re-renders only while it moved, and keeps
//!   asking for frames only while it's still animating.
//! - Per-request — a view that needs a single frame (a texture landed, a size
//!   changed) calls [`WakeHandle::request`] and stops. Nothing keeps ticking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::{Context, Entity, Subscription};

/// Frames per second for coarse, non-animated readouts: the transport's time
/// labels, the waveform progress, the live bitrate. Low on purpose — these
/// change slowly and re-rendering the UI for them is wasteful.
pub const SLOW_FPS: f32 = 1.0;

/// Frames per second for animation: selection fades, bar motion, lyric
/// scrolling. Raise toward 144 on a high-refresh display.
pub const SMOOTH_FPS: f32 = 144.0;

/// What one animation step needs from the clock.
///
/// `redraw` separates "the view changed" from `awake`: a playing visualizer
/// has to keep sampling even through a silent passage where no bar moved, but
/// it doesn't need to re-render until one does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Advance {
    /// The view's visible state changed, so it has to re-render.
    pub redraw: bool,
    /// The animation isn't finished: ask for another frame even without a
    /// redraw (otherwise a silent sampler would stall itself).
    pub awake: bool,
}

impl Advance {
    /// Nothing moved, nothing pending.
    pub const IDLE: Self = Self { redraw: false, awake: false };
    /// Pixels changed, so draw and keep going.
    pub const MOVED: Self = Self { redraw: true, awake: true };
    /// No pixels changed, but the animation still needs frames (sampling).
    pub const BUSY: Self = Self { redraw: false, awake: true };
}

/// Drive a view's continuous animation from the shared clock (144 Hz).
///
/// The observer boilerplate every animated view used to repeat — read `dt`,
/// tick, ask for another frame, re-render — lives here once. `step` advances
/// the view and reports what it needs; the helper takes care of the rest, and
/// when every stepper settles it simply stops asking, so the clock parks
/// itself. A view that only ever needs one frame should not subscribe this
/// way: use [`WakeHandle::request`] instead.
pub fn animate<V: 'static>(
    animator: &Entity<Animator>,
    cx: &mut Context<V>,
    mut step: impl FnMut(&mut V, f32, &mut Context<V>) -> Advance + 'static,
) -> Subscription {
    cx.observe(animator, move |this, animator, cx| {
        let dt = animator.read(cx).dt();
        let advance = step(this, dt, cx);
        if advance.awake {
            animator.read(cx).request();
        }
        if advance.redraw {
            cx.notify();
        }
    })
}

/// A cheap, clonable way to ask the frame clock for another frame.
///
/// Deliberately needs no `Context`: animation can begin from an event listener,
/// a render closure, or a plain helper struct, all of which are the places a
/// [`Tween`]/[`Spring`] gets retargeted. Calling it from inside a frame's
/// observer is what keeps the clock awake until the motion settles.
#[derive(Clone)]
pub struct WakeHandle {
    shared: Arc<WakeShared>,
    // Held here, not in `WakeShared`: when every handle is dropped the channel
    // closes and the parked loop exits, rather than the loop keeping its own
    // sender alive forever.
    wake: mpsc::UnboundedSender<()>,
}

struct WakeShared {
    /// A frame has been requested since the clock last consumed the flag.
    demand: AtomicBool,
    /// Whether the clock is currently running (so `request` knows whether it
    /// needs to kick the parked loop with a channel message).
    awake: AtomicBool,
}

impl WakeHandle {
    /// Ask for at least one more frame. While an animation is running this is
    /// called once per observer tick; the send only happens on the idle→awake
    /// transition, so it never floods the channel.
    pub fn request(&self) {
        self.shared.demand.store(true, Ordering::SeqCst);
        if !self.shared.awake.swap(true, Ordering::SeqCst) {
            // The clock is parked: nudge it. It may already have gone away if
            // the window closed; that's fine.
            let _ = self.wake.unbounded_send(());
        }
    }
}

/// The shared frame clock.
///
/// Sits parked on a channel until something calls [`WakeHandle::request`], then
/// ticks at [`SMOOTH_FPS`] and notifies its observers each frame while any of
/// them asks for another. Views advance their own animations by
/// [`dt`](Self::dt); prefer [`animate`] over observing the clock by hand.
pub struct Animator {
    /// Seconds since the previous frame, clamped so a stalled frame can't jump
    /// an animation to its end.
    dt: f32,
    /// Seconds since the clock started.
    elapsed: f32,
    wake: WakeHandle,
}

impl Animator {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let wake = Self::start(cx);
        Self { dt: 0.0, elapsed: 0.0, wake }
    }

    /// Seconds since the previous frame.
    pub fn dt(&self) -> f32 {
        self.dt
    }

    /// Seconds since the clock started.
    pub fn elapsed(&self) -> f32 {
        self.elapsed
    }

    /// A handle for asking this clock for frames without borrowing the entity.
    pub fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    /// Ask for at least one more frame. Shorthand for `wake_handle().request()`.
    pub fn request(&self) {
        self.wake.request();
    }

    fn start(cx: &mut Context<Self>) -> WakeHandle {
        let (wake, mut rx) = mpsc::unbounded::<()>();
        let shared = Arc::new(WakeShared {
            demand: AtomicBool::new(false),
            awake: AtomicBool::new(false),
        });
        let handle = WakeHandle { shared: shared.clone(), wake };
        cx.spawn(async move |this, cx| {
            let interval = Duration::from_secs_f32(1.0 / SMOOTH_FPS.max(1.0));
            'idle: loop {
                // Park with no timer: an idle window costs nothing at all.
                if rx.next().await.is_none() {
                    break;
                }
                // Coalesce any other wake-ups queued before we got here.
                while rx.try_recv().is_ok() {}

                let mut last = Instant::now();
                loop {
                    // A request since the last frame keeps the clock running.
                    if !shared.demand.swap(false, Ordering::SeqCst) {
                        shared.awake.store(false, Ordering::SeqCst);
                        // A request may have landed between the swap and the
                        // store — it saw `awake` still true and didn't nudge
                        // us, so pick it up here rather than losing it.
                        if shared.demand.load(Ordering::SeqCst) {
                            shared.awake.store(true, Ordering::SeqCst);
                            continue;
                        }
                        continue 'idle;
                    }

                    cx.background_executor().timer(interval).await;
                    let now = Instant::now();
                    // Clamp so a long stall (a slow frame, a suspend) doesn't
                    // make an animation leap straight to its end.
                    let dt = now.saturating_duration_since(last).as_secs_f32().min(0.1);
                    last = now;
                    if this
                        .update(cx, |this, cx| {
                            this.dt = dt;
                            this.elapsed += dt;
                            cx.notify();
                        })
                        .is_err()
                    {
                        break 'idle;
                    }
                    // Observers have now run and requested another frame if
                    // they're still moving; the flag is read at the loop top.
                }
            }
        })
        .detach();
        handle
    }
}

/// A one-shot eased interpolation of a single `f32`.
///
/// Views hold one per animated value, retarget it when the thing it follows
/// changes, and advance it from their `Animator` observer.
#[derive(Clone, Copy, Debug)]
pub struct Tween {
    value: f32,
    from: f32,
    to: f32,
    elapsed: f32,
    duration: f32,
    running: bool,
}

impl Tween {
    /// A settled tween at `value`.
    pub fn new(value: f32) -> Self {
        Self { value, from: value, to: value, elapsed: 0.0, duration: 0.0, running: false }
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn is_animating(&self) -> bool {
        self.running
    }

    /// Jump to `value`, cancelling any animation in flight.
    pub fn set(&mut self, value: f32) {
        self.value = value;
        self.from = value;
        self.to = value;
        self.running = false;
    }

    /// Animate from the current value to `to` over `duration` seconds.
    pub fn to(&mut self, to: f32, duration: f32) {
        self.from = self.value;
        self.to = to;
        self.elapsed = 0.0;
        self.duration = duration.max(1e-4);
        self.running = true;
    }

    /// Advance by `dt` seconds. Returns whether it advanced — true for the
    /// frame that lands on the target too, so the view renders the settled
    /// value once.
    pub fn tick(&mut self, dt: f32) -> bool {
        if !self.running {
            return false;
        }
        self.elapsed += dt;
        let t = (self.elapsed / self.duration).clamp(0.0, 1.0);
        self.value = self.from + (self.to - self.from) * ease_out_cubic(t);
        if t >= 1.0 {
            self.value = self.to;
            self.running = false;
        }
        true
    }
}

/// Fast at the start, easing to a stop — the standard "settle" curve.
pub(crate) fn ease_out_cubic(t: f32) -> f32 {
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

/// Stiffness of the highlight springs, in force per unit displacement. Higher
/// is snappier.
pub const SPRING_STIFFNESS: f32 = 260.0;

/// Damping of the highlight springs. `2 * sqrt(stiffness)` is critically damped;
/// a little under that gives the faintest overshoot — alive, not bouncy.
pub const SPRING_DAMPING: f32 = 30.0;

/// Below this displacement and velocity a spring counts as settled.
const SPRING_SETTLE_VALUE: f32 = 0.001;
const SPRING_SETTLE_VELOCITY: f32 = 0.02;

/// Longest integration step before the spring substeps, so a long frame can't
/// make the explicit integration blow up.
const SPRING_MAX_STEP: f32 = 0.008;

/// A damped spring, for values that retarget constantly.
///
/// Unlike [`Tween`], which restarts its curve from zero on every retarget, a
/// spring **carries its velocity**: flicking the pointer down a list doesn't
/// queue up a fresh animation per row, it just chases the new target with the
/// speed it already had. That's what makes fast hover read as continuous rather
/// than smeared or steppy.
///
/// gpui 0.2 has no spring element (the git version's `SpringAnimation` isn't in
/// the published crate), and its `Animation` is duration-based with the same
/// restart problem — so this is hand-rolled and driven by the shared
/// [`Animator`](super::animation::Animator) instead, which also means it stops
/// asking for frames the moment it settles.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    value: f32,
    velocity: f32,
    target: f32,
    stiffness: f32,
    damping: f32,
}

impl Spring {
    /// A spring at rest on `value`, tuned for the highlight crossfades.
    pub fn new(value: f32) -> Self {
        Self::with_params(value, SPRING_STIFFNESS, SPRING_DAMPING)
    }

    /// A spring at rest on `value` with explicit tuning (a stiffer, under-damped
    /// pair overshoots, which is how the marquee's snap back reads).
    pub fn with_params(value: f32, stiffness: f32, damping: f32) -> Self {
        Self { value, velocity: 0.0, target: value, stiffness, damping }
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    /// Aim at `target`, keeping whatever velocity it already has.
    pub fn target(&mut self, target: f32) {
        self.target = target;
    }

    /// Jump to `value`, cancelling the motion.
    pub fn set(&mut self, value: f32) {
        self.value = value;
        self.velocity = 0.0;
        self.target = value;
    }

    pub fn is_animating(&self) -> bool {
        (self.target - self.value).abs() > SPRING_SETTLE_VALUE
            || self.velocity.abs() > SPRING_SETTLE_VELOCITY
    }

    /// Integrate one step. Returns whether it was still moving, so the frame
    /// that lands it still gets drawn.
    pub fn tick(&mut self, dt: f32) -> bool {
        if !self.is_animating() {
            self.value = self.target;
            self.velocity = 0.0;
            return false;
        }
        let steps = ((dt / SPRING_MAX_STEP).ceil() as usize).clamp(1, 16);
        let h = dt / steps as f32;
        for _ in 0..steps {
            let spring_force = (self.target - self.value) * self.stiffness;
            let damping_force = self.velocity * self.damping;
            self.velocity += (spring_force - damping_force) * h;
            self.value += self.velocity * h;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_wakes_a_parked_clock_exactly_once() {
        let (tx, mut rx) = mpsc::unbounded::<()>();
        let shared = Arc::new(WakeShared {
            demand: AtomicBool::new(false),
            awake: AtomicBool::new(false),
        });
        let handle = WakeHandle { shared: shared.clone(), wake: tx };

        // A request from parked state arms the demand flag and nudges the
        // channel once.
        handle.request();
        assert!(shared.demand.load(Ordering::SeqCst));
        assert!(shared.awake.load(Ordering::SeqCst));
        assert!(rx.try_recv().is_ok());

        // Further requests while the clock is running must not enqueue more
        // messages — otherwise a long animation would flood the channel.
        handle.request();
        handle.request();
        assert!(rx.try_recv().is_err());

        // Once the loop parks again, the next request nudges it afresh.
        shared.awake.store(false, Ordering::SeqCst);
        handle.request();
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn a_tween_settles_on_its_target() {
        let mut tween = Tween::new(0.0);
        tween.to(1.0, 0.1);
        assert!(tween.is_animating());
        // Halfway through, it's partway there (eased, so past the midpoint).
        assert!(tween.tick(0.05));
        assert!(tween.value() > 0.5 && tween.value() < 1.0, "{}", tween.value());
        // Ticking past the duration lands exactly on the target and stops.
        assert!(tween.tick(0.1), "the settling frame still needs a render");
        assert_eq!(tween.value(), 1.0);
        assert!(!tween.is_animating());
        assert!(!tween.tick(0.1), "a settled tween asks for no more frames");
    }

    #[test]
    fn set_cancels_an_animation() {
        let mut tween = Tween::new(0.0);
        tween.to(1.0, 1.0);
        tween.set(0.25);
        assert_eq!(tween.value(), 0.25);
        assert!(!tween.is_animating());
        assert!(!tween.tick(0.1));
    }

    #[test]
    fn retargeting_starts_from_the_current_value() {
        let mut tween = Tween::new(0.0);
        tween.to(1.0, 0.1);
        tween.tick(0.05);
        let mid = tween.value();
        tween.to(0.0, 0.1);
        assert!(tween.tick(0.001));
        assert!(tween.value() < mid, "it turns around rather than jumping");
    }

    #[test]
    fn a_spring_settles_on_its_target_and_then_rests() {
        let mut spring = Spring::new(0.0);
        spring.target(1.0);
        let mut frames = 0;
        while spring.is_animating() && frames < 600 {
            spring.tick(1.0 / 60.0);
            frames += 1;
        }
        assert!(!spring.is_animating(), "it settles");
        // It stops within its own settle tolerance, not necessarily dead on.
        assert!(
            (spring.value() - 1.0).abs() <= SPRING_SETTLE_VALUE + 0.001,
            "value {}",
            spring.value()
        );
        assert!(frames < 60, "settled in {frames} frames");
        assert!(!spring.tick(1.0 / 60.0), "a settled spring asks for no more frames");
    }

    #[test]
    fn a_spring_retargeted_to_where_it_is_keeps_coasting() {
        // The property a duration tween can't have: it carries its velocity
        // through a retarget. Aiming at where it already is leaves it moving,
        // and it coasts past rather than stopping dead.
        let mut spring = Spring::new(0.0);
        spring.target(1.0);
        spring.tick(1.0 / 60.0);
        let moving = spring.value();
        assert!(moving > 0.0 && moving < 1.0, "{moving}");

        spring.target(moving);
        assert!(spring.is_animating(), "it still has momentum");
        spring.tick(1.0 / 60.0);
        assert!(spring.value() > moving, "it coasts past instead of stopping");
    }

    #[test]
    fn retargeting_a_spring_is_continuous() {
        // No restart and no snap: the value moves out of wherever it had got to
        // rather than jumping back to where it started or to the new target.
        let mut spring = Spring::new(0.0);
        spring.target(1.0);
        for _ in 0..6 {
            spring.tick(1.0 / 60.0);
        }
        let before = spring.value();
        assert!(before > 0.5, "it made progress: {before}");

        spring.target(0.0);
        spring.tick(1.0 / 60.0);
        let after = spring.value();
        // Scale-free, so it holds whatever the spring is tuned to (a snappy
        // spring may even overshoot past the target first).
        assert!(after > before / 2.0, "it moves gradually: {before} -> {after}");
        assert!(after > 0.0, "and it hasn't snapped to the new target");
    }

    #[test]
    fn a_spring_stays_bounded_on_a_long_frame() {
        let mut spring = Spring::new(0.0);
        spring.target(1.0);
        for _ in 0..600 {
            spring.tick(0.25);
        }
        // Substepping keeps the explicit integration stable; a little overshoot
        // is expected from an under-damped spring, a blow-up is not.
        assert!(
            (-0.1..=1.15).contains(&spring.value()),
            "value {}",
            spring.value()
        );
    }
}
