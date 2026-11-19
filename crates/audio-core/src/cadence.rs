//! Limitador de cadencia por sesión para el receptor de audio.
//!
//! Es un token bucket puro: el tiempo entra como parámetro (`now_us` de un
//! reloj monotónico local) y el estado no asigna memoria ni depende del
//! tamaño del datagrama. La tasa y la ráfaga salen de `docs/protocol.md`
//! §8.2: 250 fps sostenidos con 240 muestras y 10 frames de ráfaga; con 120
//! o 480 muestras la tasa escala para conservar 48 000 muestras/s (§8.3).

/// Tasa sostenida del perfil base de 5 ms, en frames por segundo.
pub const SUSTAINED_FPS_5MS: u32 = 250;
/// Ráfaga tolerada por el receptor, en frames.
pub const BURST_FRAMES: u32 = 10;
// Un frame consume un token; los tokens se guardan en millonésimas para no
// perder la parte fraccionaria del repostaje.
const TOKEN_SCALE: u64 = 1_000_000;

/// Tasa sostenida según las muestras por frame del perfil v1.
///
/// 240 muestras son 200 fps nominales y 250 sostenidos; 120 y 480 escalan
/// para conservar 48 000 muestras/s (500 y 125 fps). Un valor desconocido
/// usa la tasa del perfil base.
pub const fn sustained_fps(sample_count: u16) -> u32 {
    match sample_count {
        120 => 500,
        480 => 125,
        _ => SUSTAINED_FPS_5MS,
    }
}

/// Token bucket de cadencia de una sesión.
///
/// Arranca con la ráfaga llena. [`CadenceLimiter::allow`] descuenta un token
/// o cuenta el frame como descartado. El reloj sólo avanza: un `now_us` menor
/// que el anterior no repone nada, así que un retroceso no regala tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CadenceLimiter {
    rate_per_sec: u32,
    burst: u32,
    tokens_micro: u64,
    last_us: Option<u64>,
    dropped: u64,
}

impl CadenceLimiter {
    /// Crea el limitador con la tasa y la ráfaga indicadas.
    ///
    /// Una tasa o una ráfaga en cero se elevan a uno: el limitador nunca
    /// queda mudo por configuración.
    pub const fn new(rate_per_sec: u32, burst: u32) -> Self {
        let rate_per_sec = if rate_per_sec == 0 { 1 } else { rate_per_sec };
        let burst = if burst == 0 { 1 } else { burst };
        Self {
            rate_per_sec,
            burst,
            tokens_micro: (burst as u64).saturating_mul(TOKEN_SCALE),
            last_us: None,
            dropped: 0,
        }
    }

    /// Ajusta la tasa sostenida sin vaciar la ráfaga disponible.
    pub fn set_rate_per_sec(&mut self, rate_per_sec: u32) {
        self.rate_per_sec = if rate_per_sec == 0 { 1 } else { rate_per_sec };
    }

    /// Tasa efectiva, en frames por segundo.
    pub const fn rate_per_sec(&self) -> u32 {
        self.rate_per_sec
    }

    /// Frames descartados por exceso de cadencia.
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// ¿Cabe un frame en `now_us`? Si no, lo cuenta como descartado.
    ///
    /// El repostaje es `elapsed_us * rate` en millonésimas de token; el saldo
    /// se satura en la ráfaga y los contadores en `u64::MAX`.
    pub fn allow(&mut self, now_us: u64) -> bool {
        let elapsed_us = self.last_us.map_or(0, |last| now_us.saturating_sub(last));
        if self.last_us.map_or(true, |last| now_us > last) {
            self.last_us = Some(now_us);
        }
        let capacity_micro = (self.burst as u64).saturating_mul(TOKEN_SCALE);
        let refill_micro = elapsed_us.saturating_mul(u64::from(self.rate_per_sec));
        self.tokens_micro = self
            .tokens_micro
            .saturating_add(refill_micro)
            .min(capacity_micro);
        if self.tokens_micro >= TOKEN_SCALE {
            self.tokens_micro -= TOKEN_SCALE;
            true
        } else {
            self.dropped = self.dropped.saturating_add(1);
            false
        }
    }
}

impl Default for CadenceLimiter {
    fn default() -> Self {
        Self::new(SUSTAINED_FPS_5MS, BURST_FRAMES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_is_allowed_then_excess_is_dropped() {
        let mut limiter = CadenceLimiter::default();
        for _ in 0..BURST_FRAMES {
            assert!(limiter.allow(0));
        }
        assert!(!limiter.allow(0));
        assert!(!limiter.allow(0));
        assert_eq!(limiter.dropped(), 2);
    }

    #[test]
    fn refill_follows_the_sustained_rate() {
        let mut limiter = CadenceLimiter::default();
        for _ in 0..BURST_FRAMES {
            assert!(limiter.allow(0));
        }
        assert!(
            !limiter.allow(3_999),
            "a 250 fps hacen falta 4 ms para reponer un token"
        );
        assert!(limiter.allow(4_000), "4 ms reponen exactamente un token");
        assert!(!limiter.allow(4_000), "el token ya se consumió");
        assert_eq!(limiter.dropped(), 2);
    }

    #[test]
    fn sustained_rate_holds_over_one_second() {
        let mut limiter = CadenceLimiter::default();
        let mut allowed = 0;
        for _ in 0..BURST_FRAMES {
            assert!(limiter.allow(0));
            allowed += 1;
        }
        for millis in 1..=1_000u64 {
            if limiter.allow(millis * 1_000) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, u64::from(BURST_FRAMES + SUSTAINED_FPS_5MS));
    }

    #[test]
    fn idle_does_not_exceed_the_burst() {
        let mut limiter = CadenceLimiter::default();
        for _ in 0..BURST_FRAMES {
            assert!(limiter.allow(0));
        }
        for _ in 0..BURST_FRAMES {
            assert!(limiter.allow(1_000_000), "un segundo repone la ráfaga");
        }
        assert!(!limiter.allow(1_000_000), "la ráfaga sigue acotada");
    }

    #[test]
    fn clock_regression_does_not_grant_tokens() {
        let mut limiter = CadenceLimiter::default();
        for _ in 0..BURST_FRAMES {
            assert!(limiter.allow(1_000));
        }
        assert!(!limiter.allow(0), "un reloj que retrocede no repone");
        assert!(
            !limiter.allow(1_000),
            "el reloj queda anclado al último valor"
        );
        assert_eq!(limiter.dropped(), 2);
    }

    #[test]
    fn sustained_fps_scales_with_the_profile() {
        assert_eq!(sustained_fps(120), 500);
        assert_eq!(sustained_fps(240), SUSTAINED_FPS_5MS);
        assert_eq!(sustained_fps(480), 125);
        assert_eq!(sustained_fps(0), SUSTAINED_FPS_5MS);
    }

    #[test]
    fn zero_rate_and_burst_are_clamped() {
        let mut limiter = CadenceLimiter::new(0, 0);
        assert_eq!(limiter.rate_per_sec(), 1);
        assert!(limiter.allow(0), "la ráfaga mínima deja pasar un frame");
        assert!(!limiter.allow(0));
        assert!(limiter.allow(1_000_000), "un token por segundo");
    }

    #[test]
    fn huge_burst_saturates_instead_of_overflowing() {
        let mut limiter = CadenceLimiter::new(SUSTAINED_FPS_5MS, u32::MAX);
        for _ in 0..1_000 {
            assert!(
                limiter.allow(0),
                "la ráfaga saturada no envuelve ni entra en pánico"
            );
        }
        assert_eq!(limiter.dropped(), 0);
    }
}
