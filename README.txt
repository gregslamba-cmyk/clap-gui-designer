================================================================================
GREG_GUI_CORE — MOTORE GRAFICO 2D VETTORIALE & TELEMETRIA LOCK-FREE
================================================================================

greg_gui_core e' la libreria grafica comune dell'ecosistema Analog-Vibe.
Costruita sopra nih_plug_vizia e FemtoVG, implementa un'interfaccia utente 2D
vettoriale ad alte prestazioni, con telemetria lock-free tra audio e grafica,
fallback automatico per sistemi headless e supporto completo al layout
personalizzabile tramite file JSON (gui_layout.json).


--------------------------------------------------------------------------------
1. ARCHITETTURA GRAFICA & STANDARD DI LAYOUT
--------------------------------------------------------------------------------

Il layout visivo dei plugin segue un design modulare a due sezioni:

1. Pannello Principale (Main Panel):
   Contiene i controlli primari dell'unita' hardware modellata (Threshold, Ratio,
   Attack, Release, Gain, Frequenze di taglio, ecc.).
   I controlli sono renderizzati tramite widget vettoriali dedicati:
   - RotaryKnob: manopola rotativa (48x48px con disco metallico, arco ambra da
     -135° a +135°, tacca bianca e visualizzazione numerica formattata).
   - ToggleSwitch: interruttore a levetta a 2 posizioni (Bypass, Stereo Link).
   - EnumSwitch: selettore a scatti a piu' posizioni (Ratio, Revisione, Modo).

2. Pannello Avanzato a Scomparsa (+ ADV):
   Pannello espandibile (altezza finestra passa da 380px a 560px) che racchiude
   i parametri fisici del circuito:
   - Transformer / Iron %: saturazione del nucleo magnetico del trasformatore.
   - Analog Drift: deriva termica asincrona dei componenti elettronici.
   - PSU Sag / Overdrive: compressione dinamica dell'alimentazione.

3. Sezione Metering e Monitoraggio:
   Meter multicanale di picco Input e Output (fino a 16 canali visualizzabili),
   indicatori di Clip con memoria, barra di Gain Reduction (GR) in dB negativi
   e indicatore di correlazione di fase [-1.0 .. +1.0].


--------------------------------------------------------------------------------
2. TELEMETRIA AUDIO -> UI LOCK-FREE (GuiTelemetry)
--------------------------------------------------------------------------------

La telemetria garantisce la totale separazione tra il thread di calcolo audio
in tempo reale e il thread di rendering grafico (60 FPS):

- Struttura GuiTelemetry:
  * num_channels: AtomicU32 (canali attivi negoziati con l'host, da 1 a 8).
  * in_peaks / out_peaks: [AtomicU32; MAX_GUI_CHANNELS] contenenti i valori
    lineari di picco codificati via f32::to_bits().
  * in_clip / out_clip: [AtomicBool; MAX_GUI_CHANNELS] per il rilevamento di clip.
  * gain_reduction_db: AtomicU32 per la compressione istantanea in dB negativi.
  * phase_correlation: AtomicU32 per la correlazione di fase stereo.
  * show_gr_meter: AtomicBool per visualizzare/nascondere la barra GR.

- Garanzie Real-Time:
  Tutte le scritture nel thread audio process() avvengono con Ordering::Relaxed.
  Nessuna allocazione heap, nessun blocco di mutex e zero contesa di cache line.


--------------------------------------------------------------------------------
3. FALLBACK AUTOMATICO (GREG_NO_GUI=1 & HEADLESS)
--------------------------------------------------------------------------------

Per consentire l'utilizzo dei plugin in ambienti server, render farm, test
automatizzati o su sistemi privi di server grafico X11/OpenGL funzionante:

1. Variabile d'ambiente GREG_NO_GUI=1:
   Se questa variabile e' impostata nell'ambiente di esecuzione, il metodo
   editor() del plugin restituisce None, forzando la DAW a esporre
   immediatamente l'interfaccia a parametri generici nativa dell'host.
2. Gestione Robusta dell'Inizializzazione Finestra:
   Se la connessione al display server (XCB/Wayland) fallisce, il plugin non
   va in crash e degrada elegantemente alla modalita' headless senza GUI.


--------------------------------------------------------------------------------
4. SCHEMA JSON PER LAYOUT PERSONALIZZATO (GuiLayoutSpec / gui_layout.json)
--------------------------------------------------------------------------------

greg_gui_core integra un motore di deserializzazione JSON opzionale che consente
agli utenti e al GUI Designer di definire un layout grafico personalizzato
con posizionamento assoluto dei widget (PositionType::SelfDirected).

4.1 Specifiche dello Schema JSON:
```json
{
  "window_width": 820.0,
  "window_height": 360.0,
  "background_hex": "#20232A",
  "accent_hex": "#E5A93B",
  "elements": [
    {
      "param_id": "threshold",
      "label": "Threshold",
      "widget_type": "knob",
      "x": 20.0,
      "y": 40.0,
      "width": 80.0,
      "height": 88.0
    },
    {
      "param_id": "ratio",
      "label": "Ratio",
      "widget_type": "enum",
      "x": 110.0,
      "y": 40.0,
      "width": 80.0,
      "height": 88.0
    },
    {
      "param_id": "in_meter",
      "label": "IN",
      "widget_type": "meter",
      "x": 700.0,
      "y": 30.0,
      "width": 30.0,
      "height": 280.0
    }
  ]
}
```

4.2 Tipi di Widget Supportati:
- "knob": manopola rotativa continua per parametri FloatParam.
- "toggle": interruttore on/off per parametri BoolParam.
- "enum": selettore a scatti per parametri IntParam / Enum.
- "meter": meter di livello audio ("in_meter", "out_meter", "gr_meter").
- "spectrum": display analizzatore spettrale FFT.

4.3 Ordine di Ricerca del File di Layout (load_gui_layout):
All'apertura della finestra UI, il sistema cerca il layout nel seguente ordine:
1. Variabile d'ambiente: GREG_GUI_LAYOUT_<PLUGIN_ID> (per testing rapido e CI).
2. File gui_layout.json nella directory corrente di lavoro.
3. File gui_layout.json collocato nella cartella accanto al file binario .clap.

4.4 Retrocompatibilita' 100%:
La deserializzazione con serde_json avviene ESCLUSIVAMENTE all'apertura della
finestra UI (MAI nel thread audio). Se il file gui_layout.json non esiste o
contiene errori di sintassi, il loader restituisce None e la GUI esegue
automaticamente il fallback alla classica griglia procedurale param_grid().


--------------------------------------------------------------------------------
5. ESEMPIO DI CREAZIONE EDITOR NEL PLUGIN
--------------------------------------------------------------------------------

```rust
use greg_gui_core::create_greg_editor;
use nih_plug::prelude::Editor;

impl Plugin for MyPlugin {
    // ...
    fn editor(&self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        create_greg_editor(
            self.params.clone(),
            self.telemetry.clone(),
            "Greg 76 Compressor",
            // Callback costruttore controlli pannello principale
            |cx, params| {
                RotaryKnob::new(cx, params, |p| &p.input);
                RotaryKnob::new(cx, params, |p| &p.output);
                RotaryKnob::new(cx, params, |p| &p.attack);
                RotaryKnob::new(cx, params, |p| &p.release);
                EnumSwitch::new(cx, params, |p| &p.ratio);
            },
            // Callback costruttore controlli pannello avanzato (+ ADV)
            |cx, params| {
                RotaryKnob::new(cx, params, |p| &p.iron);
                RotaryKnob::new(cx, params, |p| &p.analog_drift);
            },
        )
    }
}
```
================================================================================
