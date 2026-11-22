//! Camera controller: grab-drag with inertia, smooth zoom anchored under the cursor,
//! animated look-at and projection morphs, and autorotation.

use glam::{Vec2, Vec3};
use nodal_core::math::{ease_in_out_cubic, wrap_180, yaw_pitch_from_dir, DEG};
use nodal_core::tour::{Autorotate, Controls, DragMode};
use nodal_core::{Projection, Projector, View, ViewLimits};

struct Anim {
    from: View,
    to: View,
    t: f32,
    duration: f32,
}

pub struct Camera {
    pub view: View,
    pub limits: ViewLimits,
    /// Extra minimum fov derived from the panorama resolution (prevents blurry over-zoom).
    pub min_fov_res: f32,
    pub autorotate: Autorotate,
    /// User toggle on top of the tour setting.
    pub autorotate_on: bool,
    pub controls: Controls,
    vel: Vec2,
    vel_est: Vec2,
    last_drag_time: f64,
    fov_target: Option<f32>,
    anchor: Option<(Vec2, Vec3)>,
    anim: Option<Anim>,
    pub idle: f32,
    ramp: f32,
    pub dragging: bool,
    joystick_origin: Option<Vec2>,
    joystick_pos: Vec2,
    pub keys: Vec2,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            view: View::default(),
            limits: ViewLimits::default(),
            min_fov_res: 0.0,
            autorotate: Autorotate::default(),
            autorotate_on: true,
            controls: Controls::default(),
            vel: Vec2::ZERO,
            vel_est: Vec2::ZERO,
            last_drag_time: 0.0,
            fov_target: None,
            anchor: None,
            anim: None,
            idle: 0.0,
            ramp: 0.0,
            dragging: false,
            joystick_origin: None,
            joystick_pos: Vec2::ZERO,
            keys: Vec2::ZERO,
        }
    }
}

impl Camera {
    /// Any user interaction: resets idle time and stops autorotation and animations.
    pub fn touch(&mut self) {
        self.idle = 0.0;
        self.ramp = 0.0;
    }

    pub fn is_rotating(&self) -> bool {
        self.ramp > 0.0
    }

    pub fn jump_to(&mut self, view: View) {
        self.view = view;
        self.vel = Vec2::ZERO;
        self.fov_target = None;
        self.anchor = None;
        self.anim = None;
        self.clamp();
    }

    pub fn begin_drag(&mut self, at: Vec2) {
        self.touch();
        self.dragging = true;
        self.anim = None;
        self.vel = Vec2::ZERO;
        self.vel_est = Vec2::ZERO;
        if self.controls.mode == DragMode::Joystick {
            self.joystick_origin = Some(at);
            self.joystick_pos = at;
        }
    }

    /// Pointer moved from `from` to `to` (physical pixels) while dragging.
    pub fn drag(&mut self, proj: &Projector, from: Vec2, to: Vec2, dt: f32, now: f64) {
        self.touch();
        if self.controls.mode == DragMode::Joystick {
            self.joystick_pos = to;
            return;
        }
        let (Some(a), Some(b)) = (proj.screen_to_dir(from), proj.screen_to_dir(to)) else { return };
        let (ya, pa) = yaw_pitch_from_dir(a);
        let (yb, pb) = yaw_pitch_from_dir(b);
        let mut d = Vec2::new(wrap_180((ya - yb) / DEG), (pa - pb) / DEG);
        if self.controls.invert {
            d = -d;
        }
        // Near the poles yaw can swing wildly for tiny pointer moves; keep it sane.
        d.x = d.x.clamp(-25.0, 25.0);
        d.y = d.y.clamp(-25.0, 25.0);
        self.view.yaw += d.x;
        self.view.pitch += d.y;
        if dt > 0.0 {
            let inst = d / dt.max(1.0 / 240.0);
            self.vel_est = self.vel_est.lerp(inst, 0.4);
        }
        self.last_drag_time = now;
        self.clamp();
    }

    pub fn end_drag(&mut self, now: f64) {
        self.dragging = false;
        self.joystick_origin = None;
        // Only fling if the pointer was still moving when released.
        self.vel = if now - self.last_drag_time < 0.06 { self.vel_est.clamp_length_max(900.0) } else { Vec2::ZERO };
    }

    /// Multiplies the target fov by `factor`; `anchor` keeps a direction under a screen point.
    pub fn zoom(&mut self, factor: f32, anchor: Option<(Vec2, Vec3)>) {
        self.touch();
        self.anim = None;
        let current = self.fov_target.unwrap_or(self.view.fov);
        let mut v = self.view;
        v.fov = current * factor;
        v.clamp(&self.limits, self.min_fov_res);
        self.fov_target = Some(v.fov);
        if self.controls.zoom_to_cursor {
            self.anchor = anchor;
        }
    }

    pub fn animate_to(&mut self, mut to: View, duration: Option<f32>) {
        let from = self.view;
        to.clamp(&self.limits, self.min_fov_res.min(to.fov));
        // Take the short way round.
        to.yaw = from.yaw + wrap_180(to.yaw - from.yaw);
        let ang = (to.yaw - from.yaw).abs().max((to.pitch - from.pitch).abs());
        let duration = duration.unwrap_or((0.45 + ang / 140.0).clamp(0.45, 1.6));
        self.anim = Some(Anim { from, to, t: 0.0, duration });
        self.vel = Vec2::ZERO;
        self.fov_target = None;
        self.anchor = None;
    }

    pub fn set_projection(&mut self, p: Projection, view_hint: Option<(f32, f32)>) {
        let mut to = self.view;
        to.projection = p;
        if let Some((pitch, fov)) = view_hint {
            to.pitch = pitch;
            to.fov = fov;
        } else {
            let max = p.max_fov();
            if to.fov > max {
                to.fov = max * 0.8;
            }
        }
        self.animate_to(to, Some(0.9));
    }

    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
            || self.fov_target.is_some()
            || self.vel.length_squared() > 0.01
            || self.keys != Vec2::ZERO
            || self.joystick_origin.is_some()
    }

    fn clamp(&mut self) {
        let mut v = self.view;
        v.clamp(&self.limits, self.min_fov_res);
        self.view = v;
    }

    /// Advances the camera by `dt` seconds for a viewport of `w×h` physical pixels. `real_dt` is the
    /// wall-clock time since the previous frame (frames are not drawn while nothing changes), and
    /// `active` is false when the window is in the background, where idle rotation is paused.
    pub fn update(&mut self, dt: f32, real_dt: f32, active: bool, w: f32, h: f32) -> Motion {
        let dt = dt.clamp(0.0, 0.1);
        self.idle += real_dt.max(0.0);

        if let Some(anim) = &mut self.anim {
            anim.t += dt / anim.duration;
            let k = ease_in_out_cubic(anim.t);
            let (a, b) = (anim.from, anim.to);
            self.view = View {
                yaw: a.yaw + (b.yaw - a.yaw) * k,
                pitch: a.pitch + (b.pitch - a.pitch) * k,
                roll: a.roll + (b.roll - a.roll) * k,
                fov: (a.fov.ln() + (b.fov.ln() - a.fov.ln()) * k).exp(),
                projection: a.projection.lerp(&b.projection, k),
            };
            if anim.t >= 1.0 {
                self.view = b;
                self.anim = None;
            }
            self.view.yaw = wrap_180(self.view.yaw);
            return Motion::Moving;
        }

        // Joystick drag: offset from the press point sets the angular velocity.
        if let Some(origin) = self.joystick_origin {
            let off = (self.joystick_pos - origin) / w.max(h);
            let speed = off * 220.0;
            self.view.yaw += speed.x * dt * if self.controls.invert { -1.0 } else { 1.0 };
            self.view.pitch -= speed.y * dt * if self.controls.invert { -1.0 } else { 1.0 };
        }

        // Keyboard panning (deg/s scaled with fov so it feels the same at every zoom).
        if self.keys != Vec2::ZERO {
            self.touch();
            let speed = self.view.fov.clamp(10.0, 120.0) * 0.9;
            self.view.yaw += self.keys.x * speed * dt;
            self.view.pitch += self.keys.y * speed * dt;
        }

        // Inertia.
        if !self.dragging && self.vel.length_squared() > 0.0 {
            self.view.yaw += self.vel.x * dt;
            self.view.pitch += self.vel.y * dt;
            let k = 1.5 + 10.0 * self.controls.friction.clamp(0.0, 1.0);
            self.vel *= (-k * dt).exp();
            if self.vel.length() < 0.3 {
                self.vel = Vec2::ZERO;
            }
        }

        // Smooth zoom with the anchor direction pinned under the cursor.
        if let Some(target) = self.fov_target {
            let k = 1.0 - (-14.0 * dt).exp();
            self.view.fov += (target - self.view.fov) * k;
            if (target - self.view.fov).abs() < 0.01 {
                self.view.fov = target;
                self.fov_target = None;
            }
            if let Some((px, dir)) = self.anchor {
                for _ in 0..3 {
                    let p = Projector::new(&self.view, w, h);
                    let Some(cur) = p.screen_to_dir(px) else { break };
                    let (y1, p1) = yaw_pitch_from_dir(dir);
                    let (y2, p2) = yaw_pitch_from_dir(cur);
                    self.view.yaw += wrap_180((y1 - y2) / DEG);
                    self.view.pitch += (p1 - p2) / DEG;
                    self.clamp();
                }
            }
            if self.fov_target.is_none() {
                self.anchor = None;
            }
        }

        // Autorotation after a period of inactivity, ramping in gently.
        let rotating = active && self.autorotate_due() && !self.dragging && self.vel == Vec2::ZERO;
        if rotating {
            self.ramp = (self.ramp + dt / 1.8).min(1.0);
            let r = ease_in_out_cubic(self.ramp);
            self.view.yaw += self.autorotate.speed * r * dt;
            if self.autorotate.level_horizon && self.view.projection.mode != nodal_core::ProjMode::Equirect && self.view.pitch.abs() < 60.0
            {
                self.view.pitch *= 1.0 - (1.0 - (-0.35 * dt).exp()) * r;
            }
        }
        self.clamp();
        if self.is_animating() {
            Motion::Moving
        } else if rotating {
            Motion::AutoRotating
        } else {
            Motion::Still
        }
    }

    /// Whether idle rotation should be running right now (ignoring window state).
    fn autorotate_due(&self) -> bool {
        let a = &self.autorotate;
        a.enabled && self.autorotate_on && self.idle > a.delay && (a.max_seconds <= 0.0 || self.idle < a.delay + a.max_seconds)
    }

    /// Seconds until idle rotation would start, if it is waiting to.
    pub fn autorotate_starts_in(&self) -> Option<f32> {
        let a = &self.autorotate;
        (a.enabled && self.autorotate_on && self.idle <= a.delay).then_some(a.delay - self.idle)
    }

    /// Starts idle rotation now (or cancels the wait when turned off).
    pub fn set_autorotate(&mut self, on: bool) {
        self.autorotate_on = on;
        self.idle = if on { self.autorotate.delay } else { 0.0 };
    }
}

/// What the camera is doing, which decides how often the view must be redrawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// Nothing changes: no frames are needed.
    Still,
    /// Slow idle rotation: a reduced frame rate is plenty.
    AutoRotating,
    /// The user is steering, or an animation is running: full frame rate.
    Moving,
}
