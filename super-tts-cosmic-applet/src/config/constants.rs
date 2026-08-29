// SPDX-License-Identifier: GPL-3.0-only
// =============================================================================
// FREQUENCY VISUALIZATION CONFIGURATION
// =============================================================================
// Controls how frequency data is normalized and displayed in the equalizer

/// Expected maximum amplitude for frequency normalization
/// This determines the scaling of frequency bars:
/// - Lower values (1.0-2.0): More sensitive, bars fill up easier
/// - Higher values (5.0-10.0): Less sensitive, need louder audio to fill bars
/// - Recommended range: 2.0-5.0 for good balance
pub const FREQUENCY_NORMALIZATION_MAX: f32 = 3.0;

// =============================================================================
// DYNAMIC FREQUENCY MAPPING CONFIGURATION
// =============================================================================
// Controls how detected audio frequencies map to wave visualization frequencies

/// Minimum visualization frequency (when audio frequency is very low)
/// This prevents visualization waves from becoming too slow and boring
pub const MIN_VISUALIZATION_WAVE_FREQUENCY: f32 = 8.0;

/// Maximum visualization frequency (when audio frequency is very high)
/// This prevents visualization waves from becoming too fast and chaotic
pub const MAX_VISUALIZATION_WAVE_FREQUENCY: f32 = 60.0;

/// Default visualization frequency (when no clear dominant frequency is detected)
/// This is used when confidence is low or no audio is present
pub const DEFAULT_VISUALIZATION_WAVE_FREQUENCY: f32 = 28.0; // Original constant

/// Audio frequency range for mapping (Hz)
/// Frequencies outside this range are clamped to the edges
pub const MIN_AUDIO_FREQUENCY: f32 = 80.0; // Lowest meaningful speech frequency
pub const MAX_AUDIO_FREQUENCY: f32 = 1600.0; // Upper range of typical speech fundamentals

/// Confidence threshold for using dynamic frequency
/// Below this threshold, fall back to default frequency for stability
pub const FREQUENCY_CONFIDENCE_THRESHOLD: f32 = 0.3;

/// Smoothing factor for frequency changes (0.0 = no smoothing, 1.0 = no change)
/// This prevents jarring visual transitions when frequency changes rapidly
pub const FREQUENCY_SMOOTHING: f32 = 0.5;
