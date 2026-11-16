//! Tickets cortos de un solo uso y revocación de identidades.
//!
//! Un ticket vale para una identidad y una sesión, expira y se consume al
//! presentarse, aunque la comprobación falle. Revocar una identidad invalida
//! sus tickets y la quita del archivo de identidades; el cierre de sesiones
//! vivas lo hace `SessionServer::revoke`.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use arl_transport::identity::SpkiPin;
use arl_transport::quinn::SessionId;
use ring::rand::{SecureRandom, SystemRandom};

use crate::pairing;

/// Motivo por el que un ticket no se canjea.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    /// No existe, ya se canjeó o fue revocado.
    Unknown,
    /// Venció antes de presentarse.
    Expired,
    /// Emitido para otra identidad.
    WrongIdentity,
    /// Emitido para otra sesión.
    WrongSession,
}

impl fmt::Display for TicketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "ticket desconocido o ya usado",
            Self::Expired => "ticket vencido",
            Self::WrongIdentity => "ticket de otra identidad",
            Self::WrongSession => "ticket de otra sesión",
        })
    }
}

impl std::error::Error for TicketError {}

#[derive(Debug, Clone, Copy)]
struct Entry {
    pin: SpkiPin,
    session: SessionId,
    expires: Instant,
}

/// Tickets vivos y auditoría en memoria.
#[derive(Debug, Default)]
pub struct Tickets {
    live: HashMap<[u8; 32], Entry>,
    audit: Vec<String>,
}

impl Tickets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Emite un ticket de 256 bits para `pin` y `session` con vigencia `ttl`.
    pub fn issue(
        &mut self,
        pin: SpkiPin,
        session: SessionId,
        ttl: Duration,
        now: Instant,
    ) -> [u8; 32] {
        let mut ticket = [0u8; 32];
        SystemRandom::new()
            .fill(&mut ticket)
            .expect("aleatorio del sistema disponible");
        self.live.insert(
            ticket,
            Entry {
                pin,
                session,
                expires: now + ttl,
            },
        );
        self.audit.push(audit_line("ticket.issue", &pin, "ok"));
        ticket
    }

    /// Canjea un ticket. Se consume en cualquier resultado.
    pub fn redeem(
        &mut self,
        ticket: &[u8; 32],
        pin: &SpkiPin,
        session: SessionId,
        now: Instant,
    ) -> Result<(), TicketError> {
        let entry = self.live.remove(ticket).ok_or(TicketError::Unknown)?;
        let outcome = if now >= entry.expires {
            Err(TicketError::Expired)
        } else if entry.pin != *pin {
            Err(TicketError::WrongIdentity)
        } else if entry.session != session {
            Err(TicketError::WrongSession)
        } else {
            Ok(())
        };
        let result = if outcome.is_ok() { "ok" } else { "rechazado" };
        self.audit.push(audit_line("ticket.redeem", pin, result));
        outcome
    }

    /// Invalida todos los tickets de una identidad.
    pub fn revoke_identity(&mut self, pin: &SpkiPin) {
        self.live.retain(|_, entry| entry.pin != *pin);
        self.audit.push(audit_line("ticket.revoke", pin, "ok"));
    }

    /// Líneas de auditoría. Pin abreviado; nunca tickets ni claves.
    pub fn audit_lines(&self) -> &[String] {
        &self.audit
    }
}

/// Borra la identidad de `path` y invalida sus tickets.
pub fn revoke(path: &Path, tickets: &mut Tickets, pin: &SpkiPin) -> io::Result<()> {
    pairing::remove_identity(path, pin)?;
    tickets.revoke_identity(pin);
    Ok(())
}

/// Línea de auditoría con el pin abreviado a 12 hex.
fn audit_line(action: &str, pin: &SpkiPin, outcome: &str) -> String {
    let hex = pin.to_hex();
    format!("acl {action} pin={} {outcome}", &hex[..12])
}
