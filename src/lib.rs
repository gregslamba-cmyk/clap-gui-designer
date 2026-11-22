//! `greg_gui_core` - Libreria GUI comune leggera e multipiattaforma (`nih_plug_vizia` + FemtoVG)
//! Implementa il layout standardizzato di `GUI_SPEC.md` con zero allocazioni sul thread DSP.

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

/// Re-export vizia types for downstream plugins to avoid version conflicts.
pub mod prelude {
    pub use nih_plug_vizia::vizia::prelude::*;
    pub use crate::RotaryKnob;
    pub use crate::ToggleSwitch;
    pub use crate::EnumSwitch;
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

#[derive(Lens)]
pub struct GregGuiData {
    pub plugin_title: String,
    pub advanced_open: bool,
    pub telemetry: Arc<GuiTelemetry>,
}

#[derive(Debug, Clone)]
pub enum GregGuiEvent {
    ToggleAdvanced,
    ResetClips,
}

impl Model for GregGuiData {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|msg, _| match msg {
            GregGuiEvent::ToggleAdvanced => {
                self.advanced_open = !self.advanced_open;
                let h = if self.advanced_open { 560 } else { 360 };
                cx.emit(WindowEvent::SetSize(WindowSize {
                    width: 820,
                    height: h,
                }));
            }
            GregGuiEvent::ResetClips => {
                for ch in 0..MAX_GUI_CHANNELS {
                    self.telemetry.in_clip[ch].store(false, Ordering::Relaxed);
                    self.telemetry.out_clip[ch].store(false, Ordering::Relaxed);
                }
            }
        });
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
        }
        .build(
            cx,
            ParamWidgetBase::build_view(params, params_to_param, move |cx, param_data| {
                let display_value_lens = param_data.make_lens(|param| {
                    param.normalized_value_to_string(param.unmodulated_normalized_value(), true)
                });

                ZStack::new(cx, |cx| {
                    // Parameter label (top)
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(12.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .child_top(Pixels(4.0))
                        .hoverable(false);

                    // Value display (bottom)
                    Label::new(cx, display_value_lens)
                        .height(Pixels(14.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(230, 185, 50))
                        .child_bottom(Pixels(4.0))
                        .hoverable(false);
                })
                .height(Stretch(1.0))
                .width(Stretch(1.0));
            }),
        )
        .width(Pixels(80.0))
        .height(Pixels(88.0))
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
                    let bounds = cx.bounds();
                    let center_y = bounds.h / 2.0;
                    let rel_y = center_y - y;
                    let norm = (rel_y / (bounds.h / 2.0)).clamp(-1.0, 1.0);
                    self.param_base.set_normalized_value(cx, norm);
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

        let center_x = bounds.w / 2.0;
        // Center knob in the middle area (between top label and bottom value)
        let center_y = bounds.h * 0.5;
        let knob_radius = 24.0; // 48px diameter
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
            &Paint::color(Color::rgb(30, 32, 36).into()),
        );

        // Outer bevel ring (border #5A6270)
        let mut path = Path::new();
        path.arc(center_x, center_y, knob_radius, start_angle, end_angle, Solidity::Solid);
        canvas.stroke_path(
            &path,
            &Paint::color(Color::rgb(90, 98, 112).into()),
        );

        // Ambra progression arc (#E5A93B)
        let mut path = Path::new();
        path.arc(center_x, center_y, arc_radius, start_angle, angle, Solidity::Solid);
        canvas.stroke_path(
            &path,
            &Paint::color(Color::rgb(229, 169, 59).into()),
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
            &Paint::color(Color::rgb(255, 255, 255).into()),
        );
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

                ZStack::new(cx, |cx| {
                    // Parameter label (top)
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(12.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .child_top(Pixels(4.0))
                        .hoverable(false);

                    // State label (bottom): ON or OFF
                    Label::new(cx, display_value_lens)
                        .height(Pixels(14.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(230, 185, 50))
                        .child_bottom(Pixels(4.0))
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

                ZStack::new(cx, |cx| {
                    // Parameter label (top)
                    Label::new(cx, param_data.param().name())
                        .height(Pixels(12.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(210, 215, 225))
                        .child_top(Pixels(4.0))
                        .hoverable(false);

                    // Active value text (bottom)
                    Label::new(cx, display_value_lens)
                        .height(Pixels(14.0))
                        .width(Stretch(1.0))
                        .font_size(10.0)
                        .color(Color::rgb(230, 185, 50))
                        .child_bottom(Pixels(4.0))
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
            let peak = f32::from_bits(bits);
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
        let gr_db = f32::from_bits(
            self.telemetry
                .gain_reduction_db
                .load(Ordering::Relaxed),
        )
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

/// Costruisce l'editor Vizia standardizzato per tutti i plugin della suite.
pub fn create_greg_editor<M, A>(
    title: &'static str,
    telemetry: Arc<GuiTelemetry>,
    main_builder: M,
    advanced_builder: A,
) -> Option<Box<dyn Editor>>
where
    M: Fn(&mut Context) + 'static + Send + Sync,
    A: Fn(&mut Context) + 'static + Send + Sync,
{
    let state = ViziaState::new(|| (820, 360));
    let title_str = title.to_string();
    let main_fn = Arc::new(main_builder);
    let adv_fn = Arc::new(advanced_builder);

    create_vizia_editor(state, ViziaTheming::Custom, move |cx, _| {
        assets::register_noto_sans_regular(cx);
        assets::register_noto_sans_bold(cx);

        GregGuiData {
            plugin_title: title_str.clone(),
            advanced_open: false,
            telemetry: telemetry.clone(),
        }
        .build(cx);

        let tel_out = telemetry.clone();
        let tel_in = telemetry.clone();
        let tel_foot = telemetry.clone();
        let m_fn = main_fn.clone();
        let a_fn = adv_fn.clone();

        VStack::new(cx, move |cx| {
            // HEADER
            HStack::new(cx, |cx| {
                Label::new(cx, GregGuiData::plugin_title)
                    .font_size(15.0)
                    .font_weight(FontWeightKeyword::Bold)
                    .color(Color::rgb(235, 225, 205))
                    .left(Pixels(12.0));
                Element::new(cx).width(Stretch(1.0));
                Button::new(
                    cx,
                    |cx| cx.emit(GregGuiEvent::ToggleAdvanced),
                    |cx| {
                        Label::new(
                            cx,
                            GregGuiData::advanced_open.map(|o| if *o { "- ADV" } else { "+ ADV" }),
                        )
                    },
                )
                .right(Pixels(12.0));
            })
            .height(Pixels(38.0))
            .background_color(Color::rgb(24, 26, 30))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));

            // CORPO CENTRALE: IN (SX) | MAIN + ADV | OUT (DX)
            HStack::new(cx, move |cx| {
                // IN METER (SX)
                VStack::new(cx, move |cx| {
                    Label::new(cx, "IN")
                        .font_size(10.0)
                        .color(Color::rgb(180, 185, 195));
                    ChannelMeterView {
                        telemetry: tel_in.clone(),
                        is_output: false,
                    }
                    .build(cx, |_| { })
                    .width(Pixels(46.0))
                    .height(Stretch(1.0));
                })
                .width(Pixels(62.0))
                .child_space(Pixels(6.0))
                .background_color(Color::rgb(22, 24, 28));

                // AREA CENTRALE PERSISTENTE
                VStack::new(cx, |cx| {
                    HStack::new(cx, move |cx| {
                        (m_fn)(cx);
                    })
                    .height(Pixels(96.0))
                    .child_space(Pixels(10.0))
                    .width(Stretch(1.0))
                    .background_color(Color::rgb(34, 37, 42));

                    Binding::new(cx, GregGuiData::advanced_open, move |cx, open| {
                        let a_inner = a_fn.clone();
                        if open.get(cx) {
                            VStack::new(cx, |cx| {
                                (a_inner)(cx);
                            })
                            .height(Pixels(200.0))
                            .background_color(Color::rgb(28, 30, 35))
                            .child_space(Pixels(10.0))
                            .width(Stretch(1.0))
                            .height(Stretch(1.0));
                        }
                    });
                })
                .width(Stretch(1.0))
                .height(Stretch(1.0));

                // OUT METER (DX)
                VStack::new(cx, move |cx| {
                    Label::new(cx, "OUT")
                        .font_size(10.0)
                        .color(Color::rgb(180, 185, 195));
                    ChannelMeterView {
                        telemetry: tel_out.clone(),
                        is_output: true,
                    }
                    .build(cx, |_| {})
                    .width(Pixels(46.0))
                    .height(Stretch(1.0));
                })
                .width(Pixels(62.0))
                .child_space(Pixels(6.0))
                .background_color(Color::rgb(22, 24, 28));
            })
            .height(Stretch(1.0));

            // FOOTER: PHASE & GAIN REDUCTION
            HStack::new(cx, move |cx| {
                Label::new(cx, "PHASE [-1..+1]")
                    .font_size(10.0)
                    .color(Color::rgb(175, 180, 190))
                    .left(Pixels(12.0));
                Element::new(cx).width(Stretch(1.0));
                if tel_foot.show_gr_meter.load(Ordering::Relaxed) {
                    Label::new(cx, "GR -0.0 dB")
                        .font_size(10.0)
                        .color(Color::rgb(229, 122, 59))
                        .right(Pixels(12.0));
                }
            })
            .height(Pixels(34.0))
            .background_color(Color::rgb(24, 26, 30))
            .child_top(Stretch(1.0))
            .child_bottom(Stretch(1.0));
        })
        .background_color(Color::rgb(20, 22, 25));
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
