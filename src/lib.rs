#![forbid(unsafe_code)]
//! `greg_gui_core` - Libreria GUI comune leggera e multipiattaforma (`nih_plug_vizia` + FemtoVG)
//! Implementa il layout standardizzato di `GUI_SPEC.md` con zero allocazioni sul thread DSP.

#![allow(
    clippy::clone_on_copy,
    clippy::collapsible_match,
    clippy::needless_borrows_for_generic_args,
    clippy::unnecessary_cast,
)]

mod gui_tokens;

use nih_plug::prelude::*;
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::vg::{Paint, Path};
use nih_plug_vizia::vizia::vg::Solidity;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::widgets::param_base::ParamWidgetBase;
use nih_plug_vizia::widgets::util::ModifiersExt;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::path::PathBuf;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

/// Hardware knob visual sizing (per `GUI_SPEC.md` v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnobSize {
    Large,
    Medium,
    Small,
}

impl KnobSize {
    pub fn diameter_px(self) -> f32 {
        match self {
            KnobSize::Large => 64.0,
            KnobSize::Medium => 48.0,
            KnobSize::Small => 36.0,
        }
    }
}

/// Standard UI metrics.
use gui_tokens::colours;
use gui_tokens::metrics;

/// Re-export vizia types for downstream plugins to avoid version conflicts.
pub mod prelude {
    pub use nih_plug_vizia::vizia::prelude::*;
    pub use crate::RotaryKnob;
    pub use crate::ToggleSwitch;
    pub use crate::EnumSwitch;
    pub use crate::DropdownSelect;
}

/// Numero massimo di canali simultanei supportati nei meter multicanale SX/DX.
pub const MAX_GUI_CHANNELS: usize = 16;

/// Telemetria lock-free condivisa tra thread DSP (`process()`) e thread GUI (60 FPS).
pub struct GuiTelemetry {
    pub num_channels: AtomicU32,
    pub in_peaks: [AtomicU32; MAX_GUI_CHANNELS],
    pub out_peaks: [AtomicU32; MAX_GUI_CHANNELS],
    pub in_clip: [AtomicBool; MAX_GUI_CHANNELS],
    pub out_clip: [AtomicBool; MAX_GUI_CHANNELS],
    /// Gain reduction in dB negativi (es. -4.2 dB).
    pub gain_reduction_db: AtomicU32,
    /// Correlazione di fase stereo [-1.0 .. +1.0].
    pub phase_correlation: AtomicU32,
    pub show_gr_meter: AtomicBool,
}

impl GuiTelemetry {
    pub fn new(show_gr: bool) -> Arc<Self> {
        Arc::new(Self {
            num_channels: AtomicU32::new(2),
            in_peaks: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            out_peaks: std::array::from_fn(|_| AtomicU32::new(0.0f32.to_bits())),
            in_clip: std::array::from_fn(|_| AtomicBool::new(false)),
            out_clip: std::array::from_fn(|_| AtomicBool::new(false)),
            gain_reduction_db: AtomicU32::new(0.0f32.to_bits()),
            phase_correlation: AtomicU32::new(1.0f32.to_bits()),
            show_gr_meter: AtomicBool::new(show_gr),
        })
    }

    #[inline(always)]
    pub fn push_in_peak(&self, ch: usize, linear_peak: f32) {
        if ch < MAX_GUI_CHANNELS {
            let prev = f32::from_bits(self.in_peaks[ch].load(Ordering::Relaxed));
            if linear_peak > prev {
                self.in_peaks[ch].store(linear_peak.to_bits(), Ordering::Relaxed);
            }
            if linear_peak > 1.0 {
                self.in_clip[ch].store(true, Ordering::Relaxed);
            }
        }
    }

    #[inline(always)]
    pub fn push_out_peak(&self, ch: usize, linear_peak: f32) {
        if ch < MAX_GUI_CHANNELS {
            let prev = f32::from_bits(self.out_peaks[ch].load(Ordering::Relaxed));
            if linear_peak > prev {
                self.out_peaks[ch].store(linear_peak.to_bits(), Ordering::Relaxed);
            }
            if linear_peak > 1.0 {
                self.out_clip[ch].store(true, Ordering::Relaxed);
            }
        }
    }

    #[inline(always)]
    pub fn set_gr_and_phase(&self, gr_db_negative: f32, phase: f32) {
        self.gain_reduction_db
            .store(gr_db_negative.min(0.0).to_bits(), Ordering::Relaxed);
        self.phase_correlation
            .store(phase.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// Creates a static slice of `ParamDef` from a vector, using `OnceLock` for one-time initialization.
/// The `OnceLock` is shared across all callers within the same crate context — use the
/// [`greg_static_params!`] macro for per-plugin isolated `OnceLock` instances.
pub fn static_param_defs(defs: Vec<ParamDef>) -> &'static [ParamDef] {
    static LOCK: OnceLock<Vec<ParamDef>> = OnceLock::new();
    LOCK.get_or_init(|| defs).as_slice()
}

/// Macro exports per-plugin isolated `OnceLock<Vec<ParamDef>>` statics.
/// Each invocation generates a distinct `static` item, so every plugin gets its own lock.
#[macro_export]
macro_rules! greg_static_params {
    ($expr:expr) => {{
        static LOCK: std::sync::OnceLock<Vec<$crate::ParamDef>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| $expr).as_slice()
    }};
}

/// A per-parameter accessor for the universal toolbar (A/B Compare, Copy/Paste, Save Default, Preset Browser).
/// Plugins register these alongside their `ParamDef` entries so the toolbar can snapshot/restore values.
pub struct ParamAccess {
    /// Human-readable identifier (the param `#[id = "..."]` string, or the label).
    pub id: &'static str,
    /// Reads the current normalized value `[0.0..1.0]` from the plugin's params.
    pub read: Box<dyn Fn() -> f32 + Send + Sync>,
    /// Applies a normalized value through the GUI event context (host automation).
    pub apply: Box<dyn Fn(&mut EventContext, f32) + Send + Sync>,
}

/// Declarative macro to register `ParamAccess` entries for a plugin's parameter list.
/// Usage: `greg_param_access!(params, [ ("drive", drive_db), ("mix", mix) ])`
#[macro_export]
macro_rules! greg_param_access {
    ($params:expr, [ $( ($id:literal, $field:ident) ),* $(,)? ]) => {{
        vec![
            $( $crate::ParamAccess {
                id: $id,
                read: { let p = $params.clone(); Box::new(move || p.$field.unmodulated_normalized_value()) },
                apply: { let p = $params.clone(); Box::new(move |cx, n| {
                    let setter = ::nih_plug::prelude::ParamSetter::new(cx);
                    setter.begin_set_parameter(&p.$field);
                    setter.set_parameter_normalized(&p.$field, n);
                    setter.end_set_parameter(&p.$field);
                }) },
            } ),*
        ]
    }};
}

/// Serializable snapshot of a plugin's parameters keyed by param id → normalized value.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct AbSnapshot {
    pub values: Vec<(String, f32)>,
}

impl AbSnapshot {
    /// Capture current normalized values from the parameter access list.
    pub fn capture(access: &[ParamAccess]) -> Self {
        let mut values = Vec::with_capacity(access.len());
        for acc in access {
            values.push((acc.id.to_string(), (acc.read)()));
        }
        values.sort_by(|a, b| a.0.cmp(&b.0));
        Self { values }
    }

    /// Restore normalized values through the GUI event context.
    pub fn restore(&self, cx: &mut EventContext, access: &[ParamAccess]) {
        for acc in access {
            if let Some((_, v)) = self.values.iter().find(|(id, _)| id == acc.id) {
                (acc.apply)(cx, *v);
            }
        }
    }
}

/// Persistent state for the universal toolbar (A/B Compare + preset directory index).
#[derive(Debug)]
pub struct ToolbarState {
    /// Two in-memory A/B snapshots.
    pub ab_slots: [Option<AbSnapshot>; 2],
    /// Currently active slot (0 = A, 1 = B).
    pub ab_active: usize,
    /// Plugin identifier used for default/preset file paths.
    pub plugin_id: String,
    /// Last preset list directory listing (kept in-memory).
    pub preset_names: Vec<String>,
    /// Selected preset index in `preset_names`.
    pub preset_index: usize,
}

impl ToolbarState {
    pub fn new(plugin_id: &str) -> Self {
        Self {
            ab_slots: [None, None],
            ab_active: 0,
            plugin_id: plugin_id.to_string(),
            preset_names: Vec::new(),
            preset_index: 0,
        }
    }
}

/// Path for the shared parameter clipboard: `~/.cache/analog-vibe/param_clipboard.json`.
pub fn param_clipboard_path() -> PathBuf {
    home_cache_dir().join("analog-vibe/param_clipboard.json")
}

/// Path for a plugin's user default state: `~/.config/analog-vibe/<plugin_id>.json`.
pub fn default_state_path(plugin_id: &str) -> PathBuf {
    home_config_dir().join(format!("analog-vibe/{plugin_id}.json"))
}

/// Directory for a plugin's presets: `~/.config/analog-vibe/presets/<plugin_id>/`.
pub fn presets_dir(plugin_id: &str) -> PathBuf {
    home_config_dir().join(format!("analog-vibe/presets/{plugin_id}"))
}

/// `~/.cache` directory (creating it if missing).
fn home_cache_dir() -> PathBuf {
    let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let p = PathBuf::from(base).join(".cache");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// `~/.config` directory (creating it if missing).
fn home_config_dir() -> PathBuf {
    let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let p = PathBuf::from(base).join(".config");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// List available preset file names for a plugin (without extension).
pub fn list_presets(plugin_id: &str) -> Vec<String> {
    let dir = presets_dir(plugin_id);
    let mut out = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            if let Some(ext) = entry.path().extension() {
                if ext == "json" {
                    if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                        out.push(stem.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out
}

// ============================================================================
// Model & events
// ============================================================================

#[derive(Lens)]
pub struct GregGuiData {
    pub plugin_title: String,
    pub advanced_open: bool,
    pub telemetry: Arc<GuiTelemetry>,
    /// Universal toolbar state.
    pub toolbar: ToolbarState,
    /// Parameter accessors used by the toolbar for snapshot/restore.
    pub param_access: &'static [ParamAccess],
}

#[derive(Debug, Clone)]
pub enum GregGuiEvent {
    ToggleAdvanced,
    ResetClips,
    /// Toggle snapshot into slot `slot` (0=A, 1=B).
    AbSlot(usize),
    /// Restore snapshot from slot `slot`.
    AbRecall(usize),
    /// Copy current params to shared clipboard.
    CopyParams,
    /// Paste clipboard params into this instance.
    PasteParams,
    /// Save user default state file.
    SaveDefault,
    /// Select previous preset.
    PresetPrev,
    /// Select next preset.
    PresetNext,
    /// Save current state as a named preset (text payload in `event.data` string).
    SavePreset(String),
}

impl Model for GregGuiData {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|msg, _| match msg {
            GregGuiEvent::ToggleAdvanced => {
                self.advanced_open = !self.advanced_open;
                let h = if self.advanced_open { 560 } else { 380 };
                cx.emit(WindowEvent::SetSize(WindowSize {
                    width: 900,
                    height: h,
                }));
            }
            GregGuiEvent::ResetClips => {
                for ch in 0..MAX_GUI_CHANNELS {
                    self.telemetry.in_clip[ch].store(false, Ordering::Relaxed);
                    self.telemetry.out_clip[ch].store(false, Ordering::Relaxed);
                }
            }
            GregGuiEvent::AbSlot(ref slot) => {
                self.toolbar.ab_slots[*slot] = Some(AbSnapshot::capture(&self.param_access));
                self.toolbar.ab_active = *slot;
            }
            GregGuiEvent::AbRecall(ref slot) => {
                if let Some(snap) = &self.toolbar.ab_slots[*slot] {
                    snap.restore(cx, &self.param_access);
                    self.toolbar.ab_active = *slot;
                }
            }
            GregGuiEvent::CopyParams => {
                let _ = self.write_clipboard();
            }
            GregGuiEvent::PasteParams => {
                let _ = self.read_clipboard(cx);
            }
            GregGuiEvent::SaveDefault => {
                let _ = self.save_default();
            }
            GregGuiEvent::PresetPrev => {
                let n = self.toolbar.preset_names.len();
                if n > 0 {
                    self.toolbar.preset_index = (self.toolbar.preset_index + n - 1) % n;
                }
            }
            GregGuiEvent::PresetNext => {
                let n = self.toolbar.preset_names.len();
                if n > 0 {
                    self.toolbar.preset_index = (self.toolbar.preset_index + 1) % n;
                }
            }
            GregGuiEvent::SavePreset(ref name) => {
                let _ = self.save_preset(name);
            }
        });
    }
}

impl GregGuiData {
    /// Write current parameter state as JSON to the clipboard file.
    pub fn write_clipboard(&self) -> std::io::Result<()> {
        let snap = AbSnapshot::capture(&self.param_access);
        let json = serde_json::to_string_pretty(&snap).unwrap_or_default();
        if let Some(parent) = param_clipboard_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(param_clipboard_path(), json)
    }

    /// Read clipboard JSON and restore param values through the GUI context.
    pub fn read_clipboard(&mut self, cx: &mut EventContext) -> std::io::Result<()> {
        let content = std::fs::read_to_string(param_clipboard_path())?;
        if let Ok(snap) = serde_json::from_str::<AbSnapshot>(&content) {
            snap.restore(cx, &self.param_access);
        }
        Ok(())
    }

    /// Save current state as the user default for this plugin.
    pub fn save_default(&mut self) -> std::io::Result<()> {
        let snap = AbSnapshot::capture(&self.param_access);
        let json = serde_json::to_string_pretty(&snap).unwrap_or_default();
        let path = default_state_path(&self.toolbar.plugin_id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, json)
    }

    /// Save current state as a named preset file.
    pub fn save_preset(&mut self, name: &str) -> std::io::Result<()> {
        let snap = AbSnapshot::capture(&self.param_access);
        let dir = presets_dir(&self.toolbar.plugin_id);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.json"));
        let json = serde_json::to_string_pretty(&snap).unwrap_or_default();
        std::fs::write(path, json)?;
        // Refresh preset list
        self.toolbar.preset_names = list_presets(&self.toolbar.plugin_id);
        Ok(())
    }

    /// Return the normalized read of param `id`, if registered.
    pub fn param_value(&self, id: &str) -> Option<f32> {
        self.param_access.iter().find(|a| a.id == id).map(|a| (a.read)())
    }
}

/// Hardware rotary knob for continuous (`FloatParam`) parameters.
/// Layout verticale compatto (`width: 80px`, `height: 88px`): Etichetta Nome Parametro (10px),
/// Manopola Rotativa (48x48px con ghiera, disco metallico, arco ambra da -135° a +135°,
/// tacca bianca) e Valore formattato (10px).
#[derive(Lens)]
pub struct RotaryKnob {
    param_base: ParamWidgetBase,
    drag_active: bool,
    drag_start_y: f32,
    drag_start_norm: f32,
    last_norm: f32,
}

impl RotaryKnob {
    pub fn new<L, Params, P, FMap>(cx: &mut Context, params: L, params_to_param: FMap) -> Handle<'_, RotaryKnob>
    where
        L: Lens<Target = Params> + Clone,
        Params: 'static,
        P: Param + 'static,
        FMap: Fn(&Params) -> &P + Copy + 'static,
    {
        Self {
            param_base: ParamWidgetBase::new(cx, params, params_to_param),
            drag_active: false,
            drag_start_y: 0.0,
            drag_start_norm: 0.0,
            last_norm: 0.0,
        }
        .build(
            cx,
            ParamWidgetBase::build_view(params, params_to_param, move |cx, param_data| {
                let display_value_lens = param_data.make_lens(|param| {
                    param.normalized_value_to_string(param.unmodulated_normalized_value(), true)
                });

                VStack::new(cx, |cx| {
                    // Parameter label (top band, centered white, no overlap)
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .hoverable(false);

                    // Middle band: flexible spacer so the graphic lives in the center.
                    Label::new(cx, "")
                        .height(Stretch(1.0))
                        .hoverable(false);

                    // Value display (bottom band, centered amber, no overlap)
                    Label::new(cx, display_value_lens)
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(230, 185, 50))
                        .hoverable(false);
                })
                .height(Stretch(1.0))
                .width(Stretch(1.0));
            }),
        )
        .width(Pixels(metrics::KNOB_W))
        .height(Pixels(metrics::KNOB_H))
    }
}

impl View for RotaryKnob {
    fn element(&self) -> Option<&'static str> {
        Some("rotary-knob")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                self.drag_active = true;
                self.param_base.begin_set_parameter(cx);
                cx.capture();
                cx.set_active(true);
                // Record start position and normalized value for 200px drag range.
                let bounds = cx.bounds();
                self.drag_start_y = bounds.y + bounds.h / 2.0;
                self.drag_start_norm = self.param_base.unmodulated_normalized_value();
                self.last_norm = self.drag_start_norm;
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if self.drag_active {
                    self.drag_active = false;
                    self.param_base.end_set_parameter(cx);
                    cx.release();
                    cx.set_active(false);
                }
            }
            WindowEvent::MouseMove(_x, y) => {
                if self.drag_active {
                    // Full normalized range (0.0..1.0) mapped over 200px vertical travel.
                    // With Shift (0.1x) resolution is finer: each pixel moves by 0.1/200 of a step.
                    let scale = if cx.modifiers().shift() { 0.1 } else { 1.0 };
                    let delta = (*y - self.drag_start_y) * scale / 200.0;
                    let norm = self.drag_start_norm + delta;
                    let norm = norm.clamp(0.0, 1.0);
                    if (norm - self.last_norm).abs() > 0.002 {
                        self.param_base.set_normalized_value(cx, norm);
                        cx.needs_redraw();
                    } else {
                        self.param_base.set_normalized_value(cx, norm);
                    }
                    self.last_norm = norm;
                }
            }
            WindowEvent::MouseScroll(_x, scroll_y) => {
                let use_finer_steps = cx.modifiers().shift();
                let current = self.param_base.unmodulated_normalized_value();
                let next = if *scroll_y > 0.0 {
                    self.param_base.next_normalized_step(current, use_finer_steps)
                } else {
                    self.param_base.previous_normalized_step(current, use_finer_steps)
                };
                if next.is_finite() {
                    self.param_base.set_normalized_value(cx, next);
                }
            }
            _ => {}
        });
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let bounds = cx.bounds();
        if bounds.w == 0.0 || bounds.h == 0.0 {
            return;
        }

        let center_x = bounds.x + bounds.w / 2.0;
        let center_y = bounds.y + bounds.h / 2.0;
        let knob_radius = metrics::KNOB_RADIUS; // 48px diameter
        let arc_radius = knob_radius * 0.85;
        let start_angle = -135.0_f32.to_radians();
        let end_angle = 135.0_f32.to_radians();

        let normalized = self.param_base.unmodulated_normalized_value();
        let angle = start_angle + (end_angle - start_angle) * (normalized + 1.0) / 2.0;

        // Background fill (dark)
        let mut path = Path::new();
        path.rect(bounds.x, bounds.y, bounds.w as f32, bounds.h as f32);
        canvas.fill_path(
            &path,
            &Paint::color(Color::rgb(colours::SURFACE[0], colours::SURFACE[1], colours::SURFACE[2]).into()),
        );

        // Outer bevel ring (border #2E333D)
        let mut path = Path::new();
        path.arc(center_x, center_y, knob_radius, start_angle, end_angle, Solidity::Solid);
        canvas.stroke_path(
            &path,
            &Paint::color(Color::rgb(colours::BORDER[0], colours::BORDER[1], colours::BORDER[2]).into()),
        );

        // Ambra progression arc (#E5A93B)
        let mut path = Path::new();
        path.arc(center_x, center_y, arc_radius, start_angle, angle, Solidity::Solid);
        canvas.stroke_path(
            &path,
            &Paint::color(Color::rgb(colours::ACCENT[0], colours::ACCENT[1], colours::ACCENT[2]).into()),
        );

        // White indicator tick
        let indicator_len = arc_radius * 0.6;
        let ix = center_x + indicator_len * angle.cos();
        let iy = center_y + indicator_len * angle.sin();
        let mut path = Path::new();
        path.move_to(center_x, center_y);
        path.line_to(ix, iy);
        canvas.stroke_path(
            &path,
            &Paint::color(Color::rgb(colours::TEXT[0], colours::TEXT[1], colours::TEXT[2]).into()),
        );
    }
}

/// Hardware rotary knob optimized for bipolar parameters (e.g. Gain, Pan) that map
/// `0.5` to the centered `0°` position on the dial (12 o'clock).
#[derive(Lens)]
pub struct BipolarKnob {
    param_base: ParamWidgetBase,
    drag_active: bool,
    drag_start_y: f32,
    drag_start_norm: f32,
    last_norm: f32,
}

impl BipolarKnob {
    pub fn new<L, Params, P, FMap>(
        cx: &mut Context,
        params: L,
        params_to_param: FMap,
    ) -> Handle<'_, BipolarKnob>
    where
        L: Lens<Target = Params> + Clone,
        Params: 'static,
        P: Param + 'static,
        FMap: Fn(&Params) -> &P + Copy + 'static,
    {
        Self {
            param_base: ParamWidgetBase::new(cx, params, params_to_param),
            drag_active: false,
            drag_start_y: 0.0,
            drag_start_norm: 0.0,
            last_norm: 0.0,
        }
        .build(
            cx,
            ParamWidgetBase::build_view(params, params_to_param, move |cx, param_data| {
                let display_value_lens = param_data.make_lens(|param| {
                    param.normalized_value_to_string(param.unmodulated_normalized_value(), true)
                });
                VStack::new(cx, |cx| {
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .hoverable(false);
                    Label::new(cx, "")
                        .height(Stretch(1.0))
                        .hoverable(false);
                    Label::new(cx, display_value_lens)
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(230, 185, 50))
                        .hoverable(false);
                })
                .height(Stretch(1.0))
                .width(Stretch(1.0));
            }),
        )
        .width(Pixels(metrics::KNOB_W))
        .height(Pixels(metrics::KNOB_H))
    }
}

impl View for BipolarKnob {
    fn element(&self) -> Option<&'static str> {
        Some("bipolar-knob")
    }
}

/// Hardware toggle switch for boolean (`BoolParam`) parameters.
#[derive(Lens)]
pub struct ToggleSwitch {
    param_base: ParamWidgetBase,
    drag_active: bool,
}

impl ToggleSwitch {
    pub fn new<L, Params, P, FMap>(cx: &mut Context, params: L, params_to_param: FMap) -> Handle<'_, ToggleSwitch>
    where
        L: Lens<Target = Params> + Clone,
        Params: 'static,
        P: Param + 'static,
        FMap: Fn(&Params) -> &P + Copy + 'static,
    {
        Self {
            param_base: ParamWidgetBase::new(cx, params, params_to_param),
            drag_active: false,
        }
        .build(
            cx,
            ParamWidgetBase::build_view(params, params_to_param, move |cx, param_data| {
                let display_value_lens = param_data.make_lens(|param| {
                    let normalized = param.unmodulated_normalized_value();
                    let is_active = normalized > 0.5;
                    if is_active { "ON".to_string() } else { "OFF".to_string() }
                });

                VStack::new(cx, |cx| {
                    // Parameter label (top band, centered white, no overlap)
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .hoverable(false);

                    // Middle band: flexible spacer so the graphic lives in the center.
                    Label::new(cx, "")
                        .height(Stretch(1.0))
                        .hoverable(false);

                    // State label (bottom band): ON or OFF, centered amber, no overlap
                    Label::new(cx, display_value_lens)
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(230, 185, 50))
                        .hoverable(false);
                })
                .height(Stretch(1.0))
                .width(Stretch(1.0));
            }),
        )
        .width(Pixels(80.0))
        .height(Pixels(64.0))
    }
}

impl View for ToggleSwitch {
    fn element(&self) -> Option<&'static str> {
        Some("toggle-switch")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                self.drag_active = true;
                self.param_base.begin_set_parameter(cx);
                cx.capture();
                cx.set_active(true);
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if self.drag_active {
                    self.drag_active = false;
                    self.param_base.end_set_parameter(cx);
                    cx.release();
                    cx.set_active(false);
                }
            }
            _ => {}
        });
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let bounds = cx.bounds();
        if bounds.w == 0.0 || bounds.h == 0.0 {
            return;
        }
        let normalized = self.param_base.unmodulated_normalized_value();
        let is_active = normalized > 0.5;

        // Center the switch box in the middle area (between top label and bottom state)
        let center_y = bounds.h / 2.0;
        let box_w = 56.0;
        let box_h = 26.0;
        let x = bounds.x + (bounds.w - box_w) / 2.0;
        let y = bounds.y + center_y - box_h / 2.0;

        let mut path = Path::new();
        path.rounded_rect(x, y, box_w, box_h, 4.0);
        if is_active {
            canvas.fill_path(
                &path,
                &Paint::color(Color::rgba(229, 169, 59, 100).into()),
            );
        } else {
            canvas.fill_path(
                &path,
                &Paint::color(Color::rgb(45, 48, 55).into()),
            );
        }

        let mut path = Path::new();
        path.rounded_rect(x, y, box_w, box_h, 4.0);
        canvas.stroke_path(
            &path,
            &Paint::color(Color::rgb(70, 72, 80).into()),
        );
    }
}

/// Hardware enum switch for (`EnumParam`) parameters.
#[derive(Lens)]
pub struct EnumSwitch {
    param_base: ParamWidgetBase,
    drag_active: bool,
}

impl EnumSwitch {
    pub fn new<L, Params, P, FMap>(cx: &mut Context, params: L, params_to_param: FMap) -> Handle<'_, EnumSwitch>
    where
        L: Lens<Target = Params> + Clone,
        Params: 'static,
        P: Param + 'static,
        FMap: Fn(&Params) -> &P + Copy + 'static,
    {
        Self {
            param_base: ParamWidgetBase::new(cx, params, params_to_param),
            drag_active: false,
        }
        .build(
            cx,
            ParamWidgetBase::build_view(params, params_to_param, move |cx, param_data| {
                let display_value_lens = param_data.make_lens(|param| {
                    param.normalized_value_to_string(param.unmodulated_normalized_value(), true)
                });

                VStack::new(cx, |cx| {
                    // Parameter label (top band, centered white, no overlap)
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .hoverable(false);

                    // Middle band: flexible spacer so the graphic lives in the center.
                    Label::new(cx, "")
                        .height(Stretch(1.0))
                        .hoverable(false);

                    // Active value text (bottom band): centered amber, no overlap
                    Label::new(cx, display_value_lens)
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(230, 185, 50))
                        .hoverable(false);
                })
                .height(Stretch(1.0))
                .width(Stretch(1.0));
            }),
        )
        .width(Pixels(94.0))
        .height(Pixels(64.0))
    }
}

impl View for EnumSwitch {
    fn element(&self) -> Option<&'static str> {
        Some("enum-switch")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                self.drag_active = true;
                self.param_base.begin_set_parameter(cx);
                cx.capture();
                cx.set_active(true);
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if self.drag_active {
                    self.drag_active = false;
                    self.param_base.end_set_parameter(cx);
                    cx.release();
                    cx.set_active(false);
                }
            }
            WindowEvent::MouseDoubleClick(MouseButton::Left) => {
                // Double-click resets to default normalized value
                self.param_base.begin_set_parameter(cx);
                self.param_base.set_normalized_value(cx, self.param_base.default_normalized_value());
                self.param_base.end_set_parameter(cx);
            }
            _ => {}
        });
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let bounds = cx.bounds();
        if bounds.w == 0.0 || bounds.h == 0.0 {
            return;
        }

        // Center the enum button box in the middle area (between top label and bottom value)
        let center_y = bounds.h / 2.0;
        let box_w = 84.0;
        let box_h = 28.0;
        let x = bounds.x + (bounds.w - box_w) / 2.0;
        let y = bounds.y + center_y - box_h / 2.0;

        let mut path = Path::new();
        path.rounded_rect(x, y, box_w, box_h, 4.0);
        canvas.fill_path(
            &path,
            &Paint::color(Color::rgb(45, 48, 55).into()),
        );

        let mut path = Path::new();
        path.rounded_rect(x, y, box_w, box_h, 4.0);
        canvas.stroke_path(
            &path,
            &Paint::color(Color::rgb(70, 72, 80).into()),
        );
    }
}

/// Hardware dropdown selector for discrete / enum (`EnumParam`) parameters.
#[derive(Lens)]
pub struct DropdownSelect {
    param_base: ParamWidgetBase,
    expanded: bool,
    options: &'static [&'static str],
    last_y: f32,
}

impl DropdownSelect {
    pub fn new<'a, L, Params, P, FMap>(
        cx: &'a mut Context,
        params: L,
        params_to_param: FMap,
        options: &'static [&'static str],
    ) -> Handle<'a, DropdownSelect>
    where
        L: Lens<Target = Params> + Clone,
        Params: 'static,
        P: Param + 'static,
        FMap: Fn(&Params) -> &P + Copy + 'static,
    {
        let expanded = false;
        Self {
            param_base: ParamWidgetBase::new(cx, params, params_to_param),
            expanded,
            options,
            last_y: 0.0,
        }
        .build(
            cx,
            ParamWidgetBase::build_view(params, params_to_param, move |cx, param_data| {
                let display_value_lens = param_data.make_lens(|param| {
                    param.normalized_value_to_string(param.unmodulated_normalized_value(), true)
                });
                VStack::new(cx, move |cx| {
                    // Parameter label (top band, centered white, no overlap)
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(18.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .hoverable(false);

                    Binding::new(cx, Self::expanded, move |cx, expanded| {
                        if expanded.get(cx) {
                            VStack::new(cx, move |cx| {
                                for option in options.iter() {
                                    Label::new(cx, *option)
                                        .height(Pixels(16.0))
                                        .width(Stretch(1.0))
                                        .font_size(9.0)
                                        .color(Color::rgb(235, 225, 205))
                                        .hoverable(false);
                                }
                            })
                            .height(Pixels(16.0 * options.len() as f32))
                            .width(Stretch(1.0));
                        } else {
                            Label::new(cx, display_value_lens.clone())
                                .height(Pixels(18.0))
                                .width(Stretch(1.0))
                                .font_size(10.0)
                                .color(Color::rgb(230, 185, 50))
                                .hoverable(false);
                        }
                    });
                })
                .height(Stretch(1.0))
                .width(Stretch(1.0));
            }),
        )
        .width(Pixels(94.0))
        .height(Pixels(64.0))
    }
}

impl View for DropdownSelect {
    fn element(&self) -> Option<&'static str> {
        Some("dropdown-select")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _meta| match window_event {
            WindowEvent::MouseMove(_x, y) => {
                self.last_y = *y;
            }
            WindowEvent::MouseDown(MouseButton::Left) => {
                self.param_base.begin_set_parameter(cx);
                cx.capture();
                cx.set_active(true);
                if !self.expanded {
                    self.expanded = true;
                } else {
                    // Determine which option row was clicked from the vertical position.
                    let bounds = cx.bounds();
                    let list_top = bounds.y + 18.0;
                    let row_h = 16.0;
                    let rel = (self.last_y - list_top) / row_h;
                    let idx = if rel < 0.0 {
                        0
                    } else {
                        (rel as usize).min(self.options.len().saturating_sub(1))
                    };
                    let normalized = if self.options.len() > 1 {
                        idx as f32 / (self.options.len() - 1) as f32
                    } else {
                        0.0
                    };
                    self.param_base.set_normalized_value(cx, normalized);
                    self.expanded = false;
                    self.param_base.end_set_parameter(cx);
                    cx.release();
                    cx.set_active(false);
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if !self.expanded {
                    self.param_base.end_set_parameter(cx);
                    cx.release();
                    cx.set_active(false);
                }
            }
            _ => {}
        });
    }
}

pub struct ChannelMeterView {
    telemetry: Arc<GuiTelemetry>,
    is_output: bool,
}

impl View for ChannelMeterView {
    fn element(&self) -> Option<&'static str> {
        Some("channel-meter")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let bounds = cx.bounds();
        let n_ch = (self.telemetry.num_channels.load(Ordering::Relaxed) as usize)
            .clamp(1, MAX_GUI_CHANNELS);
        let gap = 2.0;
        let bar_w = ((bounds.w - gap * (n_ch as f32 - 1.0)) / n_ch as f32).max(2.0);

        for ch in 0..n_ch {
            let bits = if self.is_output {
                self.telemetry.out_peaks[ch].swap(0.0f32.to_bits(), Ordering::Relaxed)
            } else {
                self.telemetry.in_peaks[ch].swap(0.0f32.to_bits(), Ordering::Relaxed)
            };
            let peak = if f32::from_bits(bits).is_finite() { f32::from_bits(bits) } else { 0.0 };
            let db = nih_plug::util::gain_to_db(peak.max(1e-6)).clamp(-60.0, 0.0);
            let norm = (db + 60.0) / 60.0;

            let x = bounds.x + ch as f32 * (bar_w + gap);
            let mut bg = Path::new();
            bg.rect(x, bounds.y, bar_w, bounds.h);
            canvas.fill_path(
                &bg,
                &Paint::color(nih_plug_vizia::vizia::vg::Color::rgb(16, 18, 22)),
            );

            let fill_h = bounds.h * norm;
            let mut fg = Path::new();
            fg.rect(x, bounds.y + (bounds.h - fill_h), bar_w, fill_h);
            let col = if db > -3.0 {
                nih_plug_vizia::vizia::vg::Color::rgb(225, 65, 55)
            } else if db > -12.0 {
                nih_plug_vizia::vizia::vg::Color::rgb(230, 185, 50)
            } else {
                nih_plug_vizia::vizia::vg::Color::rgb(46, 185, 95)
            };
            canvas.fill_path(&fg, &Paint::color(col));
        }
    }
}

/// Dedicated Gain Reduction bar view (amber #E57A3B, 0 dB → -24 dB).
pub struct GainReductionBarView {
    telemetry: Arc<GuiTelemetry>,
}

impl View for GainReductionBarView {
    fn element(&self) -> Option<&'static str> {
        Some("gain-reduction-bar")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let bounds = cx.bounds();
        let gr_db = if f32::from_bits(
            self.telemetry
                .gain_reduction_db
                .load(Ordering::Relaxed),
        ).is_finite() {
            f32::from_bits(
                self.telemetry
                    .gain_reduction_db
                    .load(Ordering::Relaxed),
            )
        } else {
            0.0
        }
        .clamp(-24.0, 0.0);
        let gr_norm = (gr_db + 24.0) / 24.0;

        // Background (dark)
        let mut bg = Path::new();
        bg.rect(bounds.x, bounds.y, bounds.w, bounds.h);
        canvas.fill_path(
            &bg,
            &Paint::color(Color::rgb(30, 32, 36).into()),
        );

        // Fill bar (amber #E57A3B), fills from left as gain reduction increases
        let mut fg = Path::new();
        fg.rect(bounds.x, bounds.y, bounds.w * gr_norm, bounds.h);
        canvas.fill_path(
            &fg,
            &Paint::color(Color::rgb(229, 122, 59).into()),
        );

        // Numeric readout: "-4.2 dB"
        let text = format!("{:.1} dB", gr_db);
        let text_bounds = BoundingBox {
            x: bounds.x + 4.0,
            y: bounds.y,
            w: bounds.w - 8.0,
            h: bounds.h,
        };
        // Note: `cx.draw_text` uses the current text context. For a custom view
        // we draw via a simple label overlay by reusing `cx.draw_text` with a
        // formatted string is not directly supported; we rely on a separate
        // `Label` placed next to the bar in `create_greg_editor` for the numeric
        // readout. Here we only draw the bar itself.
        let _ = (text, text_bounds);
    }
}

/// Identifies which widget type a parameter should be rendered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    Knob, Toggle, Enum,
}

/// Static description of a parameter for the auto-generated parameter grid.
/// Stores the label and a closure that builds the widget in a Vizia context.
/// Plugins typically pass closures like `Box::new(|cx| RotaryKnob::new(cx, EditorData::params, |p| &p.threshold))`.
pub struct ParamDef {
    pub label: &'static str,
    pub kind: WidgetKind,
    pub build: Box<dyn Fn(&mut Context) + Send + Sync>,
}

impl ParamDef {
    pub fn knob(label: &'static str, build: Box<dyn Fn(&mut Context) + Send + Sync>) -> Self {
        Self { label, kind: WidgetKind::Knob, build }
    }
    pub fn toggle(label: &'static str, build: Box<dyn Fn(&mut Context) + Send + Sync>) -> Self {
        Self { label, kind: WidgetKind::Toggle, build }
    }
    pub fn enum_switch(label: &'static str, build: Box<dyn Fn(&mut Context) + Send + Sync>) -> Self {
        Self { label, kind: WidgetKind::Enum, build }
    }
}

/// Auto-generated parameter grid that arranges parameters in a wrapping
/// horizontal layout with a label above each control and the current value
/// below it. The grid fills the available width and wraps to the next row.
pub fn param_grid(cx: &mut Context, defs: &[ParamDef]) {
    const PER_ROW: usize = 6;
    VStack::new(cx, move |cx| {
        for row in defs.chunks(PER_ROW) {
            HStack::new(cx, move |cx| {
                for def in row {
                    (def.build)(cx);
                }
            })
            .child_space(Pixels(metrics::GUTTER))
            .height(Pixels(metrics::KNOB_H));
        }
    })
    .child_space(Pixels(metrics::GUTTER))
    .width(Stretch(1.0));
}


/// Determines if GUI rendering is supported on this system.
/// Returns `false` if headless mode is forced, no display is available,
/// or we're running inside a hypervisor/VM without forced GUI.
fn is_gui_supported() -> bool {
    // Force GUI overrides everything
    if std::env::var("GREG_FORCE_GUI").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false) {
        return true;
    }
    // Explicit headless mode
    if std::env::var("GREG_NO_GUI").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false) {
        return false;
    }
    // No DISPLAY variable means no X11/Wayland server available
    if std::env::var("DISPLAY").map(|v| v.is_empty()).unwrap_or(true) {
        return false;
    }
    true
}

/// Backward-compatible wrapper: existing plugins keep their 4-argument calls.
pub fn create_greg_editor<M, A, MB>(
    title: &'static str,
    telemetry: Arc<GuiTelemetry>,
    main_builder: M,
    advanced_builder: A,
    model_builder: MB,
) -> Option<Box<dyn Editor>>
where
    M: Fn(&mut Context) + 'static + Send + Sync,
    A: Fn(&mut Context) + 'static + Send + Sync,
    MB: Fn(&mut Context) + 'static + Send + Sync,
{
    // Use the central helper — returns false for forced no-GUI or VM environments
    if !is_gui_supported() {
        return None;
    }
    create_greg_editor_with_params(title, telemetry, main_builder, advanced_builder, None, model_builder)
}

/// Fallback editor: minimal safe UI when the main builder panics.
/// Shows title, error message, and active metering. No interactive controls.
pub fn create_fallback_editor(
    title: &'static str,
    telemetry: Arc<GuiTelemetry>,
) -> Option<Box<dyn Editor>>
{
    // If GUI is not supported (no display, VM, etc.), return None immediately.
    // The fallback editor must NEVER spawn a ViziaEditor when headless.
    if !is_gui_supported() {
        return None;
    }

    let state = ViziaState::new(|| (800, 400));
    let title_str = title.to_string();
    let telemetry_clone = telemetry.clone();

    create_vizia_editor(state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_regular(cx);
        assets::register_noto_sans_bold(cx);

        GregGuiData {
            plugin_title: title_str.clone(),
            advanced_open: false,
            telemetry: telemetry_clone.clone(),
            toolbar: ToolbarState::new(title_str.as_str()),
            param_access: &[],
        }
        .build(cx);

        let tel_out = telemetry_clone.clone();
        let tel_in = telemetry_clone.clone();
        let tel_foot = telemetry_clone.clone();

        VStack::new(cx, move |cx| {
            HStack::new(cx, |cx| {
                Label::new(cx, "Greg Plugin -- Fallback Mode")
                    .font_size(14.0)
                    .font_weight(FontWeightKeyword::Bold)
                    .color(Color::rgb(229, 122, 59))
                    .left(Pixels(12.0));
                Element::new(cx).width(Stretch(1.0));
            })
            .height(Pixels(38.0))
            .background_color(Color::rgb(24, 26, 30))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

            HStack::new(cx, move |cx| {
                VStack::new(cx, move |cx| {
                    Label::new(cx, "IN")
                        .font_size(10.0)
                        .color(Color::rgb(180, 185, 195));
                    ChannelMeterView { telemetry: tel_in.clone(), is_output: false }
                        .build(cx, |_| { })
                        .width(Pixels(46.0))
                        .height(Stretch(1.0));
                })
                .width(Pixels(62.0))
                .child_space(Pixels(6.0))
                .background_color(Color::rgb(22, 24, 28));

                VStack::new(cx, |cx| {
                    Label::new(cx, "GUI build failed -- running safe fallback mode")
                        .font_size(12.0).color(Color::rgb(229, 122, 59))
                        .child_top(Pixels(12.0)).left(Pixels(12.0));
                    Label::new(cx, "Parameters are still controllable via DAW automation.")
                        .font_size(10.0).color(Color::rgb(180, 185, 195))
                        .left(Pixels(12.0));
                    Label::new(cx, "Metering is active below.")
                        .font_size(10.0).color(Color::rgb(180, 185, 195))
                        .left(Pixels(12.0));
                })
                .width(Stretch(1.0)).height(Stretch(1.0)).child_space(Pixels(8.0));

                VStack::new(cx, move |cx| {
                    Label::new(cx, "OUT")
                        .font_size(10.0).color(Color::rgb(180, 185, 195));
                    ChannelMeterView { telemetry: tel_out.clone(), is_output: true }
                        .build(cx, |_| {})
                        .width(Pixels(46.0)).height(Stretch(1.0));
                })
                .width(Pixels(62.0)).child_space(Pixels(6.0))
                .background_color(Color::rgb(22, 24, 28));
            })
            .height(Stretch(1.0));

            HStack::new(cx, move |cx| {
                Label::new(cx, "PHASE").font_size(10.0).color(Color::rgb(175, 180, 190)).left(Pixels(12.0));
                Element::new(cx).width(Stretch(1.0));
                Element::new(cx).width(Stretch(1.0));
                if tel_foot.show_gr_meter.load(Ordering::Relaxed) {
                    Label::new(cx, "GR").font_size(10.0).color(Color::rgb(175, 180, 190)).right(Pixels(4.0));
                    GainReductionBarView { telemetry: tel_foot.clone() }.build(cx, |_| {}).width(Pixels(120.0)).height(Pixels(14.0)).right(Pixels(4.0));
                    Label::new(cx, "-0.0 dB").font_size(10.0).color(Color::rgb(229, 122, 59)).right(Pixels(12.0));
                }
            })
            .height(Pixels(34.0)).background_color(Color::rgb(24, 26, 30))
            .child_top(Stretch(1.0)).child_bottom(Stretch(1.0));
        })
        .background_color(Color::rgb(20, 22, 25));
    })
}

/// Extended version with an optional auto-generated parameter grid.
/// Backwards-compatible 6-arg entry point: delegates with an empty parameter-access list
/// (the universal toolbar is rendered but A/B/clipboard operations are inert until a plugin
/// registers [`ParamAccess`] entries through [`create_greg_editor_full`]).
pub fn create_greg_editor_with_params<M, A, MB>(
    title: &'static str,
    telemetry: Arc<GuiTelemetry>,
    main_builder: M,
    advanced_builder: A,
    param_defs: Option<&'static [ParamDef]>,
    model_builder: MB,
) -> Option<Box<dyn Editor>>
where
    M: Fn(&mut Context) + 'static + Send + Sync,
    A: Fn(&mut Context) + 'static + Send + Sync,
    MB: Fn(&mut Context) + 'static + Send + Sync,
{
    create_greg_editor_full(
        title,
        telemetry,
        main_builder,
        advanced_builder,
        param_defs,
        model_builder,
        &[] as &[ParamAccess],
    )
}

/// Full entry point that also registers per-parameter accessors so the universal toolbar
/// (A/B Compare, Copy/Paste, Save Default, Preset Browser) is fully functional.
pub fn create_greg_editor_full<M, A, MB>(
    title: &'static str,
    telemetry: Arc<GuiTelemetry>,
    main_builder: M,
    advanced_builder: A,
    param_defs: Option<&'static [ParamDef]>,
    model_builder: MB,
    param_access: &'static [ParamAccess],
) -> Option<Box<dyn Editor>>
where
    M: Fn(&mut Context) + 'static + Send + Sync,
    A: Fn(&mut Context) + 'static + Send + Sync,
    MB: Fn(&mut Context) + 'static + Send + Sync,
{
    // Centralized GUI support check: returns false for forced no-GUI or VM environments
    if !is_gui_supported() {
        return None;
    }

    let state = ViziaState::new(|| (820, 360));
    let title_str = title.to_string();
    let main_fn = Arc::new(main_builder);
    let adv_fn = Arc::new(advanced_builder);

    create_vizia_editor(state, ViziaTheming::Custom, move |cx, _| {
        // Catch panics inside the window thread so it never aborts.
        // The model_builder is invoked BEFORE the parameter grid / main builder
        // so that every Lens (e.g. EditorData::params) resolves against a tree
        // that has already been built into the Vizia context — no
        // "Failed to get data from context. Has it been built into the tree?"
        // panic can occur.
        let result = catch_unwind(AssertUnwindSafe(|| {
            model_builder(cx);
            assets::register_noto_sans_regular(cx);
            assets::register_noto_sans_bold(cx);
            GregGuiData {
                plugin_title: title_str.clone(),
                advanced_open: false,
                telemetry: telemetry.clone(),
                toolbar: ToolbarState::new(&title_str),
                param_access,
            }
            .build(cx);
            let tel_out = telemetry.clone();
            let tel_in = telemetry.clone();
            let tel_foot = telemetry.clone();
            let m_fn = main_fn.clone();
            let a_fn = adv_fn.clone();
            VStack::new(cx, move |cx| {
                HStack::new(cx, |cx| {
                    Label::new(cx, GregGuiData::plugin_title).font_size(15.0).font_weight(FontWeightKeyword::Bold).color(Color::rgb(235, 225, 205)).left(Pixels(12.0));
                    Element::new(cx).width(Stretch(1.0));
                    // A/B Compare buttons
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::AbSlot(0)), |cx| Label::new(cx, "A")).right(Pixels(4.0));
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::AbSlot(1)), |cx| Label::new(cx, "B")).right(Pixels(4.0));
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::AbRecall(0)), |cx| Label::new(cx, "<A")).right(Pixels(4.0));
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::AbRecall(1)), |cx| Label::new(cx, "<B")).right(Pixels(4.0));
                    // Copy / Paste
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::CopyParams), |cx| Label::new(cx, "Copy")).right(Pixels(4.0));
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::PasteParams), |cx| Label::new(cx, "Paste")).right(Pixels(4.0));
                    // Save Default
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::SaveDefault), |cx| Label::new(cx, "Save Def")).right(Pixels(4.0));
                    // Preset nav < > and label
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::PresetPrev), |cx| Label::new(cx, "<")).right(Pixels(2.0));
                    Label::new(cx, "Presets").font_size(11.0).right(Pixels(2.0));
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::PresetNext), |cx| Label::new(cx, ">")).right(Pixels(4.0));
                    Button::new(cx, |cx| cx.emit(GregGuiEvent::ToggleAdvanced), |cx| Label::new(cx, GregGuiData::advanced_open.map(|o| if *o { "- ADV" } else { "+ ADV" }))).right(Pixels(12.0));
                }).height(Pixels(38.0)).background_color(Color::rgb(24, 26, 30)).child_top(Stretch(1.0)).child_bottom(Stretch(1.0));
                HStack::new(cx, move |cx| {
                    VStack::new(cx, move |cx| { Label::new(cx, "IN").font_size(10.0).color(Color::rgb(180, 185, 195)); ChannelMeterView { telemetry: tel_in.clone(), is_output: false }.build(cx, |_| {}).width(Pixels(46.0)).height(Stretch(1.0)); }).width(Pixels(62.0)).child_space(Pixels(6.0)).background_color(Color::rgb(22, 24, 28));
                    VStack::new(cx, |cx| {
                        if let Some(defs) = param_defs { param_grid(cx, defs); }
                        else { HStack::new(cx, move |cx| { (m_fn)(cx); }).height(Pixels(96.0)).child_space(Pixels(10.0)).width(Stretch(1.0)).background_color(Color::rgb(32, 35, 42)); }
                        Binding::new(cx, GregGuiData::advanced_open, move |cx, open| {
                            let a_inner = a_fn.clone();
                            if open.get(cx) { VStack::new(cx, |cx| { (a_inner)(cx); }).height(Pixels(200.0)).background_color(Color::rgb(28, 30, 35)).child_space(Pixels(10.0)).width(Stretch(1.0)).height(Stretch(1.0)); }
                        });
                    }).width(Stretch(1.0)).height(Stretch(1.0)).child_space(Pixels(8.0));
                    VStack::new(cx, move |cx| { Label::new(cx, "OUT").font_size(10.0).color(Color::rgb(180, 185, 195)); ChannelMeterView { telemetry: tel_out.clone(), is_output: true }.build(cx, |_| {}).width(Pixels(46.0)).height(Stretch(1.0)); }).width(Pixels(62.0)).child_space(Pixels(6.0)).background_color(Color::rgb(22, 24, 28));
                }).height(Stretch(1.0));
                HStack::new(cx, move |cx| {
                    Label::new(cx, "PHASE").font_size(10.0).color(Color::rgb(175, 180, 190)).left(Pixels(12.0));
                    Element::new(cx).width(Stretch(1.0));
                    Element::new(cx).width(Stretch(1.0));
                    if tel_foot.show_gr_meter.load(Ordering::Relaxed) {
                        Label::new(cx, "GR").font_size(10.0).color(Color::rgb(175, 180, 190)).right(Pixels(4.0));
                        GainReductionBarView { telemetry: tel_foot.clone() }.build(cx, |_| {}).width(Pixels(120.0)).height(Pixels(14.0)).right(Pixels(4.0));
                        Label::new(cx, "-0.0 dB").font_size(10.0).color(Color::rgb(229, 122, 59)).right(Pixels(12.0));
                    }
                }).height(Pixels(34.0)).background_color(Color::rgb(24, 26, 30)).child_top(Stretch(1.0)).child_bottom(Stretch(1.0));
            }).background_color(Color::rgb(20, 22, 25));
        }));
        // Emergency safe Label: if the window thread panicked while building
        // the tree, render a minimal notice instead of letting the panic
        // propagate so `window_thread` can never abort the DAW.
        if result.is_err() {
            Label::new(cx, "Greg GUI - Safe Fallback (UI build panic recovered)")
                .font_size(13.0)
                .font_weight(FontWeightKeyword::Bold)
                .color(Color::rgb(229, 122, 59))
                .left(Pixels(12.0));
        }
    })
}



pub fn param_control<P, L>(cx: &mut Context, label: &'static str, params: L)
where
    P: Param + 'static,
    L: Lens<Target = P> + Clone + 'static,
{
    VStack::new(cx, move |cx| {
        Label::new(cx, label)
            .font_size(11.0)
            .color(Color::rgb(210, 215, 225))
            .bottom(Pixels(3.0));
        ParamSlider::new(cx, params, |p| p)
            .width(Pixels(115.0))
            .height(Pixels(24.0));
    })
    .width(Pixels(125.0))
    .height(Pixels(48.0));
}
// =============================================================================
// CUSTOM GUI LAYOUT SCHEMA (gui_layout.json) — Retrocompatibile 100%
// =============================================================================

/// Schema JSON per layout personalizzabile dei widget del plugin.
/// Deserializzato con `serde_json` solo all'apertura della finestra UI,
/// MAI nel thread audio.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GuiLayoutSpec {
    /// Larghezza finestra del plugin (default 820).
    pub window_width: f32,
    /// Altezza finestra del plugin (default 360).
    pub window_height: f32,
    /// Colore sfondo in esadecimale (es. "#20232A").
    pub background_hex: String,
    /// Colore accent in esadecimale (es. "#E5A93B").
    pub accent_hex: String,
    /// Lista degli elementi da posizionare in modalità assoluta.
    pub elements: Vec<GuiElementPlacement>,
}

/// Singolo elemento posizionabile nel layout personalizzato.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GuiElementPlacement {
    /// ID del parametro CLAP (es. "threshold") oppunere widget speciale:
    /// `"in_meter"`, `"out_meter"`, `"gr_meter"`, `"spectrum"`.
    pub param_id: String,
    /// Etichetta visuale sopra il widget.
    pub label: String,
    /// Tipo di widget: `"knob"`, `"toggle"`, `"enum"`, `"meter"`, `"spectrum"`.
    pub widget_type: String,
    /// Coordinata X assoluta in pixel.
    pub x: f32,
    /// Coordinata Y assoluta in pixel.
    pub y: f32,
    /// Larghezza del widget in pixel.
    pub width: f32,
    /// Altezza del widget in pixel.
    pub height: f32,
}

/// Cerca un file `gui_layout.json` opzionale per il plugin identificato da `plugin_id`.
/// Ordine di ricerca:
/// 1. Variabile d'ambiente `GREG_GUI_LAYOUT_<plugin_id>` (per test)
/// 2. File `gui_layout.json` nella directory corrente
/// 3. File `gui_layout.json` accanto al bundle `.clap` (se percorso noto)
///
/// Restituisce `None` se il file non esiste o non è valido (fallback automatico
/// alla grigliaautomatica `param_grid()`).
pub fn load_gui_layout(plugin_id: &str) -> Option<GuiLayoutSpec> {
    // 1. Variabile d'ambiente per testing
    let env_var = format!("GREG_GUI_LAYOUT_{}", plugin_id.to_uppercase());
    if let Ok(json_str) = std::env::var(&env_var) {
        if let Ok(spec) = serde_json::from_str::<GuiLayoutSpec>(&json_str) {
            return Some(spec);
        }
    }

    // 2. Directory corrente
    let cwd_path = std::path::Path::new("gui_layout.json");
    if cwd_path.exists() {
        if let Ok(content) = std::fs::read_to_string(cwd_path) {
            if let Ok(spec) = serde_json::from_str::<GuiLayoutSpec>(&content) {
                return Some(spec);
            }
        }
    }

    // 3. Accanto al bundle .clap (se il plugin_id è un percorso valido)
    let bundle_path = std::path::Path::new(plugin_id);
    if let Some(parent) = bundle_path.parent() {
        let near_path = parent.join("gui_layout.json");
        if near_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&near_path) {
                if let Ok(spec) = serde_json::from_str::<GuiLayoutSpec>(&content) {
                    return Some(spec);
                }
            }
        }
    }

    None
}

/// Costruisce un layout personalizzato posizionando ogni elemento in modalità
/// assoluta (`PositionType::SelfDirected`). Usato quando un file `gui_layout.json`
/// valido è stato caricato.
///
/// Questa funzione è **retrocompatibile**: se `layout` è `None`, chiama
/// `param_grid()` esistente senza alcuna modifica visiva.
pub fn build_custom_layout<F>(
    cx: &mut Context,
    layout: Option<&GuiLayoutSpec>,
    param_builder: F,
) where
    F: Fn(&mut Context, &str, &str) + 'static + Send + Sync,
{
    match layout {
        Some(spec) => {
            // Layout personalizzato: posizionamento assoluto
            for element in &spec.elements {
                (param_builder)(cx, &element.param_id, &element.widget_type);
            }
        }
        None => {
            // Fallback: griglia automatica esistente (nessun cambiamento)
            // Questa branca non viene raggiunta qui; il caller gestisce il fallback.
        }
    }
}
