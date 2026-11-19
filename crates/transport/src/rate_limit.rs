//! Limitador de tasa de handshakes por IP, puro y con tiempo inyectado.
//!
//! El servidor QUIC lo consulta antes de encolar un handshake nuevo. La
//! estructura no conoce el reloj ni el endpoint: recibe `Instant` en cada
//! llamada, así que los tests fijan ventanas sin dormir. El mapa de IPs está
//! acotado y sólo crece al admitir una IP nueva; una IP que no cabe se rechaza
//! (fail closed) en vez de desplazar a otra.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// IPs distintas recordadas a la vez por el limitador.
pub const MAX_TRACKED_IPS: usize = 256;

/// Ventana fija de intentos de handshake por IP.
#[derive(Debug, Clone)]
pub struct HandshakeRateLimiter {
    window: Duration,
    max_attempts: u32,
    max_entries: usize,
    entries: HashMap<IpAddr, Entry>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    window_start: Instant,
    attempts: u32,
}

impl HandshakeRateLimiter {
    /// Crea el limitador con la ventana, el tope de intentos por IP y el
    /// máximo de IPs recordadas. Un tope de intentos cero deniega todo.
    pub fn new(window: Duration, max_attempts: u32, max_entries: usize) -> Self {
        Self {
            window,
            max_attempts,
            max_entries,
            entries: HashMap::new(),
        }
    }

    /// Anota un intento de handshake de `ip` en `now`.
    ///
    /// Devuelve `false` cuando la IP ya consumió su tope dentro de la ventana
    /// vigente o cuando el mapa está lleno de IPs vivas y `ip` no cabe. Un
    /// intento denegado no alarga ni reinicia la ventana.
    pub fn try_acquire(&mut self, now: Instant, ip: IpAddr) -> bool {
        if self.max_attempts == 0 {
            return false;
        }
        let window = self.window;
        if let Some(entry) = self.entries.get_mut(&ip) {
            if now.saturating_duration_since(entry.window_start) >= window {
                *entry = Entry {
                    window_start: now,
                    attempts: 1,
                };
                return true;
            }
            if entry.attempts >= self.max_attempts {
                return false;
            }
            entry.attempts += 1;
            return true;
        }
        if self.entries.len() >= self.max_entries {
            self.entries
                .retain(|_, entry| now.saturating_duration_since(entry.window_start) < window);
            if self.entries.len() >= self.max_entries {
                return false;
            }
        }
        self.entries.insert(
            ip,
            Entry {
                window_start: now,
                attempts: 1,
            },
        );
        true
    }

    /// IPs recordadas ahora mismo; sólo lo consultan los tests del módulo.
    #[cfg(test)]
    pub fn tracked_ips(&self) -> usize {
        self.entries.len()
    }

    /// Intentos anotados para `ip` en la ventana vigente; sólo tests.
    #[cfg(test)]
    pub fn attempts_for(&self, ip: IpAddr) -> u32 {
        self.entries.get(&ip).map_or(0, |entry| entry.attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::{HandshakeRateLimiter, MAX_TRACKED_IPS};
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    const WINDOW: Duration = Duration::from_secs(10);

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    fn limiter() -> HandshakeRateLimiter {
        HandshakeRateLimiter::new(WINDOW, 3, MAX_TRACKED_IPS)
    }

    /// Positivo: los intentos 1..=tope se admiten dentro de la ventana.
    #[test]
    fn admits_up_to_the_limit_inside_the_window() {
        let base = Instant::now();
        let mut limiter = limiter();
        for attempt in 0..3 {
            assert!(
                limiter.try_acquire(base + Duration::from_millis(attempt), ip(1)),
                "intento {attempt} dentro del tope"
            );
        }
        assert_eq!(limiter.tracked_ips(), 1, "una sola IP recordada");
    }

    /// Negativo: el intento que excede el tope se deniega y no se cuenta.
    #[test]
    fn denies_past_the_limit_and_does_not_count_denials() {
        let base = Instant::now();
        let mut limiter = limiter();
        for _ in 0..3 {
            assert!(limiter.try_acquire(base, ip(1)));
        }
        assert!(!limiter.try_acquire(base, ip(1)), "cuarto intento denegado");
        assert!(
            !limiter.try_acquire(base + Duration::from_secs(9), ip(1)),
            "seguir denegado dentro de la ventana"
        );
    }

    /// Positivo: una ventana nueva reabre el presupuesto de la misma IP.
    #[test]
    fn new_window_reopens_the_budget() {
        let base = Instant::now();
        let mut limiter = limiter();
        for _ in 0..3 {
            assert!(limiter.try_acquire(base, ip(1)));
        }
        assert!(!limiter.try_acquire(base, ip(1)));
        let next = base + WINDOW;
        assert!(limiter.try_acquire(next, ip(1)), "ventana nueva");
        assert!(limiter.try_acquire(next, ip(1)), "segundo de la ventana");
    }

    /// Negativo: dos IPs no comparten presupuesto.
    #[test]
    fn one_ip_exhausted_does_not_affect_another() {
        let base = Instant::now();
        let mut limiter = limiter();
        for _ in 0..3 {
            assert!(limiter.try_acquire(base, ip(1)));
        }
        assert!(!limiter.try_acquire(base, ip(1)));
        assert!(
            limiter.try_acquire(base, ip(2)),
            "la IP 2 conserva su ventana"
        );
    }

    /// El mapa no crece sin cota: cabe una IP nueva sólo al expirar una vieja.
    #[test]
    fn map_is_bounded_and_expires_stale_entries() {
        let base = Instant::now();
        let mut limiter = HandshakeRateLimiter::new(WINDOW, 3, 2);
        assert!(limiter.try_acquire(base, ip(1)));
        assert!(limiter.try_acquire(base, ip(2)));
        assert_eq!(limiter.tracked_ips(), 2);
        assert!(
            !limiter.try_acquire(base + Duration::from_secs(1), ip(3)),
            "con el mapa lleno y entradas vivas, una IP nueva se rechaza"
        );
        assert_eq!(limiter.tracked_ips(), 2, "no crece con la entrada nueva");
        assert!(
            limiter.try_acquire(base + WINDOW, ip(3)),
            "las entradas vencidas se expiran y dejan sitio"
        );
        assert_eq!(limiter.tracked_ips(), 1, "sólo la IP nueva queda");
    }

    /// Un tope de intentos cero deniega siempre y no reserva memoria.
    #[test]
    fn zero_budget_denies_without_tracking() {
        let base = Instant::now();
        let mut limiter = HandshakeRateLimiter::new(WINDOW, 0, MAX_TRACKED_IPS);
        assert!(!limiter.try_acquire(base, ip(1)));
        assert_eq!(limiter.tracked_ips(), 0);
    }
}
