//! Fallback PulseAudio por proceso, sin bindings de `libpulse`.
//!
//! Cuando el servidor de la sesión **no** es PipeWire, arl puede publicar una
//! fuente de captura con dos módulos de PulseAudio: un sink nulo propio y un
//! `module-remap-source` que expone su monitor como micrófono. Este módulo
//! ejecuta `pactl` como proceso hijo; no enlaza `libpulse` ni ninguna otra
//! biblioteca del sistema.
//!
//! La regla de propiedad es estricta: sólo se descargan los índices que
//! devolvieron las cargas de esta instancia. Nunca se enumeran ni se descargan
//! módulos ajenos, ni siquiera si ocupan el mismo nombre de sink o de fuente.
//! La descarga ocurre en [`PulseFallback::close`] y, si el consumidor olvida
//! cerrar, en `Drop`.
//!
//! PipeWire sigue siendo la ruta principal: [`PulseFallback::activate`] no crea
//! módulos cuando `pactl info` identifica PipeWire (o un servidor desconocido).

use std::ffi::OsString;
use std::fmt;
use std::process::Command;

use arl_audio_core::{CHANNELS, SAMPLE_RATE_HZ};

/// Nombre del sink nulo que crea el fallback.
pub const SINK_NAME: &str = "arl-fallback-sink";
/// Nombre de la fuente remapeada que expone el fallback.
pub const SOURCE_NAME: &str = "arl-fallback-mic";
/// Módulo de PulseAudio que crea el sink nulo.
pub const MODULE_NULL_SINK: &str = "module-null-sink";
/// Módulo de PulseAudio que remapea el monitor del sink a una fuente.
pub const MODULE_REMAP_SOURCE: &str = "module-remap-source";

/// Error del fallback PulseAudio.
#[derive(Debug)]
pub enum PulseError {
    /// No se pudo lanzar `pactl` (por ejemplo, no está instalado).
    Spawn(std::io::Error),
    /// `pactl` terminó con un código distinto de cero.
    Exit {
        /// Argumentos con los que se invocó `pactl`.
        args: String,
        /// Código de salida, si el proceso lo informó.
        code: Option<i32>,
        /// Error estándar del proceso, sin espacios en los extremos.
        stderr: String,
    },
    /// `pactl` respondió algo que no se puede interpretar.
    Parse {
        /// Operación cuya salida no se pudo interpretar.
        what: &'static str,
        /// Salida recibida, recortada por el propio mensaje.
        output: String,
    },
}

impl fmt::Display for PulseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PulseError::Spawn(e) => write!(f, "no se pudo ejecutar pactl: {e}"),
            PulseError::Exit { args, code, stderr } => {
                let code = code.map_or_else(|| "desconocido".to_string(), |c| c.to_string());
                write!(f, "pactl {args} falló (código {code}): {stderr}")
            }
            PulseError::Parse { what, output } => {
                write!(f, "salida inesperada de pactl en {what}: {output}")
            }
        }
    }
}

impl std::error::Error for PulseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PulseError::Spawn(e) => Some(e),
            PulseError::Exit { .. } | PulseError::Parse { .. } => None,
        }
    }
}

/// Ejecutor mínimo de `pactl`, inyectable en pruebas.
///
/// Una implementación real lanza el binario del sistema; una de prueba puede
/// responder con salidas fijas y registrar las llamadas sin tocar el servidor.
pub trait Pactl {
    /// Ejecuta `pactl` con `args` y devuelve la salida estándar si el código de
    /// salida es cero.
    fn run(&mut self, args: &[&str]) -> Result<String, PulseError>;
}

/// Ejecutor real: lanza el binario `pactl` del sistema.
#[derive(Debug, Clone, Default)]
pub struct PactlProcess {
    pulse_server: Option<OsString>,
}

impl PactlProcess {
    /// Ejecutor que hereda el entorno del proceso.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ejecutor que fija `PULSE_SERVER`, útil para una instancia privada.
    pub fn with_pulse_server(server: impl Into<OsString>) -> Self {
        Self {
            pulse_server: Some(server.into()),
        }
    }
}

impl Pactl for PactlProcess {
    fn run(&mut self, args: &[&str]) -> Result<String, PulseError> {
        let mut command = Command::new("pactl");
        command.args(args);
        if let Some(server) = &self.pulse_server {
            command.env("PULSE_SERVER", server);
        }
        let output = command.output().map_err(PulseError::Spawn)?;
        if !output.status.success() {
            return Err(PulseError::Exit {
                args: args.join(" "),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Servidor de sonido que atiende la sesión según `pactl info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    /// PipeWire con su servidor PulseAudio compatible.
    PipeWire,
    /// PulseAudio nativo.
    PulseAudio,
    /// Servidor no identificado (no se toca).
    Unknown,
}

/// Pregunta a `pactl info` qué servidor atiende la sesión.
pub fn detect_server(pactl: &mut impl Pactl) -> Result<ServerKind, PulseError> {
    let output = pactl.run(&["info"])?;
    let name = output
        .lines()
        .find_map(|line| line.strip_prefix("Server Name:"))
        .map(str::trim)
        .ok_or_else(|| PulseError::Parse {
            what: "pactl info",
            output: output.clone(),
        })?;
    let name = name.to_lowercase();
    if name.contains("pipewire") {
        Ok(ServerKind::PipeWire)
    } else if name.contains("pulseaudio") {
        Ok(ServerKind::PulseAudio)
    } else {
        Ok(ServerKind::Unknown)
    }
}

/// Fallback activo: sink nulo y fuente remapeada propios.
///
/// El ejecutor se guarda junto con los índices porque `Drop` necesita descargar
/// exactamente esos módulos. El tipo es genérico sólo para inyectar el ejecutor:
/// en producción se usa [`PactlProcess`].
#[derive(Debug)]
pub struct PulseFallback<P: Pactl> {
    pactl: P,
    modules: Vec<u32>,
}

impl<P: Pactl> PulseFallback<P> {
    /// Activa el fallback si y sólo si el servidor es PulseAudio nativo.
    ///
    /// Devuelve `Ok(None)` cuando `pactl info` identifica PipeWire o un
    /// servidor desconocido: en ese caso no se ejecuta ninguna carga.
    pub fn activate(mut pactl: P) -> Result<Option<Self>, PulseError> {
        if detect_server(&mut pactl)? != ServerKind::PulseAudio {
            return Ok(None);
        }

        let rate = format!("rate={SAMPLE_RATE_HZ}");
        let channels = format!("channels={CHANNELS}");
        let sink_args = [
            "load-module",
            MODULE_NULL_SINK,
            &format!("sink_name={SINK_NAME}"),
            &rate,
            &channels,
        ];
        let sink_output = pactl.run(&sink_args)?;
        let sink_id = parse_index(&sink_output, "load-module module-null-sink")?;

        let master = format!("master={SINK_NAME}.monitor");
        let source = format!("source_name={SOURCE_NAME}");
        let source_args = ["load-module", MODULE_REMAP_SOURCE, &master, &source];
        match pactl
            .run(&source_args)
            .and_then(|out| parse_index(&out, "load-module module-remap-source"))
        {
            Ok(source_id) => Ok(Some(Self {
                pactl,
                modules: vec![sink_id, source_id],
            })),
            Err(error) => {
                let sink_id = sink_id.to_string();
                let _ = pactl.run(&["unload-module", &sink_id]);
                Err(error)
            }
        }
    }

    /// Índices de módulo creados por esta instancia, en orden de carga.
    pub fn module_ids(&self) -> &[u32] {
        &self.modules
    }

    /// Nombre de la fuente remapeada que publica el fallback.
    pub fn source_name(&self) -> &'static str {
        SOURCE_NAME
    }

    /// Descarga los módulos propios y consume el fallback.
    ///
    /// Se intentan todos los índices aunque uno falle; se devuelve el primer
    /// error. Los índices se descartan antes de descargar para que `Drop` no
    /// repita una descarga ya intentada.
    pub fn close(mut self) -> Result<(), PulseError> {
        self.unload_owned()
    }

    fn unload_owned(&mut self) -> Result<(), PulseError> {
        let mut first_error = None;
        for id in self.modules.drain(..).rev() {
            let id = id.to_string();
            if let Err(error) = self.pactl.run(&["unload-module", &id]) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<P: Pactl> Drop for PulseFallback<P> {
    fn drop(&mut self) {
        let _ = self.unload_owned();
    }
}

/// Interpreta el índice que imprime `pactl load-module`.
fn parse_index(output: &str, what: &'static str) -> Result<u32, PulseError> {
    output.trim().parse::<u32>().map_err(|_| PulseError::Parse {
        what,
        output: output.to_string(),
    })
}
