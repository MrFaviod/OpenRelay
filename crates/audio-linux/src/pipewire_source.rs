//! Fuente de micrófono nativa de PipeWire.
//!
//! Publica un nodo `Audio/Source` (`node.name = arl-mic`) que entrega PCM16LE
//! mono 48 kHz. El stream se crea **inactivo**: la fuente no aparece activa ni
//! empuja muestras hasta que el propietario autorizado llame a
//! [`PipeWireSource::activate`]. Sin esa llamada el stream queda conectado pero
//! en pausa, que es el único estado admisible mientras no exista un daemon que
//! decida cuándo capturar (T14).
//!
//! El callback de proceso sólo hace tres cosas: tomar un buffer del stream,
//! copiar muestras ya presentes en el SPSC mediante
//! [`SampleConsumer::render_pcm16le_into`] y devolver el buffer al stream. No
//! asigna, no registra, no toca el sistema de archivos ni la red, no duerme y
//! no reconfigura el stream.

use arl_audio_core::{BYTES_PER_SAMPLE, CHANNELS, SAMPLE_RATE_HZ};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use spa::param::audio::AudioInfoRaw;
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::serialize::PodSerializer;
use spa::pod::Value;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::ring::{ring, RenderOutcome, SampleConsumer, SampleProducer};

/// Nombre de nodo que arl publica en el grafo de PipeWire.
pub const NODE_NAME: &str = "arl-mic";
/// `media.class` de una fuente de audio (nodo del que otros consumen).
pub const MEDIA_CLASS: &str = "Audio/Source";
/// Canales del perfil v1.
pub const CHANNEL_COUNT: u32 = CHANNELS as u32;

/// Errores de construcción del backend.
#[derive(Debug)]
pub enum SourceError {
    /// PipeWire rechazó la creación del bucle, contexto, core o stream.
    PipeWire(pw::Error),
    /// La descripción del formato no serializó a un pod SPA válido.
    FormatPod,
    /// El stream no llegó a `Paused`/`Streaming` tras conectar.
    NotConnected,
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::PipeWire(e) => write!(f, "pipewire: {e}"),
            SourceError::FormatPod => write!(f, "no se pudo serializar el formato S16LE mono 48k"),
            SourceError::NotConnected => write!(f, "el stream no alcanzó el estado conectado"),
        }
    }
}

impl std::error::Error for SourceError {}

impl From<pw::Error> for SourceError {
    fn from(value: pw::Error) -> Self {
        SourceError::PipeWire(value)
    }
}

/// Formato que el servidor negoció, copiado sin asignar memoria en el callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedFormat {
    /// Frecuencia en Hz, `0` si aún no se negoció.
    pub rate: u32,
    /// Canales, `0` si aún no se negoció.
    pub channels: u32,
    /// `true` cuando el formato es exactamente S16LE.
    pub s16le: bool,
}

impl NegotiatedFormat {
    /// Formato sin negociar.
    pub const UNSET: Self = Self {
        rate: 0,
        channels: 0,
        s16le: false,
    };

    /// `true` si el servidor confirmó el perfil del contrato.
    pub fn matches_contract(&self) -> bool {
        self.s16le && self.rate == SAMPLE_RATE_HZ && self.channels == CHANNEL_COUNT
    }

    /// Empaqueta el formato en una sola palabra `u64`: `rate` en los 32 bits
    /// bajos, `channels` en los 31 siguientes y `s16le` en el bit alto.
    ///
    /// Una única palabra permite publicar y leer el formato con **una**
    /// operación atómica, sin que el callback pueda observar una mezcla de dos
    /// negociaciones. `channels` se enmascara a 31 bits: la cota real es
    /// [`spa::param::audio::MAX_CHANNELS`] (64), así que el bit alto queda libre
    /// para `s16le`.
    fn packed(self) -> u64 {
        (self.rate as u64)
            | (((self.channels & CHANNEL_BITS) as u64) << 32)
            | ((self.s16le as u64) << 63)
    }

    /// Deshace [`NegotiatedFormat::packed`].
    fn from_packed(packed: u64) -> Self {
        Self {
            rate: packed as u32,
            channels: ((packed >> 32) & CHANNEL_BITS as u64) as u32,
            s16le: (packed >> 63) != 0,
        }
    }
}

/// Máscara de canales dentro de la palabra empaquetada de [`NegotiatedFormat`].
const CHANNEL_BITS: u32 = 0x7fff_ffff;

/// Efecto de un `param_changed` sobre el formato negociado.
///
/// Se separa de la publicación del formato para poder ejercitarlo con los
/// mismos pods serializados que recibe el callback real, sin stream ni servidor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatUpdate {
    /// El parámetro no es de formato: el formato anterior permanece.
    Keep,
    /// El formato negociado pasa a [`NegotiatedFormat::UNSET`].
    Reset,
    /// El servidor confirmó un formato interpretable.
    Set(NegotiatedFormat),
}

/// Decide el efecto de un `param_changed` sin tocar el estado.
///
/// Es exactamente la ruta que ejecuta el callback registrado en `open`: el
/// callback sólo publica el resultado. No hay una segunda copia para tests.
///
/// Un `Format` presente que no se puede interpretar **invalida** el estado en
/// vez de conservarlo: un formato viejo sobreviviente se publicaría como audio
/// del contrato. Sólo un id ajeno —que no es de formato— conserva el valor.
fn format_update(id: u32, param: Option<&Pod>) -> FormatUpdate {
    if id != spa::param::ParamType::Format.as_raw() {
        return FormatUpdate::Keep;
    }
    let Some(param) = param else {
        return FormatUpdate::Reset;
    };
    let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
        return FormatUpdate::Reset;
    };
    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
        return FormatUpdate::Reset;
    }
    let mut info = AudioInfoRaw::new();
    if info.parse(param).is_err() {
        return FormatUpdate::Reset;
    }
    FormatUpdate::Set(NegotiatedFormat {
        rate: info.rate(),
        channels: info.channels(),
        s16le: info.format() == spa::param::audio::AudioFormat::S16LE,
    })
}

/// Aplica el efecto de [`format_update`] a la palabra publicada.
///
/// Es la única escritura del formato negociado: `param_changed` corre en el
/// mainloop y `process` en el hilo de datos, así que la publicación tiene que
/// ser atómica de verdad. Empaquetar el formato en un `u64` y publicarlo con un
/// `store(Release)` da al lector una sola lectura `load(Acquire)` coherente:
/// nunca ve `rate` de una negociación con `channels` de otra. Un `Mutex` no es
/// opción en el camino RT y tres atómicos separados sí permitirían esa mezcla.
fn apply_format_update(format: &AtomicU64, update: FormatUpdate) {
    match update {
        FormatUpdate::Keep => {}
        FormatUpdate::Reset => format.store(NegotiatedFormat::UNSET.packed(), Ordering::Release),
        FormatUpdate::Set(negotiated) => format.store(negotiated.packed(), Ordering::Release),
    }
}

/// Lee el formato publicado por [`apply_format_update`] como un valor completo.
fn read_negotiated(format: &AtomicU64) -> NegotiatedFormat {
    NegotiatedFormat::from_packed(format.load(Ordering::Acquire))
}

/// Paso de copia del callback sobre un buffer ya dequeueado.
///
/// `slice` es el plano mapeado del `Data` (`None` cuando `data()` no expone
/// memoria), `requested` la sugerencia del buffer (0 = sin sugerencia) y
/// `format_ok` el resultado de [`NegotiatedFormat::matches_contract`] sobre el
/// estado negociado.
///
/// - Formato no conforme: no se toca el destino ni el SPSC y se publica tamaño
///   cero. El buffer se devuelve igual, sin bytes que un consumidor pueda leer
///   como audio del contrato.
/// - Sin plano mapeado: se publica tamaño cero y no se consume el anillo; el
///   `Some` es lo que permite al callback escribir `chunk.size = 0` en vez de
///   reencolar el tamaño del ciclo anterior.
/// - Sugerencia no nula: acota los frames a `min(requested, capacidad)`. La
///   conversión `u64 -> usize` se satura, no desborda. El resto queda en el
///   anillo para el callback siguiente; no se descarta nada.
fn render_process_buffer(
    consumer: &mut SampleConsumer,
    slice: Option<&mut [u8]>,
    requested: u64,
    format_ok: bool,
) -> Option<RenderOutcome> {
    if !format_ok {
        return Some(RenderOutcome::default());
    }
    let Some(slice) = slice else {
        return Some(RenderOutcome::default());
    };
    let stride = BYTES_PER_SAMPLE;
    let capacity_frames = slice.len() / stride;
    // `try_from` falla sólo si `requested` no cabe en `usize`; saturar al
    // máximo deja que el recorte por capacidad haga el trabajo sin desbordar.
    let limit = usize::try_from(requested).unwrap_or(usize::MAX);
    let frames = if requested == 0 {
        capacity_frames
    } else {
        capacity_frames.min(limit)
    };
    Some(consumer.render_pcm16le_into(slice, 0, stride, frames))
}

/// Contadores que el callback incrementa y el propietario lee.
///
/// `AtomicU64` y no `Cell<u64>`: `process` corre en el hilo de datos con
/// `RT_PROCESS` y `counters()` en el hilo propietario, así que compartir celdas
/// no atómicas es una carrera por el modelo de memoria, no un atajo. Son
/// atómicos sin candado: el callback sigue siendo el único escritor y el
/// propietario sólo lee.
///
/// La lectura del propietario es un **snapshot no simultáneo**: lee los tres
/// contadores de a uno, así que puede ver el de buffers ya incrementado y el de
/// frames todavía no. No se promete coherencia entre los tres; cada uno es
/// monótono por separado.
#[derive(Debug, Default)]
struct Counters {
    frames: AtomicU64,
    underrun_frames: AtomicU64,
    buffers: AtomicU64,
}

/// Estado que ve el callback de proceso.
///
/// Consumidor del anillo y dos `Arc` preasignados en `open`: nada que asignar,
/// liberar ni bloquear en el camino RT. Los `Arc` no se clonan ni se sueltan
/// dentro del callback; el `Rc`/`Arc` del trampolín `FnMut` lo gestiona el
/// binding fuera de `process`.
struct CallbackState {
    consumer: SampleConsumer,
    counters: Arc<Counters>,
    format: Arc<AtomicU64>,
}

/// Fuente PipeWire lista pero inactiva.
///
/// El tipo no es `Send`: PipeWire usa `Rc` internamente y el bucle principal
/// pertenece al hilo que lo creó.
pub struct PipeWireSource {
    _mainloop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    _core: pw::core::CoreRc,
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<CallbackState>,
    producer: SampleProducer,
    format_bytes: Vec<u8>,
    counters: Arc<Counters>,
    negotiated: Arc<AtomicU64>,
    active: bool,
}

impl PipeWireSource {
    /// Construye la fuente y la deja **inactiva**.
    ///
    /// Toda la asignación (bucle, contexto, core, stream, pod de formato, anillo
    /// SPSC) ocurre aquí, fuera del callback. No se llama a `pw::init` desde
    /// aquí: el propietario debe invocar [`init`] una vez por proceso.
    ///
    /// `ring_capacity` es la capacidad fija del SPSC en muestras.
    pub fn open(ring_capacity: usize) -> Result<Self, SourceError> {
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        let properties = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::MEDIA_CLASS => MEDIA_CLASS,
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => "arl microphone",
        };

        let stream = pw::stream::StreamRc::new(core.clone(), "arl-mic", properties)?;
        let (producer, consumer) = ring(ring_capacity);

        let counters = Arc::new(Counters::default());
        let negotiated = Arc::new(AtomicU64::new(NegotiatedFormat::UNSET.packed()));

        let state = CallbackState {
            consumer,
            counters: Arc::clone(&counters),
            format: Arc::clone(&negotiated),
        };

        let listener = stream
            .add_local_listener_with_user_data(state)
            .param_changed(|_stream, state, id, param| {
                apply_format_update(&state.format, format_update(id, param));
            })
            .process(|stream, state| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    // Sin buffer disponible: PipeWire ya lo reporta; en RT no hay
                    // nada que copiar ni que registrar.
                    return;
                };
                state.counters.add_buffers(1);
                let requested = buffer.requested();
                let format_ok = read_negotiated(&state.format).matches_contract();
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let stride = BYTES_PER_SAMPLE;
                // `chunk_mut` afirma que el puntero no es nulo y paniquearía en
                // RT; se comprueba antes con la vista cruda. Sin chunk no hay
                // dónde declarar lo escrito, así que tampoco se consume el
                // anillo: las muestras quedan para el callback siguiente.
                if data.as_raw().chunk.is_null() {
                    return;
                }
                // La función es total tras el arreglo: `Some` en todos los
                // caminos, con resultado vacío cuando no hay formato conforme o
                // no hay plano mapeado. Nunca deja el tamaño anterior.
                let outcome =
                    render_process_buffer(&mut state.consumer, data.data(), requested, format_ok)
                        .unwrap_or_default();
                state.counters.add_frames(outcome.frames);
                state.counters.add_underrun_frames(outcome.underrun_frames);

                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = (outcome.frames * stride) as u32;
                // `buffer` se devuelve al stream al salir del scope (Drop).
            })
            .register()?;

        let format_bytes = serialize_contract_format()?;
        let mut params = [Pod::from_bytes(&format_bytes).ok_or(SourceError::FormatPod)?];

        stream.connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::INACTIVE
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS
                | pw::stream::StreamFlags::AUTOCONNECT,
            &mut params,
        )?;

        Ok(Self {
            _mainloop: mainloop,
            _context: context,
            _core: core,
            stream,
            _listener: listener,
            producer,
            format_bytes,
            counters,
            negotiated,
            active: false,
        })
    }

    /// Productor del anillo, para que el propietario empuje muestras.
    pub fn producer(&mut self) -> &mut SampleProducer {
        &mut self.producer
    }

    /// Formato confirmado por el servidor (`UNSET` hasta el primer
    /// `param_changed`).
    ///
    /// Es una lectura `Acquire` del formato empaquetado: ve una negociación
    /// completa o la anterior, nunca una mezcla de campos.
    pub fn negotiated(&self) -> NegotiatedFormat {
        read_negotiated(&self.negotiated)
    }

    /// Bytes del pod de formato negociado, vivos mientras viva la fuente.
    pub fn format_pod_bytes(&self) -> &[u8] {
        &self.format_bytes
    }

    /// `true` si la fuente ya fue activada explícitamente.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Identificador del nodo en el servidor (`u32::MAX` si aún no registrado).
    ///
    /// Sirve al propietario para inspeccionar o enlazar el nodo por id sin
    /// adivinar nombres de puerto.
    pub fn node_id(&self) -> u32 {
        self.stream.node_id()
    }

    /// Contadores observados por el callback: (buffers, frames, underrun).
    ///
    /// Tres lecturas `Relaxed` independientes: es un snapshot **no
    /// simultáneo**, cada contador es monótono pero la tupla puede mezclar dos
    /// instantes. El formato sí es una lectura coherente.
    pub fn counters(&self) -> (u64, u64, u64) {
        (
            self.counters.buffers.load(Ordering::Relaxed),
            self.counters.frames.load(Ordering::Relaxed),
            self.counters.underrun_frames.load(Ordering::Relaxed),
        )
    }

    /// Activa la entrega de muestras. Requiere decisión explícita del
    /// propietario autorizado: construir la fuente nunca la publica activa.
    pub fn activate(&mut self) -> Result<(), SourceError> {
        self.stream.set_active(true)?;
        self.active = true;
        Ok(())
    }

    /// Vuelve a pausar la fuente sin desmontarla.
    pub fn deactivate(&mut self) -> Result<(), SourceError> {
        self.stream.set_active(false)?;
        self.active = false;
        Ok(())
    }

    /// Corre el bucle principal hasta que el propietario lo detenga.
    ///
    /// Bloquea el hilo llamante; en T11a no se invoca desde ningún binario.
    pub fn run(&self) {
        self._mainloop.run();
    }

    /// Avanza el bucle principal como mucho `timeout` y devuelve cuántas fuentes
    /// despachó.
    ///
    /// Variante **acotada** de [`Self::run`] para un propietario que conduce el
    /// ciclo de vida desde el mismo hilo (no bloquea indefinidamente). No cambia
    /// el estado del stream ni el camino RT: sólo es el `iterate` del bucle.
    #[must_use]
    pub fn iterate(&self, timeout: std::time::Duration) -> i32 {
        self._mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(timeout))
    }

    /// Pide al bucle principal que termine.
    pub fn quit(&self) {
        self._mainloop.quit();
    }

    /// Desconecta el stream. La limpieza ocurre fuera del callback RT.
    pub fn disconnect(&mut self) -> Result<(), SourceError> {
        self.active = false;
        self.stream.disconnect()?;
        Ok(())
    }
}

/// Serializa la descripción exacta del perfil del contrato: S16LE mono 48 kHz.
fn serialize_contract_format() -> Result<Vec<u8>, SourceError> {
    let mut info = AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(SAMPLE_RATE_HZ);
    info.set_channels(CHANNEL_COUNT);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);

    let bytes = PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|_| SourceError::FormatPod)?
    .0
    .into_inner();

    Ok(bytes)
}

impl Counters {
    /// Único escritor: el callback. `saturating_add` acota en `u64::MAX` en vez
    /// de envolver, y al no haber contendientes no hace falta un bucle CAS.
    fn add_buffers(&self, delta: u64) {
        let next = self.buffers.load(Ordering::Relaxed).saturating_add(delta);
        self.buffers.store(next, Ordering::Release);
    }

    fn add_frames(&self, delta: usize) {
        let next = self
            .frames
            .load(Ordering::Relaxed)
            .saturating_add(delta as u64);
        self.frames.store(next, Ordering::Release);
    }

    fn add_underrun_frames(&self, delta: usize) {
        let next = self
            .underrun_frames
            .load(Ordering::Relaxed)
            .saturating_add(delta as u64);
        self.underrun_frames.store(next, Ordering::Release);
    }
}

/// Inicializa PipeWire una vez por proceso.
///
/// Debe llamarse antes de [`PipeWireSource::open`].
pub fn init() {
    pw::init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use spa::pod::{Object, Property};
    use spa::utils::Id;

    /// Serializa un formato de audio crudo con el perfil indicado.
    fn audio_raw_pod(format: spa::param::audio::AudioFormat, rate: u32) -> Vec<u8> {
        let mut info = AudioInfoRaw::new();
        info.set_format(format);
        info.set_rate(rate);
        info.set_channels(CHANNEL_COUNT);
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
        info.set_position(position);
        serialize_object(info.into())
    }

    /// Serializa un objeto `Format` con las propiedades dadas.
    fn serialize_object(properties: Vec<Property>) -> Vec<u8> {
        PodSerializer::serialize(
            Cursor::new(Vec::new()),
            &Value::Object(Object {
                type_: spa::sys::SPA_TYPE_OBJECT_Format,
                id: spa::sys::SPA_PARAM_EnumFormat,
                properties,
            }),
        )
        .expect("el pod de prueba serializa")
        .0
        .into_inner()
    }

    /// Formato de vídeo crudo: cabecera `Format` válida, medio no Audio.
    fn video_raw_pod() -> Vec<u8> {
        serialize_object(vec![
            Property::new(
                spa::sys::SPA_FORMAT_mediaType,
                Value::Id(Id(spa::sys::SPA_MEDIA_TYPE_video)),
            ),
            Property::new(
                spa::sys::SPA_FORMAT_mediaSubtype,
                Value::Id(Id(spa::sys::SPA_MEDIA_SUBTYPE_raw)),
            ),
        ])
    }

    fn as_pod(bytes: &[u8]) -> &Pod {
        Pod::from_bytes(bytes).expect("bytes del pod son un pod completo")
    }

    fn push_all(producer: &mut SampleProducer, samples: &[i16]) {
        let outcome = producer.push_slice(samples);
        assert_eq!(outcome.dropped, 0, "capacidad suficiente en el test");
        assert_eq!(outcome.written, samples.len());
    }

    // Caso 1: sugerencia `requested` no nula.

    /// RED: la sugerencia no nula debe acotar las muestras publicadas y dejar
    /// el resto en FIFO para el callback siguiente. Hoy el helper copia todo
    /// el plano y vacía el anillo.
    #[test]
    fn requested_no_nulo_acota_frames_y_deja_resto_fifo() {
        let (mut producer, mut consumer) = ring(8);
        push_all(&mut producer, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut dst = vec![0u8; 8 * BYTES_PER_SAMPLE];

        let outcome = render_process_buffer(&mut consumer, Some(&mut dst), 4, true)
            .expect("con plano mapeado el paso publica");

        assert_eq!(outcome.frames, 4, "requested=4 con 8 frames en el plano");
        assert_eq!(outcome.underrun_frames, 0);
        assert_eq!(
            consumer.available(),
            4,
            "el resto queda para el siguiente callback"
        );

        let mut next = vec![0u8; 4 * BYTES_PER_SAMPLE];
        let again = render_process_buffer(&mut consumer, Some(&mut next), 4, true)
            .expect("segundo callback publica");
        assert_eq!(again.frames, 4);
        for i in 0..4 {
            let value = i16::from_le_bytes([next[i * 2], next[i * 2 + 1]]);
            assert_eq!(value, 5 + i as i16, "FIFO conservado en muestra {i}");
        }
    }

    /// Positivo: `requested == 0` es ausencia de sugerencia; se copia el plano
    /// completo sin descartar muestras.
    #[test]
    fn requested_cero_copia_el_plano_completo() {
        let (mut producer, mut consumer) = ring(8);
        push_all(&mut producer, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut dst = vec![0u8; 8 * BYTES_PER_SAMPLE];

        let outcome = render_process_buffer(&mut consumer, Some(&mut dst), 0, true)
            .expect("con plano mapeado el paso publica");

        assert_eq!(outcome.frames, 8);
        assert_eq!(outcome.underrun_frames, 0);
        assert_eq!(consumer.available(), 0);
        for i in 0..8 {
            let value = i16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
            assert_eq!(value, 1 + i as i16, "muestra {i} en orden");
        }
    }

    /// Positivo: una sugerencia mayor que el plano no añade frames ni deja
    /// huecos; el límite efectivo es el plano.
    #[test]
    fn requested_mayor_que_el_plano_no_crece() {
        let (mut producer, mut consumer) = ring(8);
        push_all(&mut producer, &[1, 2, 3, 4]);
        let mut dst = vec![0u8; 4 * BYTES_PER_SAMPLE];

        let outcome = render_process_buffer(&mut consumer, Some(&mut dst), 64, true)
            .expect("con plano mapeado el paso publica");

        assert_eq!(outcome.frames, 4, "no se publica más de lo que cabe");
        assert_eq!(outcome.underrun_frames, 0);
        assert_eq!(consumer.available(), 0);
    }

    // Caso 2: formato no válido en el paso de copia.

    /// RED: con formato no conforme no debe haber bytes válidos ni consumo del
    /// SPSC. Hoy el helper ignora `format_ok` y copia igual.
    #[test]
    fn formato_no_conforme_publica_cero_y_no_consume() {
        let (mut producer, mut consumer) = ring(8);
        push_all(&mut producer, &[1, 2, 3]);
        let mut dst = vec![0x11u8; 3 * BYTES_PER_SAMPLE];

        let outcome = render_process_buffer(&mut consumer, Some(&mut dst), 0, false)
            .expect("el buffer se devuelve igual");

        assert_eq!(outcome.frames, 0, "formato no conforme no publica muestras");
        assert_eq!(outcome.underrun_frames, 0);
        assert_eq!(consumer.available(), 3, "no se consume el SPSC");
        assert!(dst.iter().all(|&b| b == 0x11), "el destino no se toca");
    }

    /// Positivo: formato conforme copia y entrega en orden.
    #[test]
    fn formato_conforme_copia_y_entrega() {
        let (mut producer, mut consumer) = ring(8);
        push_all(&mut producer, &[7, 8, 9]);
        let mut dst = vec![0u8; 3 * BYTES_PER_SAMPLE];

        let outcome = render_process_buffer(&mut consumer, Some(&mut dst), 0, true)
            .expect("con plano mapeado el paso publica");

        assert_eq!(outcome.frames, 3);
        assert_eq!(outcome.underrun_frames, 0);
        assert_eq!(consumer.available(), 0);
        for i in 0..3 {
            let value = i16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
            assert_eq!(value, 7 + i as i16, "muestra {i} en orden");
        }
    }

    // Caso 3: `param_changed` con pods serializados reales.

    /// Positivo: un `Format` Audio/Raw interpretable fija el formato exacto.
    #[test]
    fn param_format_audio_raw_fija_el_formato() {
        let bytes = audio_raw_pod(spa::param::audio::AudioFormat::S16LE, SAMPLE_RATE_HZ);
        let update = format_update(spa::param::ParamType::Format.as_raw(), Some(as_pod(&bytes)));

        let FormatUpdate::Set(format) = update else {
            panic!("un Format Audio/Raw válido debe fijar estado, fue {update:?}");
        };
        assert!(format.matches_contract());
        assert_eq!(format.rate, SAMPLE_RATE_HZ);
        assert_eq!(format.channels, CHANNEL_COUNT);
    }

    /// Positivo: otro parámetro o un `Format` ausente con id ajeno no toca el
    /// estado anterior.
    #[test]
    fn param_ajeno_conserva_el_formato_anterior() {
        let bytes = audio_raw_pod(spa::param::audio::AudioFormat::S16LE, SAMPLE_RATE_HZ);
        let other = spa::param::ParamType::Props.as_raw();

        assert_eq!(
            format_update(other, Some(as_pod(&bytes))),
            FormatUpdate::Keep
        );
        assert_eq!(format_update(other, None), FormatUpdate::Keep);
    }

    /// Positivo: `Format` sin pod invalida el estado negociado.
    #[test]
    fn param_format_ausente_invalida_el_estado() {
        assert_eq!(
            format_update(spa::param::ParamType::Format.as_raw(), None),
            FormatUpdate::Reset
        );
    }

    /// RED: un `Format` malformado tras una negociación válida debe invalidar
    /// el estado; hoy se conserva el formato anterior (`Keep`).
    #[test]
    fn param_format_malformado_invalida_el_estado() {
        let malformed = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Int(7))
            .expect("pod escalar serializa")
            .0
            .into_inner();

        let update = format_update(
            spa::param::ParamType::Format.as_raw(),
            Some(as_pod(&malformed)),
        );

        assert_eq!(
            update,
            FormatUpdate::Reset,
            "un pod que no parsea como Format deja el formato anterior vivo"
        );
    }

    /// RED: un `Format` válido pero no Audio/Raw debe invalidar el estado; hoy
    /// el formato anterior sobrevive.
    #[test]
    fn param_format_no_audio_invalida_el_estado() {
        let bytes = video_raw_pod();

        let update = format_update(spa::param::ParamType::Format.as_raw(), Some(as_pod(&bytes)));

        assert_eq!(
            update,
            FormatUpdate::Reset,
            "Format video/raw conservó el formato de audio anterior"
        );
    }

    /// Positivo: la escritura compartida deja el `Cell` en el formato confirmado.
    #[test]
    fn apply_format_update_escribe_el_cell_negociado() {
        let bytes = audio_raw_pod(spa::param::audio::AudioFormat::S16LE, SAMPLE_RATE_HZ);
        // Adaptación mecánica por la representación atómica: misma semántica que
        // `Cell::new(NegotiatedFormat::UNSET)` + `cell.get()`.
        let cell = AtomicU64::new(NegotiatedFormat::UNSET.packed());

        apply_format_update(
            &cell,
            format_update(spa::param::ParamType::Format.as_raw(), Some(as_pod(&bytes))),
        );

        assert!(
            read_negotiated(&cell).matches_contract(),
            "el Cell refleja el formato real"
        );
    }

    // Caso 4: `Data` sin plano mapeado.

    /// Positivo: sin plano mapeado no se consume el anillo. El valor de
    /// retorno no se fija aquí a propósito: el hueco de tamaño publicado se
    /// cubre aparte.
    #[test]
    fn data_none_no_consume_el_anillo() {
        let (mut producer, mut consumer) = ring(8);
        push_all(&mut producer, &[1, 2, 3]);

        let _step = render_process_buffer(&mut consumer, None, 8, true);

        assert_eq!(consumer.available(), 3, "el anillo queda intacto");
    }

    /// RED: sin plano mapeado el buffer se devuelve igual, así que los
    /// metadatos publicados deben quedar en tamaño 0. Hoy el paso no produce
    /// resultado y el callback deja el `chunk.size` heredado.
    #[test]
    fn data_none_publica_tamano_cero() {
        let (mut producer, mut consumer) = ring(8);
        push_all(&mut producer, &[1, 2, 3]);

        let step = render_process_buffer(&mut consumer, None, 8, true);

        assert_eq!(
            step,
            Some(RenderOutcome {
                frames: 0,
                underrun_frames: 0,
            }),
            "sin plano mapeado el tamaño publicado debe ser 0"
        );
        assert_eq!(consumer.available(), 3);
    }
}
