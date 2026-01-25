//! Visual effects for ML visualization
//!
//! Provides visual feedback during ML operations including pulsing effects
//! during prediction and special highlighting for designed regions.

/// Visual effect types for different ML operations
#[derive(Debug, Clone)]
pub enum VisualEffect {
    /// No active effect
    None,
    /// Subtle pulsing effect during prediction (SimpleFold)
    Pulsing {
        /// Current phase of the pulse (0 to 2*PI)
        phase: f32,
        /// Pulse frequency in Hz
        frequency: f32,
    },
    /// Highlight designed regions (RFDiffusion3)
    DesignHighlight {
        /// Indices of designed residues/atoms
        designed_indices: Vec<usize>,
        /// Highlight color (RGB)
        color: [f32; 3],
        /// Intensity factor
        intensity: f32,
    },
    /// Progress indicator showing inference step
    Progress {
        /// Current step
        current: u32,
        /// Total steps
        total: u32,
    },
}

impl VisualEffect {
    /// Create a new pulsing effect
    pub fn pulsing() -> Self {
        Self::Pulsing {
            phase: 0.0,
            frequency: 2.0, // 2 Hz
        }
    }

    /// Create a design highlight effect with futuristic purple-blue color
    pub fn design_highlight(designed_indices: Vec<usize>) -> Self {
        Self::DesignHighlight {
            designed_indices,
            color: [0.5, 0.3, 0.9], // Futuristic purple-blue
            intensity: 1.0,
        }
    }

    /// Create a progress indicator
    pub fn progress(current: u32, total: u32) -> Self {
        Self::Progress { current, total }
    }

    /// Update the effect state
    ///
    /// Returns the current intensity multiplier (0.0 - 1.0+)
    pub fn tick(&mut self, dt: f32) -> f32 {
        match self {
            VisualEffect::None => 1.0,

            VisualEffect::Pulsing { phase, frequency } => {
                // Advance phase
                *phase = (*phase + dt * *frequency * std::f32::consts::TAU)
                    % std::f32::consts::TAU;

                // Subtle intensity oscillation (0.8 to 1.0)
                0.9 + 0.1 * (*phase).sin()
            }

            VisualEffect::DesignHighlight { intensity, .. } => {
                // Gentle fade-in effect
                *intensity = (*intensity + dt * 2.0).min(1.0);
                *intensity
            }

            VisualEffect::Progress { current, total } => {
                if *total > 0 {
                    *current as f32 / *total as f32
                } else {
                    0.0
                }
            }
        }
    }

    /// Get the highlight color if applicable
    pub fn get_highlight_color(&self) -> Option<[f32; 3]> {
        match self {
            VisualEffect::DesignHighlight { color, .. } => Some(*color),
            _ => None,
        }
    }

    /// Get the designed indices if applicable
    pub fn get_designed_indices(&self) -> Option<&[usize]> {
        match self {
            VisualEffect::DesignHighlight { designed_indices, .. } => {
                Some(designed_indices)
            }
            _ => None,
        }
    }

    /// Check if the effect is active
    pub fn is_active(&self) -> bool {
        !matches!(self, VisualEffect::None)
    }

    /// Update progress
    pub fn update_progress(&mut self, current: u32, total: u32) {
        if let VisualEffect::Progress { current: c, total: t } = self {
            *c = current;
            *t = total;
        }
    }

    /// Get progress percentage (0-100)
    pub fn get_progress_percent(&self) -> Option<f32> {
        match self {
            VisualEffect::Progress { current, total } if *total > 0 => {
                Some(100.0 * *current as f32 / *total as f32)
            }
            _ => None,
        }
    }
}

impl Default for VisualEffect {
    fn default() -> Self {
        Self::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pulsing_effect() {
        let mut effect = VisualEffect::pulsing();
        let intensity1 = effect.tick(0.0);
        let intensity2 = effect.tick(0.25); // Quarter period

        // Intensity should be in range
        assert!(intensity1 >= 0.8 && intensity1 <= 1.0);
        assert!(intensity2 >= 0.8 && intensity2 <= 1.0);
    }

    #[test]
    fn test_design_highlight() {
        let effect = VisualEffect::design_highlight(vec![0, 1, 2]);
        assert!(effect.is_active());
        assert_eq!(effect.get_designed_indices(), Some(&[0, 1, 2][..]));
        assert!(effect.get_highlight_color().is_some());
    }

    #[test]
    fn test_progress() {
        let mut effect = VisualEffect::progress(50, 100);
        assert_eq!(effect.get_progress_percent(), Some(50.0));

        effect.update_progress(75, 100);
        assert_eq!(effect.get_progress_percent(), Some(75.0));
    }
}
