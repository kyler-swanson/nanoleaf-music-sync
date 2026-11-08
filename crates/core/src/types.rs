//! Shared contracts between the capture, analysis, effect, engine and device code.
//!
//! Parallel tasks code against these types. Do not change them without flagging it.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type PanelId = u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Panel {
    pub id: PanelId,
    pub x: i32,
    pub y: i32,
    pub o: i32,
    pub shape_type: u8,
}

/// Light-emitting panels only; controllers are already removed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layout {
    pub panels: Vec<Panel>,
    pub global_orientation: i32,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub colors: Vec<(PanelId, Rgb)>,
    /// Units of 100 ms, matching the wire format.
    pub transition: u16,
}

pub const BAND_COUNT: usize = 16;

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioFeatures {
    /// 0.0..=1.0 after gain control.
    pub rms: f32,
    /// Low to high, 0.0..=1.0.
    pub bands: [f32; BAND_COUNT],
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    pub onset: f32,
    /// Monotonic.
    pub beats_total: u64,
    /// For latency measurement.
    pub captured_at_micros: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectParams {
    /// 2 to 8 stops.
    pub palette: Vec<Rgb>,
    pub background: Rgb,
    /// 0.1..=3.0.
    pub sensitivity: f32,
    /// 0.0..=1.0.
    pub smoothing: f32,
    /// 0.0..=1.0.
    pub brightness: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub effect_id: String,
    pub params: EffectParams,
}

pub trait Effect: Send {
    fn id(&self) -> &'static str;
    fn render(
        &mut self,
        audio: &AudioFeatures,
        layout: &Layout,
        params: &EffectParams,
        dt_secs: f32,
    ) -> Frame;
}

/// Why a [`LightSink`] could not send a frame.
#[derive(Debug, Error)]
pub enum SinkError {
    #[error("network error while sending frame: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub trait LightSink: Send {
    fn send(&mut self, frame: &Frame) -> Result<(), SinkError>;
}

/// A [`LightSink`] that records every frame instead of sending it.
///
/// Clones share one recording, so a test can keep a clone and inspect the
/// frames after handing the sink to the engine.
#[derive(Clone, Debug, Default)]
pub struct MockSink {
    frames: Arc<Mutex<Vec<Frame>>>,
}

impl MockSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of every frame received so far, oldest first.
    pub fn frames(&self) -> Vec<Frame> {
        self.frames.lock().expect("MockSink lock poisoned").clone()
    }
}

impl LightSink for MockSink {
    fn send(&mut self, frame: &Frame) -> Result<(), SinkError> {
        self.frames
            .lock()
            .expect("MockSink lock poisoned")
            .push(frame.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = Rgb { r: 255, g: 0, b: 0 };
    const BLUE: Rgb = Rgb { r: 0, g: 0, b: 255 };

    fn frame(id: PanelId, color: Rgb, transition: u16) -> Frame {
        Frame {
            colors: vec![(id, color)],
            transition,
        }
    }

    #[test]
    fn mock_sink_records_frames_in_order_across_clones() {
        let probe = MockSink::new();
        let mut sink: Box<dyn LightSink> = Box::new(probe.clone());

        sink.send(&frame(1, RED, 0)).unwrap();
        sink.send(&frame(2, BLUE, 12)).unwrap();

        let frames = probe.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].colors, vec![(1, RED)]);
        assert_eq!(frames[0].transition, 0);
        assert_eq!(frames[1].colors, vec![(2, BLUE)]);
        assert_eq!(frames[1].transition, 12);
    }

    #[test]
    fn effect_works_as_a_trait_object() {
        struct Solid;
        impl Effect for Solid {
            fn id(&self) -> &'static str {
                "solid"
            }
            fn render(
                &mut self,
                _audio: &AudioFeatures,
                layout: &Layout,
                params: &EffectParams,
                _dt_secs: f32,
            ) -> Frame {
                Frame {
                    colors: layout
                        .panels
                        .iter()
                        .map(|panel| (panel.id, params.background))
                        .collect(),
                    transition: 1,
                }
            }
        }

        let layout = Layout {
            panels: vec![Panel {
                id: 374,
                x: 10,
                y: -20,
                o: 60,
                shape_type: 7,
            }],
            global_orientation: 0,
        };
        let mut effect: Box<dyn Effect> = Box::new(Solid);
        let rendered = effect.render(&AudioFeatures::default(), &layout, &params(), 0.05);

        assert_eq!(effect.id(), "solid");
        assert_eq!(rendered.colors, vec![(374, BLUE)]);
    }

    #[test]
    fn audio_features_default_to_silence() {
        let features = AudioFeatures::default();
        assert_eq!(features.bands, [0.0; BAND_COUNT]);
        assert_eq!(features.beats_total, 0);
    }

    #[test]
    fn preset_round_trips_through_json() {
        let preset = Preset {
            name: "Test".into(),
            effect_id: "spectrum".into(),
            params: params(),
        };

        let json = serde_json::to_string(&preset).unwrap();
        let loaded: Preset = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.name, "Test");
        assert_eq!(loaded.effect_id, "spectrum");
        assert_eq!(loaded.params.palette, vec![RED, BLUE]);
        assert_eq!(loaded.params.background, BLUE);
        assert_eq!(loaded.params.sensitivity, 1.5);
        assert_eq!(loaded.params.smoothing, 0.25);
        assert_eq!(loaded.params.brightness, 0.75);
    }

    fn params() -> EffectParams {
        EffectParams {
            palette: vec![RED, BLUE],
            background: BLUE,
            sensitivity: 1.5,
            smoothing: 0.25,
            brightness: 0.75,
        }
    }
}
