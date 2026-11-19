//! Captura del monitor de la salida por defecto de PipeWire.
//!
//! Es la dirección inversa de [`crate::pipewire_source`]: en vez de publicar un
//! nodo `Audio/Source` del que otros consumen, este stream **captura** lo que
//! suena en el sink por defecto. Se declara con
//! `stream.capture.sink = true`, que en PipeWire enlaza el stream con el
//! monitor del sink por defecto en vez de con una fuente física.
//!
//! Entrega PCM16LE mono 48 kHz, igual que el resto del contrato. El callback de
//! proceso sólo toma el buffer, copia sus muestras al SPSC y devuelve el buffer;
//! no asigna, no registra, no duerme y no reconfigura el stream.
//!
//! El stream se crea **inactivo**: no entrega muestras hasta que el propietario
//! llame a [`MonitorSource::activate`]. Sin sesión de PipeWire en el host,
//! [`MonitorSource::open`] devuelve error; no entra en pánico.

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
use std::sync::{Arc, Once};
use std::time::Duration;

use crate::ring::{ring, SampleConsumer, SampleProducer};

/// Nombre de nodo del stream de captura del monitor.
pub const NODE_NAME: &str = "arl-return";
/// `media.class` de un stream de captura de audio.
pub const MEDIA_CLASS: &str = "Stream/Input/Audio";
/// Canales del perfil v1.
pub const CHANNEL_COUNT: u32 = CHANNELS as u32;

/// Errores de construcción del backend.
#[derive(Debug)]
pub enum MonitorError {
    /// PipeWire rechazó la creación del bucle, contexto, core o stream.
    PipeWire(pw::Error),
    /// La descripción del formato no serializó a un pod SPA válido.
    FormatPod,
}

impl std::fmt::Display for MonitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MonitorError::PipeWire(error) => write!(f, "pipewire: {error}"),
            MonitorError::FormatPod => {
                write!(f, "no se pudo serializar el formato S16LE mono 48k")
            }
        }
    }
}

impl std::error::Error for MonitorError {}

impl From<pw::Error> for MonitorError {
    fn from(value: pw::Error) -> Self {
        MonitorError::PipeWire(value)
    }
}

/// Formato que el servidor negoció para el monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorFormat {
    /// Frecuencia en Hz, `0` si aún no se negoció.
    pub rate: u32,
    /// Canales, `0` si aún no se negoció.
    pub channels: u32,
    /// `true` cuando el formato es exactamente S16LE.
    pub s16le: bool,
}

impl MonitorFormat {
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

    /// Empaqueta el formato en una sola palabra `u64` para publicarlo con una
    /// única operación atómica.
    fn packed(self) -> u64 {
        (self.rate as u64)
            | (((self.channels & CHANNEL_BITS) as u64) << 32)
            | ((self.s16le as u64) << 63)
    }

    /// Deshace [`MonitorFormat::packed`].
    fn from_packed(packed: u64) -> Self {
        Self {
            rate: packed as u32,
            channels: ((packed >> 32) & CHANNEL_BITS as u64) as u32,
            s16le: (packed >> 63) != 0,
        }
    }
}

/// Máscara de canales dentro de la palabra empaquetada de [`MonitorFormat`].
const CHANNEL_BITS: u32 = 0x7fff_ffff;

/// Interpreta un `Format` Audio/Raw y deja `UNSET` si no es interpretable.
///
/// Un formato viejo sobreviviente se publicaría como audio del contrato, así
/// que un `Format` presente y malformado invalida el estado en vez de
/// conservarlo.
fn negotiated_from_param(param: Option<&Pod>) -> MonitorFormat {
    let Some(param) = param else {
        return MonitorFormat::UNSET;
    };
    let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
        return MonitorFormat::UNSET;
    };
    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
        return MonitorFormat::UNSET;
    }
    let mut info = AudioInfoRaw::new();
    if info.parse(param).is_err() {
        return MonitorFormat::UNSET;
    }
    MonitorFormat {
        rate: info.rate(),
        channels: info.channels(),
        s16le: info.format() == spa::param::audio::AudioFormat::S16LE,
    }
}

/// Contadores que el callback incrementa y el propietario lee.
#[derive(Debug, Default)]
struct Counters {
    frames: AtomicU64,
    dropped: AtomicU64,
    buffers: AtomicU64,
}

/// Estado que ve el callback de proceso: productor del anillo, contadores y
/// formato negociado, todo preasignado en `open`.
struct CallbackState {
    producer: SampleProducer,
    counters: Arc<Counters>,
    format: Arc<AtomicU64>,
}

/// Stream de captura del monitor de la salida por defecto, listo pero inactivo.
///
/// El tipo no es `Send`: PipeWire usa `Rc` internamente y el bucle principal
/// pertenece al hilo que lo creó.
pub struct MonitorSource {
    _mainloop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    _core: pw::core::CoreRc,
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<CallbackState>,
    consumer: SampleConsumer,
    format_bytes: Vec<u8>,
    counters: Arc<Counters>,
    negotiated: Arc<AtomicU64>,
    active: bool,
    scratch: Vec<u8>,
}

impl MonitorSource {
    /// Construye el stream de captura y lo deja **inactivo**.
    ///
    /// Sin un servidor PipeWire alcanzable, la conexión del contexto devuelve
    /// error y no se construye el tipo. `ring_capacity` es la capacidad fija
    /// del SPSC en muestras.
    pub fn open(ring_capacity: usize) -> Result<Self, MonitorError> {
        static INIT: Once = Once::new();
        INIT.call_once(pw::init);

        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        let properties = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::MEDIA_CLASS => MEDIA_CLASS,
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => "arl return monitor",
            // Captura el monitor del sink por defecto (no una fuente física).
            *pw::keys::STREAM_CAPTURE_SINK => "true",
        };

        let stream = pw::stream::StreamRc::new(core.clone(), "arl-return", properties)?;
        let (producer, consumer) = ring(ring_capacity);

        let counters = Arc::new(Counters::default());
        let negotiated = Arc::new(AtomicU64::new(MonitorFormat::UNSET.packed()));
        let state = CallbackState {
            producer,
            counters: Arc::clone(&counters),
            format: Arc::clone(&negotiated),
        };

        let listener = stream
            .add_local_listener_with_user_data(state)
            .param_changed(|_stream, state, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let negotiated = negotiated_from_param(param);
                state.format.store(negotiated.packed(), Ordering::Release);
            })
            .process(|stream, state| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    // Sin buffer disponible: PipeWire ya lo reporta y en RT no
                    // hay nada que copiar ni que registrar.
                    return;
                };
                state.counters.add_buffers(1);
                let format_ok = MonitorFormat::from_packed(state.format.load(Ordering::Acquire))
                    .matches_contract();
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                // `chunk` afirma que el puntero no es nulo y paniquearía en
                // RT; se comprueba antes con la vista cruda.
                if data.as_raw().chunk.is_null() {
                    return;
                }
                let chunk_size = data.chunk().size() as usize;
                let offset = data.chunk().offset() as usize;
                let stride = match data.chunk().stride() {
                    stride if stride > 0 => stride as usize,
                    _ => BYTES_PER_SAMPLE,
                };
                let Some(slice) = data.data() else {
                    return;
                };
                if !format_ok {
                    return;
                }
                let frames =
                    (chunk_size / stride).min((slice.len() - offset.min(slice.len())) / stride);
                if frames == 0 {
                    return;
                }
                copy_frames(
                    &mut state.producer,
                    state.counters.as_ref(),
                    slice,
                    offset,
                    stride,
                    frames,
                );
                // `buffer` se devuelve al stream al salir del scope (Drop).
            })
            .register()?;

        let format_bytes = serialize_contract_format()?;
        let mut params = [Pod::from_bytes(&format_bytes).ok_or(MonitorError::FormatPod)?];

        stream.connect(
            spa::utils::Direction::Input,
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
            consumer,
            format_bytes,
            counters,
            negotiated,
            active: false,
            scratch: Vec::new(),
        })
    }

    /// Formato confirmado por el servidor (`UNSET` hasta el primer
    /// `param_changed`).
    pub fn negotiated(&self) -> MonitorFormat {
        MonitorFormat::from_packed(self.negotiated.load(Ordering::Acquire))
    }

    /// Bytes del pod de formato negociado, vivos mientras viva la fuente.
    pub fn format_pod_bytes(&self) -> &[u8] {
        &self.format_bytes
    }

    /// `true` si la captura ya fue activada explícitamente.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Contadores observados por el callback: (buffers, frames, dropped).
    pub fn counters(&self) -> (u64, u64, u64) {
        (
            self.counters.buffers.load(Ordering::Relaxed),
            self.counters.frames.load(Ordering::Relaxed),
            self.counters.dropped.load(Ordering::Relaxed),
        )
    }

    /// Activa la captura. Requiere decisión explícita del propietario.
    pub fn activate(&mut self) -> Result<(), MonitorError> {
        self.stream.set_active(true)?;
        self.active = true;
        Ok(())
    }

    /// Vuelve a pausar la captura sin desmontarla.
    pub fn deactivate(&mut self) -> Result<(), MonitorError> {
        self.stream.set_active(false)?;
        self.active = false;
        Ok(())
    }

    /// Avanza el bucle principal como mucho `timeout` y devuelve cuántas
    /// fuentes despachó.
    #[must_use]
    pub fn iterate(&self, timeout: Duration) -> i32 {
        self._mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(timeout))
    }

    /// Pide al bucle principal que termine.
    pub fn quit(&self) {
        self._mainloop.quit();
    }

    /// Desconecta el stream. La limpieza ocurre fuera del callback RT.
    pub fn disconnect(&mut self) -> Result<(), MonitorError> {
        self.active = false;
        self.stream.disconnect()?;
        Ok(())
    }

    /// Copia hasta `out.len()` muestras ya capturadas del anillo.
    ///
    /// Devuelve cuántas escribió; `0` significa que todavía no hay muestras.
    /// El bucle principal se despacha antes de leer para que la negociación de
    /// formato y los cambios de estado avancen sin bloquear.
    pub fn read_samples(&mut self, out: &mut [i16]) -> usize {
        if !self.active {
            return 0;
        }
        let _ = self.iterate(Duration::ZERO);
        let frames = self.consumer.available().min(out.len());
        if frames == 0 {
            return 0;
        }
        let bytes = frames * BYTES_PER_SAMPLE;
        self.scratch.resize(bytes, 0);
        let outcome = self.consumer.render_pcm16le_into(
            &mut self.scratch[..bytes],
            0,
            BYTES_PER_SAMPLE,
            frames,
        );
        for (index, sample) in out[..outcome.frames].iter_mut().enumerate() {
            let at = index * BYTES_PER_SAMPLE;
            *sample = i16::from_le_bytes([self.scratch[at], self.scratch[at + 1]]);
        }
        outcome.frames
    }
}

/// Copia `frames` muestras del plano mapeado al SPSC en tandas acotadas.
///
/// El array vive en la pila del callback: nada que asignar ni liberar en el
/// camino RT. Lo que el anillo no acepta se cuenta como descartado.
fn copy_frames(
    producer: &mut SampleProducer,
    counters: &Counters,
    slice: &[u8],
    offset: usize,
    stride: usize,
    frames: usize,
) {
    let mut scratch = [0i16; 512];
    let mut done = 0;
    while done < frames {
        let take = (frames - done).min(scratch.len());
        let mut valid = 0;
        while valid < take {
            let at = offset + (done + valid) * stride;
            if at + BYTES_PER_SAMPLE > slice.len() {
                break;
            }
            scratch[valid] = i16::from_le_bytes([slice[at], slice[at + 1]]);
            valid += 1;
        }
        if valid == 0 {
            return;
        }
        let outcome = producer.push_slice(&scratch[..valid]);
        counters.add_frames(outcome.written);
        counters.add_dropped(outcome.dropped);
        done += valid;
    }
}

/// Serializa el `EnumFormat` que el stream pide: S16LE, mono, 48 kHz.
fn serialize_contract_format() -> Result<Vec<u8>, MonitorError> {
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
    .map_err(|_| MonitorError::FormatPod)?
    .0
    .into_inner();

    Ok(bytes)
}

impl Counters {
    /// Único escritor: el callback. `saturating_add` acota en vez de envolver.
    fn add_frames(&self, delta: usize) {
        let next = self
            .frames
            .load(Ordering::Relaxed)
            .saturating_add(delta as u64);
        self.frames.store(next, Ordering::Release);
    }

    fn add_dropped(&self, delta: usize) {
        let next = self
            .dropped
            .load(Ordering::Relaxed)
            .saturating_add(delta as u64);
        self.dropped.store(next, Ordering::Release);
    }

    fn add_buffers(&self, delta: u64) {
        let next = self.buffers.load(Ordering::Relaxed).saturating_add(delta);
        self.buffers.store(next, Ordering::Release);
    }
}
