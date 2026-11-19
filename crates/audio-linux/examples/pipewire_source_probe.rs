//! Sonda acotada de la fuente PipeWire de openrelay sobre un servidor privado.
//!
//! Abre [`PipeWireSource`] (queda inactiva), deja que el servidor registre el
//! nodo, empuja una fixture PCM16LE determinista —una rampa triangular con
//! marcador cada 100 ms— y activa la entrega sólo durante una ventana acotada.
//! Al final desactiva, desconecta y publica snapshots de los contadores del
//! callback tomados **después** de cada operación.
//!
//! La fixture es **entrada** del productor: el ejemplo no fabrica captura. Las
//! muestras que salgan por el nodo las produce el callback real copiando este
//! anillo SPSC; el grabador externo (`pw-record`) las lee del servidor.
//!
//! Tres decisiones de instrumentación que el resumen hace visibles:
//!
//! - **Continuidad de la fixture.** El índice de la muestra se avanza sólo por
//!   `PushOutcome.written`, nunca por el tamaño ofrecido. Si un empuje se corta
//!   por capacidad, la cola se regenera desde el índice aceptado en el intento
//!   siguiente, así que el anillo nunca contiene un hueco de la rampa. El
//!   relleno se acota además a `free()` real, de modo que el descarte por
//!   desbordamiento del propio probe es cero y se contabiliza igual.
//! - **Ritmo desacoplado de los wakeups.** `iterate(timeout)` es una espera
//!   máxima, no un reloj: puede volver antes. El relleno se calcula por reloj
//!   monótono (`elapsed * rate`) y se limita a la capacidad disponible, así que
//!   un despertar temprano o tardío no altera la secuencia ni el cómputo.
//! - **Snapshots de ciclo de vida.** Los contadores posteriores a desactivar y
//!   a desconectar se leen con `counters()` después de bombear el bucle; no se
//!   suman snapshots anteriores. Se publican valor absoluto y delta contra el
//!   snapshot inmediatamente anterior.
//! - **Identidad en dos etapas.** El id de nodo se sigue mientras la fuente
//!   vive: el resumen publica `node_id_observed` (último id válido visto antes
//!   de desactivar/desconectar) separado de `node_id_at_exit` (el que el
//!   backend expone al salir). Tras `disconnect` el proxy borra el nodo y el
//!   método devuelve `u32::MAX`; ese sentinel no es un id y se publica `null`.
//!   Si nunca se observó un id válido, también `null`: no se sustituye por
//!   ningún valor conocido.
//! - **Etiquetas de tiempo.** Las líneas de desactivar y desconectar publican
//!   `call_elapsed_ms` (duración de la llamada) y `observation_elapsed_ms`
//!   (desde antes de la llamada hasta después del bombeo) junto al
//!   `pump_requested_ms` pedido. Ningún campo se rotula como latencia de
//!   asentamiento: esa latencia no se observó.
//!
//! `is_active()` sólo refleja la decisión del propietario: el resumen no
//! afirma ausencia de callbacks a partir de él, publica los contadores
//! observados para que la ausencia o presencia se lea de ellos.
//!
//! Uso: `pipewire_source_probe [segundos_ventana]` con `PW` ya apuntando al
//! servidor privado (XDG_RUNTIME_DIR/PIPEWIRE_REMOTE). La ventana se acota a
//! `[MIN_WINDOW_S, MAX_WINDOW_S]`.
//!
//! Códigos de salida: `0` sólo si el resumen declara `ok`; `2`/`3` si abrir o
//! activar falló; `4` si nunca se negoció el contrato; `5` si el contrato se
//! negoció pero el callback no entregó ni un frame. Un `0` con cero muestras no
//! es un PASS.

use openrelay_audio_core::{SAMPLE_COUNT, SAMPLE_RATE_HZ};
use openrelay_audio_linux::{init, NegotiatedFormat, PipeWireSource, SampleProducer};
use std::time::{Duration, Instant};

/// Periodo de la rampa en muestras: 500 ms a 48 kHz, pendiente ±5.
const RAMP_PERIOD: u64 = 24_000;
/// Cada cuántas muestras se intercala el marcador (100 ms).
const MARKER_PERIOD: u64 = 4_800;
/// Nivel de marcador: pico que la rampa nunca alcanza (|v| <= 30 000).
const MARKER_LEVEL: i16 = 32_000;

/// Ventana de entrega por defecto, en segundos.
const DEFAULT_WINDOW_S: u64 = 6;
/// Cota inferior de la ventana: por debajo no hay ni un periodo de snapshot.
const MIN_WINDOW_S: u64 = 1;
/// Cota superior de la ventana: acota la captura y evita `Instant` lejano.
const MAX_WINDOW_S: u64 = 60;
/// Capacidad del anillo en frames del contrato (8 × 5 ms).
const RING_FRAMES: usize = 8;
/// Tiempo máximo de una espera del bucle dentro de la ventana.
const PUMP_SLICE: Duration = Duration::from_millis(5);
/// Cadencia de las líneas `T11B_MID`.
const SNAPSHOT_PERIOD: Duration = Duration::from_secs(2);
/// Espera acotada, sólo observacional, por la primera negociación.
const NEGOTIATION_WAIT: Duration = Duration::from_millis(1_500);
/// Bombeo tras desactivar: deja procesar la pausa antes de leer contadores.
const POST_DEACTIVATE_PUMP: Duration = Duration::from_millis(300);
/// Bombeo tras desconectar: deja procesar el desmontaje antes de leer.
const POST_DISCONNECT_PUMP: Duration = Duration::from_millis(200);

/// Fixture determinista del productor: rampa triangular de pendiente ±5 con un
/// pico de marcador cada [`MARKER_PERIOD`] muestras.
///
/// La pendiente constante permite alinear la captura aunque empiece en mitad de
/// una rampa; el marcador acota la ambigüedad a un periodo y delata huecos
/// (silencio) que no deben confundirse con una entrega exacta.
fn fixture_sample(n: u64) -> i16 {
    if n % MARKER_PERIOD == 0 {
        return MARKER_LEVEL;
    }
    let phase = (n % RAMP_PERIOD) as i64;
    let half = RAMP_PERIOD as i64 / 2;
    if phase < half {
        (-30_000 + phase * 5) as i16
    } else {
        (30_000 - (phase - half) * 5) as i16
    }
}

/// Acota la ventana pedida por argumento.
///
/// Un valor enorme no puede llegar a `Instant::now() + Duration`: la cota evita
/// el desbordamiento y una captura sin fin.
fn clamp_window_s(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WINDOW_S)
        .clamp(MIN_WINDOW_S, MAX_WINDOW_S)
}

/// Muestras ofrecidas, aceptadas y descartadas por el productor.
///
/// `attempted == accepted + dropped` en cada empuje: `dropped` es lo que
/// `PushOutcome` reportó como no escrito, no lo que se supuso.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PushTotals {
    attempted: u64,
    accepted: u64,
    dropped: u64,
}

/// Contadores del callback más el hueco libre del anillo en ese instante.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    buffers: u64,
    frames: u64,
    underrun: u64,
    free: usize,
}

impl Snapshot {
    /// Diferencia contra un snapshot anterior, saturando en cero.
    fn since(self, before: Self) -> Self {
        Self {
            buffers: self.buffers.saturating_sub(before.buffers),
            frames: self.frames.saturating_sub(before.frames),
            underrun: self.underrun.saturating_sub(before.underrun),
            free: self.free,
        }
    }
}

/// Lee un snapshot no simultáneo de la fuente: contadores y hueco libre.
///
/// Los tres contadores son lecturas independientes (documentado en el
/// backend); aquí se copian juntos para publicar un instante aproximado.
fn snapshot(source: &mut PipeWireSource) -> Snapshot {
    let (buffers, frames, underrun) = source.counters();
    let free = source.producer().free();
    Snapshot {
        buffers,
        frames,
        underrun,
        free,
    }
}

/// Rellena el anillo hasta que el total aceptado llegue a `target`, sin
/// saltarse muestras de la fixture.
///
/// Invariante central: `next` (índice de la próxima muestra) avanza **sólo**
/// `PushOutcome.written`. Si el anillo acepta un prefijo, la cola se vuelve a
/// generar desde ese índice en la iteración siguiente, así que la secuencia
/// contenida es siempre un prefijo contiguo de la fixture. El tamaño ofrecido
/// se acota a `free()` real: el probe no provoca descartes, y si el anillo
/// rechazara algo se contabiliza en `dropped` en vez de darse por escrito.
///
/// Devuelve las muestras aceptadas en esta llamada.
fn fill_until(
    producer: &mut SampleProducer,
    next: &mut u64,
    target: u64,
    totals: &mut PushTotals,
    scratch: &mut [i16],
) -> u64 {
    let mut accepted_now = 0u64;
    while *next < target {
        let free = producer.free();
        let want = ((target - *next) as usize).min(free).min(scratch.len());
        if want == 0 {
            break;
        }
        for (i, slot) in scratch[..want].iter_mut().enumerate() {
            *slot = fixture_sample(*next + i as u64);
        }
        let outcome = producer.push_slice(&scratch[..want]);
        totals.attempted += want as u64;
        totals.accepted += outcome.written as u64;
        totals.dropped += outcome.dropped as u64;
        *next += outcome.written as u64;
        accepted_now += outcome.written as u64;
        if outcome.written == 0 {
            // Anillo lleno pese a `free()`: se corta sin contabilizar como
            // escrito; la misma muestra se reintenta en la llamada siguiente.
            break;
        }
    }
    accepted_now
}

/// Objetivo de muestras entregadas al anillo según el reloj monótono.
///
/// Aritmética entera en `u128` para no perder precisión ni desbordar con
/// ventanas largas; el resultado se satura en `u64`.
fn scheduled_samples(prefill: u64, elapsed: Duration, rate_hz: u32) -> u64 {
    let scaled = elapsed.as_nanos() * rate_hz as u128 / 1_000_000_000u128;
    prefill.saturating_add(u64::try_from(scaled).unwrap_or(u64::MAX))
}

/// Bombea el bucle principal durante `total`, sin dormir fuera de él.
///
/// `iterate` puede volver antes de su timeout; el bucle repite hasta agotar el
/// plazo, así que el bombeo sí dura lo pedido.
fn pump(source: &PipeWireSource, total: Duration) {
    let start = Instant::now();
    while start.elapsed() < total {
        let left = total.saturating_sub(start.elapsed());
        let _ = source.iterate(left.min(PUMP_SLICE));
    }
}

/// Id de nodo observado antes del desmontaje y el vigente al salir.
///
/// `node_id()` reexpone el id vigente del stream y el proxy borra el nodo al
/// desconectar, así que el resumen separa las dos etapas en vez de publicar el
/// sentinel como si fuera el nodo observado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeIdReport {
    observed: Option<u32>,
    at_exit: Option<u32>,
}

/// Id de nodo válido, o `None` si el backend sólo expone el sentinel.
fn valid_node_id(raw: u32) -> Option<u32> {
    if raw == u32::MAX {
        None
    } else {
        Some(raw)
    }
}

/// Acumula el último id válido visto mientras la fuente sigue viva.
#[derive(Debug, Clone, Copy)]
struct NodeIdTrace {
    last_valid: Option<u32>,
}

impl NodeIdTrace {
    fn new() -> Self {
        Self { last_valid: None }
    }

    /// Guarda el id si el backend expone uno real; el sentinel no borra lo visto.
    fn observe(&mut self, raw: u32) {
        if let Some(id) = valid_node_id(raw) {
            self.last_valid = Some(id);
        }
    }

    /// Cierra la traza: `observed` es el último id válido visto antes del
    /// desmontaje y `at_exit` el que expone el backend al salir, ya filtrado.
    fn report(&self, at_exit: u32) -> NodeIdReport {
        NodeIdReport {
            observed: self.last_valid,
            at_exit: valid_node_id(at_exit),
        }
    }
}

/// Render JSON de un id opcional: número real o `null`, sin inventar.
fn json_node_id(id: Option<u32>) -> String {
    match id {
        Some(v) => v.to_string(),
        None => String::from("null"),
    }
}

/// Fragmento del resumen con las dos etapas del id, tal como lo publica
/// `T11B_SUMMARY`. Una sola implementación: las pruebas inspeccionan este texto.
fn summary_node_id_fields(report: NodeIdReport) -> String {
    format!(
        "\"node_id_observed\":{},\"node_id_at_exit\":{}",
        json_node_id(report.observed),
        json_node_id(report.at_exit)
    )
}

/// Tiempos observados de una operación de ciclo de vida.
///
/// `call` es lo que duró la llamada; `observation` va desde antes de la llamada
/// hasta después del bombeo que deja procesar la operación; `pump_requested` es
/// el plazo pedido al bombeo, no su duración real. Ninguno es una latencia de
/// asentamiento: tal latencia no se observó.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ElapsedReport {
    call: Duration,
    observation: Duration,
    pump_requested: Duration,
}

impl ElapsedReport {
    fn new(call: Duration, observation: Duration, pump_requested: Duration) -> Self {
        Self {
            call,
            observation,
            pump_requested,
        }
    }

    /// Etiquetas explícitas: duración de la llamada, ventana observada completa
    /// y plazo pedido al bombeo. Ninguna sugiere una pausa instantánea.
    fn render(&self) -> String {
        format!(
            "call_elapsed_ms={} observation_elapsed_ms={} pump_requested_ms={}",
            self.call.as_millis(),
            self.observation.as_millis(),
            self.pump_requested.as_millis()
        )
    }
}

/// Veredicto del resumen, derivado de lo observado y no de la intención.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    IncompleteNoContract,
    IncompleteNoCallback,
}

impl Status {
    /// Clasifica: sin contrato negociado nunca, el formato no es el del
    /// contrato; con contrato pero cero frames, el callback no entregó nada.
    /// Cero frames jamás cuenta como `Ok`.
    fn classify(contract_ever: bool, frames: u64) -> Self {
        if !contract_ever {
            Self::IncompleteNoContract
        } else if frames == 0 {
            Self::IncompleteNoCallback
        } else {
            Self::Ok
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::IncompleteNoContract => "incomplete_no_contract",
            Self::IncompleteNoCallback => "incomplete_no_callback",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::IncompleteNoContract => 4,
            Self::IncompleteNoCallback => 5,
        }
    }
}

fn main() {
    let window_s = clamp_window_s(std::env::args().nth(1).as_deref());
    init();

    let mut source = match PipeWireSource::open(RING_FRAMES * SAMPLE_COUNT) {
        Ok(s) => s,
        Err(e) => {
            println!("T11B_ERROR open {e}");
            std::process::exit(2);
        }
    };
    println!(
        "T11B_OPEN node_id={} active={} ring_samples={}",
        source.node_id(),
        source.is_active(),
        RING_FRAMES * SAMPLE_COUNT
    );
    // La identidad se sigue mientras la fuente vive: el primer id real visto se
    // guarda aquí y no se sustituye por el sentinel del desmontaje.
    let mut node_trace = NodeIdTrace::new();
    node_trace.observe(source.node_id());

    // Espera acotada y sólo observacional: el servidor registra el nodo y
    // negocia mientras corre el bucle. No se exige negociar antes de activar
    // —podría ser una dependencia circular—: si no hay formato aquí, la
    // activación sigue y la negociación posterior se reporta igual.
    let neg_deadline = Instant::now() + NEGOTIATION_WAIT;
    while Instant::now() < neg_deadline && !source.negotiated().matches_contract() {
        let _ = source.iterate(Duration::from_millis(20));
    }
    let neg_initial: NegotiatedFormat = source.negotiated();
    node_trace.observe(source.node_id());
    println!(
        "T11B_NEGOTIATED phase=initial node_id={} rate={} channels={} s16le={} contract={}",
        source.node_id(),
        neg_initial.rate,
        neg_initial.channels,
        neg_initial.s16le,
        neg_initial.matches_contract()
    );
    let mut contract_ever = neg_initial.matches_contract();

    // Prellenado: la ventana arranca con muestras ya presentes en el anillo.
    // `next` es el total aceptado y el índice de la fixture a la vez.
    let mut next: u64 = 0;
    let mut prefill = PushTotals::default();
    let mut window = PushTotals::default();
    let mut scratch = [0i16; SAMPLE_COUNT];
    let prefill_target = (RING_FRAMES * SAMPLE_COUNT) as u64;
    let prefill_accepted = {
        let producer = source.producer();
        fill_until(
            producer,
            &mut next,
            prefill_target,
            &mut prefill,
            &mut scratch,
        )
    };

    if let Err(e) = source.activate() {
        println!("T11B_ERROR activate {e}");
        let _ = source.disconnect();
        std::process::exit(3);
    }
    println!(
        "T11B_ACTIVE active={} prefill_accepted={} prefill_attempted={} prefill_dropped={} fixture_next={}",
        source.is_active(),
        prefill_accepted,
        prefill.attempted,
        prefill.dropped,
        next
    );

    // Ventana de entrega acotada. El relleno se calcula por reloj monótono y se
    // limita a `free()`; `iterate` sólo bombea, nunca dicta el ritmo.
    let start = Instant::now();
    let deadline = start
        .checked_add(Duration::from_secs(window_s))
        .unwrap_or_else(|| start + Duration::from_secs(MAX_WINDOW_S));
    let mut next_mid = start + SNAPSHOT_PERIOD;
    let mut behind_at_end = 0u64;
    let mut scheduled_at_end = prefill_target;

    while Instant::now() < deadline {
        let elapsed = Instant::now().duration_since(start);
        scheduled_at_end = scheduled_samples(prefill_target, elapsed, SAMPLE_RATE_HZ);
        {
            let producer = source.producer();
            fill_until(
                producer,
                &mut next,
                scheduled_at_end,
                &mut window,
                &mut scratch,
            );
        }
        behind_at_end = scheduled_at_end.saturating_sub(next);

        let left = deadline.saturating_duration_since(Instant::now());
        let _ = source.iterate(left.min(PUMP_SLICE));

        let now = Instant::now();
        while now >= next_mid && next_mid < deadline {
            let snap = snapshot(&mut source);
            let neg = source.negotiated();
            contract_ever |= neg.matches_contract();
            println!(
                "T11B_MID t_ms={} buffers={} frames={} underrun={} accepted={} attempted={} dropped={} free={} behind={} rate={} channels={} s16le={} contract={}",
                now.duration_since(start).as_millis(),
                snap.buffers,
                snap.frames,
                snap.underrun,
                window.accepted,
                window.attempted,
                window.dropped,
                snap.free,
                scheduled_at_end.saturating_sub(next),
                neg.rate,
                neg.channels,
                neg.s16le,
                neg.matches_contract()
            );
            next_mid += SNAPSHOT_PERIOD;
        }
    }

    let at_window_end = snapshot(&mut source);
    let neg_window = source.negotiated();
    contract_ever |= neg_window.matches_contract();
    println!(
        "T11B_WINDOW_END buffers={} frames={} underrun={} accepted={} attempted={} dropped={} scheduled={} fixture_next={} behind={} contract={} node_id_observed={}",
        at_window_end.buffers,
        at_window_end.frames,
        at_window_end.underrun,
        window.accepted,
        window.attempted,
        window.dropped,
        scheduled_at_end,
        next,
        behind_at_end,
        neg_window.matches_contract(),
        json_node_id(valid_node_id(source.node_id()))
    );

    // Desactivar y medir después de bombear: el valor absoluto es el del
    // callback al final del bombeo, y el delta se toma contra el instante
    // previo a la operación. No se suman snapshots viejos.
    //
    // `call` es lo que duró `deactivate()`; `observation` cubre desde antes de
    // la llamada hasta pasado el bombeo, que es cuando se leen los contadores.
    // El plazo pedido al bombeo se publica aparte: no es su duración real.
    let deact_observe_start = Instant::now();
    let deact_start = Instant::now();
    let deact = source.deactivate();
    let deact_call = deact_start.elapsed();
    pump(&source, POST_DEACTIVATE_PUMP);
    let post_deactivate = snapshot(&mut source);
    node_trace.observe(source.node_id());
    let deact_delta = post_deactivate.since(at_window_end);
    let deact_elapsed = ElapsedReport::new(
        deact_call,
        deact_observe_start.elapsed(),
        POST_DEACTIVATE_PUMP,
    );
    println!(
        "T11B_DEACTIVATE result={:?} active={} buffers={} frames={} underrun={} delta_frames={} delta_buffers={} {}",
        deact.is_ok(),
        source.is_active(),
        post_deactivate.buffers,
        post_deactivate.frames,
        post_deactivate.underrun,
        deact_delta.frames,
        deact_delta.buffers,
        deact_elapsed.render()
    );

    // El id se guarda justo antes de desconectar: lo que el proxy borre después
    // no puede reescribir lo que sí se observó con el nodo vivo.
    node_trace.observe(source.node_id());
    let disc_observe_start = Instant::now();
    let disc_start = Instant::now();
    let disc = source.disconnect();
    let disc_call = disc_start.elapsed();
    pump(&source, POST_DISCONNECT_PUMP);
    let post_disconnect = snapshot(&mut source);
    let disc_delta = post_disconnect.since(post_deactivate);
    let node_report = node_trace.report(source.node_id());
    let disc_elapsed = ElapsedReport::new(
        disc_call,
        disc_observe_start.elapsed(),
        POST_DISCONNECT_PUMP,
    );
    let neg_final = source.negotiated();
    contract_ever |= neg_final.matches_contract();
    let status = Status::classify(contract_ever, post_disconnect.frames);
    println!(
        "T11B_DISCONNECT result={:?} active={} buffers={} frames={} underrun={} delta_frames={} delta_buffers={} node_id_observed={} node_id_at_exit={} {}",
        disc.is_ok(),
        source.is_active(),
        post_disconnect.buffers,
        post_disconnect.frames,
        post_disconnect.underrun,
        disc_delta.frames,
        disc_delta.buffers,
        json_node_id(node_report.observed),
        json_node_id(node_report.at_exit),
        disc_elapsed.render()
    );

    // El resumen es una sola línea JSON: se arma en un buffer y se imprime una
    // vez, sin partir el objeto entre dos `println!`.
    let mut summary = format!(
        "T11B_SUMMARY {{\"status\":\"{}\",{},\"ring_samples\":{},",
        status.as_str(),
        summary_node_id_fields(node_report),
        RING_FRAMES * SAMPLE_COUNT
    );
    summary.push_str(&format!(
        "\"window_s\":{},\"prefill_attempted\":{},\"prefill_accepted\":{},\"prefill_dropped\":{},\"window_attempted\":{},\"window_accepted\":{},\"window_dropped\":{},\"scheduled_at_window_end\":{},\"fixture_next\":{},\"behind_at_window_end\":{},\"buffers\":{},\"frames\":{},\"underrun\":{},\"post_deactivate_buffers\":{},\"post_deactivate_frames\":{},\"post_deactivate_frames_delta\":{},\"post_deactivate_buffers_delta\":{},\"post_deactivate_underrun\":{},\"post_disconnect_buffers\":{},\"post_disconnect_frames\":{},\"post_disconnect_frames_delta\":{},\"post_disconnect_buffers_delta\":{},\"post_disconnect_underrun\":{},\"neg_initial_rate\":{},\"neg_initial_channels\":{},\"neg_initial_s16le\":{},\"neg_initial_contract\":{},\"neg_window_rate\":{},\"neg_window_channels\":{},\"neg_window_s16le\":{},\"neg_window_contract\":{},\"neg_final_rate\":{},\"neg_final_channels\":{},\"neg_final_s16le\":{},\"neg_final_contract\":{},\"contract_ever\":{},\"active_at_exit\":{}}}",
        window_s,
        prefill.attempted,
        prefill.accepted,
        prefill.dropped,
        window.attempted,
        window.accepted,
        window.dropped,
        scheduled_at_end,
        next,
        behind_at_end,
        at_window_end.buffers,
        at_window_end.frames,
        at_window_end.underrun,
        post_deactivate.buffers,
        post_deactivate.frames,
        deact_delta.frames,
        deact_delta.buffers,
        post_deactivate.underrun,
        post_disconnect.buffers,
        post_disconnect.frames,
        disc_delta.frames,
        disc_delta.buffers,
        post_disconnect.underrun,
        neg_initial.rate,
        neg_initial.channels,
        neg_initial.s16le,
        neg_initial.matches_contract(),
        neg_window.rate,
        neg_window.channels,
        neg_window.s16le,
        neg_window.matches_contract(),
        neg_final.rate,
        neg_final.channels,
        neg_final.s16le,
        neg_final.matches_contract(),
        contract_ever,
        source.is_active()
    ));
    println!("{summary}");

    std::process::exit(status.exit_code());
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrelay_audio_linux::ring;

    /// La fixture es una función pura del índice: mismo índice, misma muestra,
    /// con marcador exacto cada [`MARKER_PERIOD`] y fuera del rango de la rampa.
    #[test]
    fn fixture_es_determinista_y_marca_cada_100ms() {
        assert_eq!(fixture_sample(0), MARKER_LEVEL);
        assert_eq!(fixture_sample(MARKER_PERIOD), MARKER_LEVEL);
        assert_eq!(fixture_sample(2 * MARKER_PERIOD), MARKER_LEVEL);
        assert_eq!(fixture_sample(1), -29_995);
        assert_eq!(fixture_sample(2), -29_990);
        assert_eq!(
            fixture_sample(MARKER_PERIOD - 1),
            fixture_sample(MARKER_PERIOD - 1)
        );
        for n in 0..RAMP_PERIOD {
            if n % MARKER_PERIOD == 0 {
                continue;
            }
            let v = fixture_sample(n);
            assert!(
                (-30_000..=30_000).contains(&v),
                "rama fuera de rango en {n}"
            );
        }
    }

    /// Continuidad con anillo real: el relleno se detiene en la capacidad, el
    /// total aceptado coincide con el índice de la fixture y el consumidor lee
    /// exactamente el prefijo de la fixture, sin huecos ni duplicados.
    #[test]
    fn fill_respeta_capacidad_y_conserva_la_secuencia() {
        let (mut producer, mut consumer) = ring(7);
        let mut next = 0u64;
        let mut totals = PushTotals::default();
        let mut scratch = [0i16; 8];

        let accepted = fill_until(&mut producer, &mut next, 20, &mut totals, &mut scratch);

        assert_eq!(accepted, 7, "la capacidad real acota el relleno");
        assert_eq!(next, 7, "el índice avanza por muestras aceptadas");
        assert_eq!(totals.accepted, 7);
        assert_eq!(totals.attempted, 7, "no se ofrece lo que no cabe");
        assert_eq!(totals.dropped, 0);
        assert_eq!(producer.dropped(), 0);

        // Drena 3 y vuelve a rellenar: la secuencia continúa donde quedó.
        let mut drained = [0u8; 3 * 2];
        let out = consumer.render_pcm16le_into(&mut drained, 0, 2, 3);
        assert_eq!(out.frames, 3);
        assert_eq!(out.underrun_frames, 0);
        let again = fill_until(&mut producer, &mut next, 20, &mut totals, &mut scratch);
        assert_eq!(again, 3, "sólo caben las 3 muestras drenadas");
        assert_eq!(next, 10);

        // El anillo vuelve a estar lleno: 4 muestras viejas (3..7) + 3 nuevas
        // (7..10) = 7. Se drena completo y se compara con la fixture en su
        // índice absoluto, lo que prueba que no hubo hueco ni duplicado.
        let mut all = [0u8; 7 * 2];
        let out = consumer.render_pcm16le_into(&mut all, 0, 2, 7);
        assert_eq!(out.frames, 7);
        assert_eq!(
            out.underrun_frames, 0,
            "el anillo estaba lleno tras reponer"
        );
        for i in 0..7 {
            let value = i16::from_le_bytes([all[i * 2], all[i * 2 + 1]]);
            assert_eq!(
                value,
                fixture_sample(3 + i as u64),
                "muestra {i} corresponde al índice {} de la fixture",
                3 + i
            );
        }
    }

    /// Empuje parcial real: `push_slice` recorta y reporta descarte, y el
    /// cursor sigue el aceptado, no el tamaño ofrecido. Tras el recorte, el
    /// relleno continúa exactamente en el índice aceptado.
    #[test]
    fn cursor_avanza_por_escrito_no_por_ofrecido() {
        let (mut producer, mut consumer) = ring(5);
        let offered: [i16; 7] = [1, 2, 3, 4, 5, 6, 7];
        let outcome = producer.push_slice(&offered);
        assert_eq!(outcome.written, 5);
        assert_eq!(outcome.dropped, 2);
        assert_eq!(producer.dropped(), 2);

        // El cursor de la fixture vive en el aceptado: 5, no 7.
        let mut next = outcome.written as u64;
        let mut totals = PushTotals::default();
        let mut scratch = [0i16; 4];
        let mut drained = [0u8; 5 * 2];
        assert_eq!(
            consumer.render_pcm16le_into(&mut drained, 0, 2, 5).frames,
            5
        );
        let target = next + 2;
        let accepted = fill_until(&mut producer, &mut next, target, &mut totals, &mut scratch);
        assert_eq!(accepted, 2);
        assert_eq!(next, 7);
        assert_eq!(totals.dropped, 0);

        let mut tail = [0u8; 2 * 2];
        assert_eq!(consumer.render_pcm16le_into(&mut tail, 0, 2, 2).frames, 2);
        assert_eq!(
            i16::from_le_bytes([tail[0], tail[1]]),
            fixture_sample(5),
            "continúa en la muestra 5 de la fixture"
        );
        assert_eq!(i16::from_le_bytes([tail[2], tail[3]]), fixture_sample(6));
    }

    /// Un objetivo ya alcanzado no escribe nada y no altera los totales.
    #[test]
    fn objetivo_alcanzado_no_escribe() {
        let (mut producer, _consumer) = ring(4);
        let mut next = 0u64;
        let mut totals = PushTotals::default();
        let mut scratch = [0i16; 4];
        assert_eq!(
            fill_until(&mut producer, &mut next, 0, &mut totals, &mut scratch),
            0
        );
        assert_eq!(totals, PushTotals::default());
        assert_eq!(producer.dropped(), 0);
    }

    /// El delta de snapshots es la diferencia real, nunca una suma acumulada,
    /// y satura en cero si el orden se invierte.
    #[test]
    fn delta_de_snapshot_es_diferencia_no_suma() {
        let before = Snapshot {
            buffers: 10,
            frames: 2_400,
            underrun: 7,
            free: 12,
        };
        let after = Snapshot {
            buffers: 13,
            frames: 3_120,
            underrun: 9,
            free: 4,
        };
        let d = after.since(before);
        assert_eq!(d.buffers, 3);
        assert_eq!(d.frames, 720);
        assert_eq!(d.underrun, 2);
        assert_eq!(d.free, 4, "el hueco libre es el del snapshot posterior");

        let inverted = before.since(after);
        assert_eq!(inverted.frames, 0, "no hay delta negativo");
    }

    /// El objetivo por reloj crece con el tiempo transcurrido y satura sin
    /// desbordar con entradas enormes.
    #[test]
    fn objetivo_por_reloj_es_aritmetica_acotada() {
        assert_eq!(scheduled_samples(0, Duration::ZERO, SAMPLE_RATE_HZ), 0);
        assert_eq!(
            scheduled_samples(0, Duration::from_secs(1), SAMPLE_RATE_HZ),
            SAMPLE_RATE_HZ as u64
        );
        assert_eq!(
            scheduled_samples(1_920, Duration::from_millis(5), SAMPLE_RATE_HZ),
            1_920 + 240
        );
        assert_eq!(
            scheduled_samples(u64::MAX - 1, Duration::from_secs(60), SAMPLE_RATE_HZ),
            u64::MAX,
            "satura en vez de envolver"
        );
    }

    /// La ventana se acota por ambos extremos y un argumento inválido cae al
    /// valor por defecto, sin llegar nunca a un `Instant` desbordado.
    #[test]
    fn ventana_se_acota_y_el_argumento_invalido_usa_default() {
        assert_eq!(clamp_window_s(None), DEFAULT_WINDOW_S);
        assert_eq!(clamp_window_s(Some("3")), 3);
        assert_eq!(clamp_window_s(Some("0")), MIN_WINDOW_S);
        assert_eq!(
            clamp_window_s(Some("999999999999999999999")),
            DEFAULT_WINDOW_S
        );
        assert_eq!(
            clamp_window_s(Some(u64::MAX.to_string().as_str())),
            MAX_WINDOW_S
        );
        assert_eq!(clamp_window_s(Some("6.5")), DEFAULT_WINDOW_S);
    }

    /// Un veredicto con cero frames jamás es `ok`; el contrato ausente manda
    /// sobre el conteo y cada estado tiene su código de salida no cero.
    #[test]
    fn status_no_declara_ok_sin_contrato_ni_sin_frames() {
        assert_eq!(
            Status::classify(false, 0),
            Status::IncompleteNoContract,
            "sin contrato no hay audio del perfil"
        );
        assert_eq!(Status::classify(false, 720), Status::IncompleteNoContract);
        assert_eq!(
            Status::classify(true, 0),
            Status::IncompleteNoCallback,
            "contrato sin frames no es PASS"
        );
        assert_eq!(Status::classify(true, 720), Status::Ok);
        assert_ne!(Status::IncompleteNoContract.exit_code(), 0);
        assert_ne!(Status::IncompleteNoCallback.exit_code(), 0);
        assert_eq!(Status::Ok.exit_code(), 0);
    }

    /// El sentinel del backend no es un id: se filtra antes de publicar.
    #[test]
    fn sentinel_no_es_un_id_valido() {
        assert_eq!(valid_node_id(u32::MAX), None);
        assert_eq!(valid_node_id(17), Some(17));
        assert_eq!(
            valid_node_id(0),
            Some(0),
            "el 0 se publica si el backend lo da"
        );
    }

    /// Caso observado en la captura real: id 17 vivo, `u32::MAX` tras
    /// desconectar. El resumen conserva 17 como observado y no publica el
    /// sentinel como si fuera el nodo; el id de salida queda `null`.
    #[test]
    fn resumen_conserva_id_observado_y_no_publica_el_sentinel() {
        let mut trace = NodeIdTrace::new();
        trace.observe(17);
        trace.observe(u32::MAX);
        let report = trace.report(u32::MAX);

        assert_eq!(report.observed, Some(17), "el id visto antes sobrevive");
        assert_eq!(report.at_exit, None, "el sentinel al salir no es un id");
        let fields = summary_node_id_fields(report);
        assert!(fields.contains("\"node_id_observed\":17"), "{fields}");
        assert!(
            fields.contains("\"node_id_at_exit\":null"),
            "el sentinel no se imprime como número: {fields}"
        );
        assert!(
            !fields.contains("4294967295"),
            "u32::MAX no aparece en el resumen: {fields}"
        );
    }

    /// Nunca se observó un id válido: ni antes ni al salir. Se publica `null`
    /// en las dos etapas, sin inventar el 17 de otra corrida.
    #[test]
    fn sin_id_valido_observado_publica_null_en_ambas_etapas() {
        let mut trace = NodeIdTrace::new();
        trace.observe(u32::MAX);
        let report = trace.report(u32::MAX);

        assert_eq!(report.observed, None);
        assert_eq!(report.at_exit, None);
        let fields = summary_node_id_fields(report);
        assert!(fields.contains("\"node_id_observed\":null"), "{fields}");
        assert!(fields.contains("\"node_id_at_exit\":null"), "{fields}");
        assert!(
            !fields.contains("17"),
            "no se rellena con un id ajeno: {fields}"
        );
    }

    /// Un id que sigue vigente al salir se distingue del observado antes: las
    /// dos etapas se publican por separado aunque coincidan en valor.
    #[test]
    fn etapas_de_id_se_distinguen_aunque_el_valor_coincida() {
        let mut trace = NodeIdTrace::new();
        trace.observe(17);
        let report = trace.report(17);

        assert_eq!(report.observed, Some(17));
        assert_eq!(report.at_exit, Some(17));
        let fields = summary_node_id_fields(report);
        assert!(fields.contains("\"node_id_observed\":17"), "{fields}");
        assert!(fields.contains("\"node_id_at_exit\":17"), "{fields}");
        assert_eq!(fields.matches("17").count(), 2, "{fields}");
    }

    /// El sentinel no borra un id ya visto: una lectura posterior del método,
    /// por ejemplo tras desactivar, no degrada la identidad observada.
    #[test]
    fn sentinel_posterior_no_borra_lo_observado() {
        let mut trace = NodeIdTrace::new();
        trace.observe(17);
        trace.observe(u32::MAX);
        assert_eq!(trace.last_valid, Some(17));

        trace.observe(u32::MAX);
        assert_eq!(trace.report(17).observed, Some(17));
    }

    /// Las etiquetas de tiempo separan la llamada de la ventana observada y del
    /// plazo pedido al bombeo. Un `call` de 0 ms con 300 ms de observación no
    /// se rotula como asentamiento en 0 ms: son campos distintos.
    #[test]
    fn tiempos_se_etiquetan_por_separado_y_no_como_settle() {
        let report = ElapsedReport::new(
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(300),
        );
        let rendered = report.render();

        assert!(rendered.contains("call_elapsed_ms=0"), "{rendered}");
        assert!(
            rendered.contains("observation_elapsed_ms=300"),
            "{rendered}"
        );
        assert!(rendered.contains("pump_requested_ms=300"), "{rendered}");
        assert!(
            !rendered.contains("settle"),
            "no se publica una latencia de asentamiento no observada: {rendered}"
        );
        assert!(
            !rendered.contains(" elapsed_ms="),
            "la etiqueta ambigua ya no existe: {rendered}"
        );
    }

    /// La ventana observada nunca es menor que la llamada: se mide desde antes
    /// de la operación hasta después del bombeo.
    #[test]
    fn observacion_cubre_la_llamada_y_el_bombeo() {
        let report = ElapsedReport::new(
            Duration::from_millis(2),
            Duration::from_millis(302),
            Duration::from_millis(300),
        );
        assert!(report.observation >= report.call);
        assert!(
            report.observation >= report.pump_requested,
            "el bombeo pedido cabe en la ventana observada"
        );
        assert_ne!(
            report.call, report.observation,
            "llamada y observación no son el mismo número"
        );
    }

    /// Replay de la secuencia cruda de `a2.probe`: `T11B_OPEN` ya trae el
    /// sentinel (el nodo aún no existe), `T11B_NEGOTIATED` trae 17 y tras
    /// desconectar vuelve el sentinel. El resumen debe publicar 17 como
    /// observado y `null` al salir — lo que el archivo real no hacía.
    #[test]
    fn replay_de_a2_probe_corrige_el_id_del_resumen() {
        let mut trace = NodeIdTrace::new();
        trace.observe(u32::MAX); // T11B_OPEN node_id=4294967295
        trace.observe(17); // T11B_NEGOTIATED node_id=17
        trace.observe(17); // antes de deactivate
        trace.observe(u32::MAX); // tras deactivate
        let report = trace.report(u32::MAX); // tras disconnect

        assert_eq!(report.observed, Some(17), "el 17 sí se observó con vida");
        assert_eq!(report.at_exit, None);

        let fields = summary_node_id_fields(report);
        assert!(fields.contains("\"node_id_observed\":17"), "{fields}");
        assert!(fields.contains("\"node_id_at_exit\":null"), "{fields}");
        assert!(
            !fields.contains("4294967295"),
            "el resumen corregido ya no publica el sentinel: {fields}"
        );
        assert!(
            !fields.contains("\"node_id\":"),
            "el campo ambiguo se sustituyó por las dos etapas: {fields}"
        );
    }
}
