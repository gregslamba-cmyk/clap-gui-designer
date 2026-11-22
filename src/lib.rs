//! `greg_gui_core` - Libreria GUI comune leggera e multipiattaforma (`nih_plug_vizia` + FemtoVG)
//! Implementa il layout standardizzato di `GUI_SPEC.md` con zero allocazioni sul thread DSP.

use nih_plug::prelude::*;
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::vg::{Paint, Path};
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Re-export vizia types for downstream plugins to avoid version conflicts.
pub mod prelude {
    pub use nih_plug_vizia::vizia::prelude::*;
    pub use nih_plug_vizia::widgets::ParamSlider;
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

/// View vettoriale FemtoVG per i meter multicanale OUT (SX) e IN (DX).
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

            // CORPO CENTRALE: OUT (SX) | MAIN + ADV | IN (DX)
            HStack::new(cx, move |cx| {
                // OUT METER (SX)
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

                // AREA CENTRALE PERSISTENTE
                VStack::new(cx, |cx| {
                    VStack::new(cx, |cx| {
                        (m_fn)(cx);
                    })
                    .height(Pixels(268.0))
                    .background_color(Color::rgb(34, 37, 42))
                    .child_space(Pixels(10.0));

                    Binding::new(cx, GregGuiData::advanced_open, move |cx, open| {
                        let a_inner = a_fn.clone();
                        if open.get(cx) {
                            VStack::new(cx, |cx| {
                                (a_inner)(cx);
                            })
                            .height(Pixels(200.0))
                            .background_color(Color::rgb(28, 30, 35))
                            .child_space(Pixels(10.0));
                        }
                    });
                })
                .width(Stretch(1.0));

                // IN METER (DX)
                VStack::new(cx, move |cx| {
                    Label::new(cx, "IN")
                        .font_size(10.0)
                        .color(Color::rgb(180, 185, 195));
                    ChannelMeterView {
                        telemetry: tel_in.clone(),
                        is_output: false,
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
                    Label::new(cx, "GR (0 -> -30 dB)")
                        .font_size(10.0)
                        .color(Color::rgb(230, 165, 65))
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

/// Helper standard per aggiungere un parametro con `ParamSlider` di `nih_plug_vizia`.
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
