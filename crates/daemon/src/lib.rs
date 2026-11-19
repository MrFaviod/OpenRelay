//! Daemon mínimo de openrelay: sesión QUIC autorizada -> `AudioFrame` -> jitter ->
//! SPSC de `PipeWireSource`.
//!
//! Una sola función (`serve`) conduce el socket UDP. La autorización la hace
//! `SessionServer` (ACL default-deny de `openrelay-transport`): sólo se leen frames
//! de sesiones que el propio servidor reportó como `Authorized`.
//!
//! Sólo se escriben frames `EmitKind::Network`; PLC y silencio del jitter no se
//! inyectan en el micrófono virtual en esta fase.

pub mod acl;
pub mod control;
pub mod issue;
pub mod pairing;
pub mod provision;
#[cfg(feature = "return-audio")]
pub mod return_audio;
pub mod runtime;
pub mod session;

use std::fmt;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

use openrelay_audio_core::jitter::{EmitKind, JitterBuffer, JitterConfig, PopOutcome};
use openrelay_audio_core::SAMPLE_COUNT;
use openrelay_audio_linux::SampleProducer;
use openrelay_transport::quinn::{ServerEvent, SessionId, SessionServer, TransportError};

/// Tick de lectura del socket: acota la latencia de bombeo del jitter.
const READ_TICK: Duration = Duration::from_millis(1);
/// Target del jitter buffer en ms (contrato: 5..=40).
const TARGET_MS: u16 = 40;
/// Offset y primo de FNV-1a 64 bits para la huella de muestras escritas.
pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Error del daemon: socket, transporte o control.
#[derive(Debug)]
pub enum DaemonError {
    Socket(std::io::Error),
    Transport(TransportError),
    Control(control::ControlError),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(error) => write!(f, "socket UDP: {error}"),
            Self::Transport(error) => write!(f, "transporte: {error}"),
            Self::Control(error) => write!(f, "control: {error}"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<std::io::Error> for DaemonError {
    fn from(error: std::io::Error) -> Self {
        Self::Socket(error)
    }
}

impl From<TransportError> for DaemonError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<control::ControlError> for DaemonError {
    fn from(error: control::ControlError) -> Self {
        Self::Control(error)
    }
}

/// Contadores de una ejecución de [`serve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeStats {
    pub sessions_authorized: u32,
    pub sessions_rejected: u32,
    pub frames_received: u64,
    pub frames_written: u64,
    pub samples_written: u64,
    /// Muestras que el anillo no aceptó (desbordamiento).
    pub samples_dropped: u64,
    /// FNV-1a 64 sobre las muestras escritas, en orden de escritura.
    pub checksum: u64,
}

/// Extiende una huella FNV-1a con muestras PCM16 en orden LE.
pub fn fnv1a_extend(mut hash: u64, samples: &[i16]) -> u64 {
    for sample in samples {
        for byte in sample.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

/// Conduce `server` sobre `socket` hasta `until`, escribiendo en `producer`.
///
/// Igual que [`serve_with_control`] sin control: sólo audio.
pub fn serve(
    socket: &UdpSocket,
    server: &mut SessionServer,
    producer: &mut SampleProducer,
    until: Instant,
) -> Result<ServeStats, DaemonError> {
    serve_with_control(socket, server, producer, until, None)
}

/// Conduce audio y, si hay `control`, el control de las sesiones autorizadas.
///
/// El control usa el mismo socket y las mismas sesiones. `poll` lee lo que
/// `recv_control` ya entregó y responde con `send_control`. Sin `control` el
/// comportamiento es el de [`serve`].
pub fn serve_with_control(
    socket: &UdpSocket,
    server: &mut SessionServer,
    producer: &mut SampleProducer,
    until: Instant,
    mut control: Option<&mut control::SharedControl>,
) -> Result<ServeStats, DaemonError> {
    socket.set_read_timeout(Some(READ_TICK))?;
    let origin = Instant::now();
    let mut jitter = JitterBuffer::new(JitterConfig::new(TARGET_MS));
    let mut sessions: Vec<SessionId> = Vec::new();
    let mut stats = ServeStats {
        sessions_authorized: 0,
        sessions_rejected: 0,
        frames_received: 0,
        frames_written: 0,
        samples_written: 0,
        samples_dropped: 0,
        checksum: FNV_OFFSET,
    };
    let mut buf = [0u8; 2048];

    while Instant::now() < until {
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                let _ = server.handle_datagram(Instant::now(), from, &buf[..len]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }

        let now = Instant::now();
        server.handle_timeout(now)?;
        while let Some((to, bytes)) = server.poll_transmit(now) {
            socket.send_to(&bytes, to)?;
        }

        while let Some(event) = server.next_event() {
            match event {
                ServerEvent::Authorized { session, .. } => {
                    stats.sessions_authorized += 1;
                    sessions.push(session);
                }
                ServerEvent::Rejected { .. } => stats.sessions_rejected += 1,
                ServerEvent::Closed { session } => {
                    sessions.retain(|s| *s != session);
                }
            }
        }

        if let Some(control) = control.as_mut() {
            control.poll(server, &sessions)?;
            while let Some((to, bytes)) = server.poll_transmit(now) {
                socket.send_to(&bytes, to)?;
            }
        }

        let now_us = now.duration_since(origin).as_micros() as u64;
        for &session in &sessions {
            while let Some(frame) = server.recv_audio(session) {
                stats.frames_received += 1;
                jitter.push(frame, now_us);
            }
        }

        while let PopOutcome::Emitted(emit) = jitter.pop(now_us) {
            if emit.kind != EmitKind::Network {
                continue;
            }
            let samples = emit.frame.samples();
            let outcome = producer.push_slice(&samples[..]);
            stats.samples_written += outcome.written as u64;
            stats.checksum = fnv1a_extend(stats.checksum, &samples[..outcome.written]);
            if outcome.written == SAMPLE_COUNT {
                stats.frames_written += 1;
            }
        }
    }

    stats.samples_dropped = producer.dropped();
    Ok(stats)
}
