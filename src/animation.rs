//! Animation controller for smooth frame interpolation
//!
//! Handles interpolation between ML model output frames to provide
//! smooth 60fps visualization during structure prediction/design.

use glam::Vec3;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Animation controller that smoothly interpolates between frames
pub struct AnimationController {
    /// Previous frame positions
    prev_positions: Vec<Vec3>,
    /// Target positions we're interpolating toward
    target_positions: Vec<Vec3>,
    /// Interpolation parameter (0.0 = prev, 1.0 = target)
    t: f32,
    /// Time of last frame transition
    last_update: Instant,
    /// Minimum time between frame transitions
    update_interval: Duration,
    /// Queue of pending position updates from ML stream
    pending_updates: VecDeque<Vec<Vec3>>,
    /// Whether animation is currently active
    is_animating: bool,
}

impl AnimationController {
    /// Create a new animation controller
    pub fn new() -> Self {
        Self {
            prev_positions: Vec::new(),
            target_positions: Vec::new(),
            t: 1.0,
            last_update: Instant::now(),
            update_interval: Duration::from_millis(500), // 2 updates/sec default
            pending_updates: VecDeque::new(),
            is_animating: false,
        }
    }

    /// Set the animation speed (frames per second)
    pub fn set_fps(&mut self, fps: f32) {
        self.update_interval = Duration::from_secs_f32(1.0 / fps);
    }

    /// Set initial positions (no animation, immediate)
    pub fn set_initial(&mut self, positions: Vec<Vec3>) {
        self.prev_positions = positions.clone();
        self.target_positions = positions;
        self.t = 1.0;
        self.is_animating = false;
    }

    /// Enqueue new target positions from ML stream
    pub fn enqueue(&mut self, positions: Vec<Vec3>) {
        self.pending_updates.push_back(positions);
        self.is_animating = true;
    }

    /// Clear all pending updates
    pub fn clear_pending(&mut self) {
        self.pending_updates.clear();
    }

    /// Check if there are pending updates
    pub fn has_pending(&self) -> bool {
        !self.pending_updates.is_empty()
    }

    /// Check if animation is currently active
    pub fn is_animating(&self) -> bool {
        self.is_animating || self.t < 1.0
    }

    /// Tick animation forward, returns interpolated positions if changed
    ///
    /// Call this every frame with the time delta since last frame.
    /// Returns Some(positions) if the positions have changed, None otherwise.
    pub fn tick(&mut self, dt: Duration) -> Option<Vec<Vec3>> {
        // Check if we should advance to next target
        if self.t >= 1.0 && self.last_update.elapsed() >= self.update_interval {
            if let Some(next) = self.pending_updates.pop_front() {
                // If positions count changed, don't animate - just snap
                if !self.target_positions.is_empty()
                    && next.len() != self.target_positions.len()
                {
                    self.prev_positions = next.clone();
                    self.target_positions = next;
                    self.t = 1.0;
                    self.last_update = Instant::now();
                    return Some(self.target_positions.clone());
                }

                // Start animation to new target
                self.prev_positions = std::mem::take(&mut self.target_positions);
                self.target_positions = next;
                self.t = 0.0;
                self.last_update = Instant::now();
            } else {
                // No more pending updates
                self.is_animating = false;
            }
        }

        // Interpolate if animation in progress
        if self.t < 1.0 {
            // Advance t based on dt (complete transition in update_interval time)
            let speed = 1.0 / self.update_interval.as_secs_f32();
            self.t = (self.t + dt.as_secs_f32() * speed).min(1.0);

            // Smooth Hermite interpolation (ease in/out)
            let smooth_t = hermite_ease(self.t);

            return Some(Self::lerp_positions(
                &self.prev_positions,
                &self.target_positions,
                smooth_t,
            ));
        }

        None
    }

    /// Get current positions (may be mid-interpolation)
    pub fn current_positions(&self) -> &[Vec3] {
        if self.t >= 1.0 {
            &self.target_positions
        } else {
            // Return target for simplicity - tick() returns interpolated
            &self.target_positions
        }
    }

    /// Linear interpolation between two position vectors
    fn lerp_positions(a: &[Vec3], b: &[Vec3], t: f32) -> Vec<Vec3> {
        a.iter()
            .zip(b.iter())
            .map(|(a, b)| a.lerp(*b, t))
            .collect()
    }
}

impl Default for AnimationController {
    fn default() -> Self {
        Self::new()
    }
}

/// Hermite smoothstep for ease-in/ease-out interpolation
fn hermite_ease(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_initial() {
        let mut anim = AnimationController::new();
        let positions = vec![Vec3::ZERO, Vec3::ONE];
        anim.set_initial(positions.clone());

        assert_eq!(anim.current_positions().len(), 2);
        assert!(!anim.is_animating());
    }

    #[test]
    fn test_enqueue_and_tick() {
        let mut anim = AnimationController::new();
        anim.set_initial(vec![Vec3::ZERO]);
        anim.enqueue(vec![Vec3::ONE]);

        assert!(anim.has_pending());
        assert!(anim.is_animating());
    }

    #[test]
    fn test_hermite_ease() {
        assert_eq!(hermite_ease(0.0), 0.0);
        assert_eq!(hermite_ease(1.0), 1.0);
        assert!((hermite_ease(0.5) - 0.5).abs() < 0.01);
    }
}
