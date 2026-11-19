//! Backend de audio Linux de arl: fuente PipeWire nativa y SPSC de capacidad
//! fija.
//!
//! Este crate no escribe `unsafe` propio: cero bloques `unsafe` en `src/`, así
//! que hereda el `unsafe_code = "forbid"` del workspace sin excepciones. Toda
//! la memoria cruda vive dentro de los bindings de PipeWire
//! (`pipewire`/`libspa`), cuya API segura delega en su propio `unsafe` interno;
//! consumir esa API no relaja el lint de este crate. `deny(unsafe_op_in_unsafe_fn)`
//! queda como red por si alguna revisión futura introduce un `unsafe fn`.
//!
//! Frontera del contrato: el perfil es PCM16LE mono 48 kHz de
//! `arl-audio-core`. Aquí no se redefine el wire ni se añaden métricas de
//! jitter/drift: eso pertenece al core.
//!
//! Estado de T11a: la fuente se construye y se conecta **inactiva**; activarla
//! es una decisión explícita del propietario. El nodo real y la captura
//! end-to-end son T11b.
//!
//! F7 añade dos rutas opcionales, apagadas por defecto y sin `unsafe` propio:
//! el feature `pulse` ejecuta `pactl` por proceso para publicar un sink nulo y
//! una fuente remapeada cuando no hay PipeWire, y el feature `alsa` abre un
//! dispositivo `hw:` sólo si la configuración lo pide. PipeWire sigue siendo la
//! ruta principal.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(feature = "alsa")]
pub mod alsa;
pub mod pipewire_source;
#[cfg(feature = "pulse")]
pub mod pulse;
pub mod ring;

#[cfg(feature = "alsa")]
pub use alsa::{open_from_config, AlsaConfig, AlsaError, AlsaHwSource, HW_PREFIX};
pub use pipewire_source::{
    init, NegotiatedFormat, PipeWireSource, SourceError, CHANNEL_COUNT, MEDIA_CLASS, NODE_NAME,
};
#[cfg(feature = "pulse")]
pub use pulse::{
    detect_server, Pactl, PactlProcess, PulseError, PulseFallback, ServerKind, MODULE_NULL_SINK,
    MODULE_REMAP_SOURCE, SINK_NAME, SOURCE_NAME,
};
pub use ring::{ring, PushOutcome, RenderOutcome, SampleConsumer, SampleProducer};
