//! `greg_gui_core` - Minimal GUI primitives for Greg Analog Suite
//! Based on `nih_plug_vizia` + `baseview` per specifiche GUI_SPEC.md
//!
//! Questo crate fornisce primitive GUI standardizzate per tutti i plugin Greg.
//! L'obiettivo è zero allocazione nel thread audio (`process()`) e coerenza visiva.

use num_traits::Float;
use vizia::vg::Paint;
use vizia::prelude::*;
use baseview::WindowBuilder;
use std::sync::Arc;

/// Primary knob component - vectoriale, configurabile
#[derive(Clone, Copy)]
pub struct GregKnob {
    pub label: String,
    pub value: f32,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub numeric_visible: bool,
    pub fine_mode: bool,
}

impl GregKnob {
    /// Crea un nuovo knob
    pub fn new(label: &str, min: f32, max: f32, default: f32) -> Self {
        Self {
            label: label.to_string(),
            value: default.clamp(min, max),
            min,
            max,
            step: (max - min) / 100.0,
            numeric_visible: true,
            fine_mode: false,
        }
    }

    /// Imposta il valore (clamped)
    pub fn set_value(&mut self, value: f32) {
        self.value = value.clamp(self.min, self.max);
    }

    /// Toggle fine mode (Shift+Drag behavior)
    pub fn toggle_fine(&mut self) {
        self.fine_mode = !self.fine_mode;
    }
}

/// Button component - toggle o selettore multi-posizione
#[derive(Clone, Copy)]
pub struct GregButton {
    pub label: String,
    pub active: bool,
    pub items: Vec<String>,
    pub multi_select: bool,
}

impl GregButton {
    /// Crea un button toggle semplice
    pub fn toggle(label: &str, active: bool) -> Self {
        Self {
            label: label.to_string(),
            active,
            items: Vec::new(),
            multi_select: false,
        }
    }

    /// Crea un selettore multi-posizione
    pub fn selector(label: &str, items: Vec<&str>, active_index: usize) -> Self {
        Self {
            label: label.to_string(),
            active: false,
            items: items.iter().map(|s| s.to_string()).collect(),
            multi_select: false,
        }
    }
}

/// Slider component - fader lineare
#[derive(Clone, Copy)]
pub struct GregSlider {
    pub label: String,
    pub value: f32,
    pub min: f32,
    pub max: f32,
    pub vertical: bool,
}

impl GregSlider {
    /// Crea un nuovo slider
    pub fn new(label: &str, min: f32, max: f32, default: f32, vertical: bool) -> Self {
        Self {
            label: label.to_string(),
            value: default.clamp(min, max),
            min,
            max,
            vertical,
        }
    }

    /// Imposta il valore (clamped)
    pub fn set_value(&mut self, value: f32) {
        self.value = value.clamp(self.min, self.max);
    }
}

/// Spectrometro FFT - attivo solo quando Advanced Panel è aperto
#[derive(Clone)]
pub struct GregSpectrum {
    pub activated: bool,
    pub samples: Vec<f32>,
    pub max_val: f32,
}

impl GregSpectrum {
    /// Crea uno spettrometro spento
    pub fn new() -> Self {
        Self {
            activated: false,
            samples: Vec::new(),
            max_val: 0.0,
        }
    }

    /// Aggiorna i dati dello spettrometro (solo thread GUI)
    pub fn update(&mut self, new_samples: Vec<f32>) {
        self.samples = new_samples;
        self.max_val = new_samples.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
    }
}

/// Meter multicanale - per le colonne SX/DX nella finestra
#[derive(Clone)]
pub struct ChannelMeter {
    pub label: String,
    pub values: Vec<f32>,
    pub peaks: Vec<f32>,
    pub peak_hold: f32,
    pub clip_indicator: bool,
}

impl ChannelMeter {
    /// Crea un meter per un canale
    pub fn new(label: &str, num_channels: usize) -> Self {
        let values: Vec<f32> = std::iter::repeat(0.0).take(num_channels).collect();
        let peaks: Vec<f32> = std::iter::repeat(0.0).take(num_channels).collect();
        Self {
            label: label.to_string(),
            values,
            peaks,
            peak_hold: 0.0,
            clip_indicator: false,
        }
    }

    /// Aggiorna il valore di un canale
    pub fn update_channel(&mut self, channel: usize, value: f32) {
        if channel < self.values.len() {
            self.values[channel] = value.max(0.0);
            if self.values[channel] > self.peaks[channel] {
                self.peaks[channel] = self.values[channel];
            }
            // Decay peak hold
            self.peaks[channel] *= 0.999; // 1.5s decay approssimativo
        }
    }

    /// Imp indica clip
    pub fn set_clip(&mut self, channel: usize, clip: bool) {
        if channel < self.values.len() {
            if clip {
                self.clip_indicator = true;
            }
        }
    }

    /// Reset clip indicator (su click)
    pub fn reset_clip(&mut self) {
        self.clip_indicator = false;
    }
}

/// Footer meter - phase correlation + gain reduction
#[derive(Clone)]
pub struct FooterMeter {
    pub phase: f32, // -1.0 (rosso) <-> 0.0 (giallo) <-> +1.0 (verde)
    pub gain_reduction: f32, // in dB (negativo, es. -4.2)
    pub gr_visible: bool,
}

impl FooterMeter {
    /// Crea footer meter valori di default
    pub fn default() -> Self {
        Self {
            phase: 0.0,
            gain_reduction: -12.0,
            gr_visible: true,
        }
    }
}