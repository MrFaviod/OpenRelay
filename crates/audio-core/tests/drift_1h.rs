//! T07 — drift de 1 h con relojes independientes y resampling real.
//!
//! La simulación es virtual y determinista: 48 kHz, frames de 5 ms, relojes de
//! productor y consumidor independientes con ppm de signo opuesto, jitter de
//! llegada acotado, reorden ocasional y pérdida rara. El controlador consume
//! los contadores reales del `LinearResampler`; no hay una demo matemática que
//! ignore la implementación. No es evidencia física: no hay hardware, red ni
//! PipeWire.

use openrelay_audio_core::drift::{DriftConfig, DriftController, DriftError};
use openrelay_audio_core::resample::{
    LinearResampler, ResampleError, RATIO_MAX, RATIO_MIN, RING_LEN,
};
use openrelay_audio_core::{SAMPLE_COUNT, SAMPLE_RATE_HZ};

const TICK_US: u64 = 5_000;
const QUEUE_CAP: usize = 64;
const UPDATE_TICKS: usize = 200; // 1 s de reloj virtual
const TICKS_PER_HOUR: usize = 720_000;
const JITTER_MAX_US: u64 = 2_000;
const REORDER_EVERY: u64 = 997;
const REORDER_DELAY_US: u64 = 6_500;
const DROP_EVERY: u64 = 12_000;

#[derive(Clone, Copy, Debug)]
struct Entry {
    seq: u32,
    samples: [i16; SAMPLE_COUNT],
}

/// Cola elástica de tamaño fijo, ordenada por secuencia: el reorden de llegada
/// no altera el orden de consumo y la memoria no crece.
struct BoundedQueue {
    slots: [Option<Entry>; QUEUE_CAP],
    len: usize,
    rejected: u64,
}

impl BoundedQueue {
    fn new() -> Self {
        Self {
            slots: [None; QUEUE_CAP],
            len: 0,
            rejected: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn insert(&mut self, entry: Entry) {
        if self.len == QUEUE_CAP {
            self.rejected = self.rejected.saturating_add(1);
            return;
        }
        let mut pos = self.len;
        while pos > 0 {
            let prev = self.slots[pos - 1].expect("entrada viva");
            if prev.seq <= entry.seq {
                break;
            }
            self.slots[pos] = Some(prev);
            pos -= 1;
        }
        self.slots[pos] = Some(entry);
        self.len += 1;
    }

    fn pop(&mut self) -> Option<Entry> {
        if self.len == 0 {
            return None;
        }
        let first = self.slots[0].take();
        for i in 1..self.len {
            self.slots[i - 1] = self.slots[i].take();
        }
        self.len -= 1;
        first
    }
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    deliver_us: u64,
    seq: u32,
    samples: [i16; SAMPLE_COUNT],
    dropped: bool,
}

#[derive(Debug)]
struct Report {
    ratio: f64,
    measured_ppm: f64,
    resyncs: u64,
    peak_fill: i64,
    min_fill: i64,
    max_fill: i64,
    dropped_frames: u64,
    underruns: u64,
    ring_overflows: u64,
    produced_samples: u64,
    delivered_samples: u64,
    consumed_samples: u64,
    produced_output: u64,
}

fn lcg(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *seed
}

fn run_simulation(producer_ppm: f64, consumer_ppm: f64, ticks: usize) -> Report {
    let mut queue = BoundedQueue::new();
    let mut resampler = LinearResampler::new(1.0).expect("ratio inicial válido");
    let mut controller = DriftController::new(DriftConfig::default()).expect("config válida");

    let producer_rate = f64::from(SAMPLE_RATE_HZ) * (1.0 + producer_ppm * 1e-6);
    let tick_period = TICK_US as f64 / (1.0 + consumer_ppm * 1e-6);

    let mut true_us = 0.0f64;
    let mut seed = 0x5eed_1234_abcd_9876u64;
    let mut pending: [Option<Pending>; 8] = [None; 8];
    let mut pending_len = 0usize;

    let mut generated: u64 = 0;
    let mut dropped_frames = 0u64;
    let mut delivered_samples = 0u64;
    let mut produced_samples = 0u64;

    let mut started = false;
    let mut peak_fill = 0i64;
    let mut min_fill = i64::MAX;
    let mut max_fill = 0i64;
    let mut underruns = 0u64;
    let mut ring_overflows = 0u64;
    let mut tick_count = 0usize;
    let mut last_update = report_stub();

    let mut out = [0i16; SAMPLE_COUNT];

    for _ in 0..ticks {
        // 1) Productor: frames completos a su propio ritmo de muestras.
        let due = ((true_us * producer_rate / 1e6) / SAMPLE_COUNT as f64).floor() as u64;
        while generated < due {
            let seq = generated as u32;
            let mut samples = [0i16; SAMPLE_COUNT];
            samples[0] = (generated % 1_000) as i16;
            samples[1] = seq as i16;

            let dropped = generated % DROP_EVERY == DROP_EVERY - 1;
            let mut delay = lcg(&mut seed) % JITTER_MAX_US;
            if generated % REORDER_EVERY == REORDER_EVERY - 1 {
                delay = REORDER_DELAY_US; // llega después del frame siguiente
            }
            let due_us =
                ((generated + 1) as f64 * SAMPLE_COUNT as f64 / producer_rate * 1e6).round() as u64;
            produced_samples = produced_samples.saturating_add(SAMPLE_COUNT as u64);

            if pending_len == pending.len() {
                dropped_frames += 1; // nunca ocurre con delay < 2 periodos
            } else {
                pending[pending_len] = Some(Pending {
                    deliver_us: due_us.saturating_add(delay),
                    seq,
                    samples,
                    dropped,
                });
                pending_len += 1;
            }
            generated += 1;
        }

        // 2) Red: entrega lo que ya venció; la pérdida rara no se entrega.
        let mut i = 0;
        while i < pending_len {
            let due_now = pending[i]
                .map(|p| p.deliver_us <= true_us as u64)
                .unwrap_or(false);
            if due_now {
                let p = pending[i].take().expect("pendiente vivo");
                pending_len -= 1;
                for j in i..pending_len {
                    pending[j] = pending[j + 1].take();
                }
                if p.dropped {
                    dropped_frames += 1;
                } else {
                    queue.insert(Entry {
                        seq: p.seq,
                        samples: p.samples,
                    });
                    delivered_samples = delivered_samples.saturating_add(SAMPLE_COUNT as u64);
                }
            } else {
                i += 1;
            }
        }

        if !started {
            if queue.len() >= DriftConfig::default().target_fill_frames as usize {
                started = true;
            } else {
                true_us += tick_period;
                continue;
            }
        }

        // 3) Consumidor: 240 muestras de salida por tick. El resampler toma
        //    del anillo todo lo que su ratio demanda; si el ratio es menor que
        //    1, ocasionalmente consume dos frames y drena el backlog.
        let ring_frames =
            |r: &LinearResampler| ((r.buffered_samples() + SAMPLE_COUNT - 1) / SAMPLE_COUNT) as i64;
        let fill_now = queue.len() as i64 + ring_frames(&resampler);
        peak_fill = peak_fill.max(fill_now);
        min_fill = min_fill.min(fill_now);
        max_fill = max_fill.max(fill_now);

        let needed = ((SAMPLE_COUNT as f64 - 1.0) / resampler.ratio()).ceil() as usize + 2;
        while resampler.buffered_samples() < needed {
            if queue.len() == 0 || resampler.buffered_samples() + SAMPLE_COUNT > RING_LEN {
                break;
            }
            let entry = queue.pop().expect("cola no vacía");
            if resampler.push_frame(&entry.samples).is_err() {
                ring_overflows += 1;
            }
        }
        if queue.len() == 0 && resampler.buffered_samples() == 0 {
            underruns += 1; // el resampler mantiene la última muestra
        }
        let written = resampler.pull_frame(&mut out);
        debug_assert_eq!(written, SAMPLE_COUNT);

        tick_count += 1;
        if tick_count % UPDATE_TICKS == 0 {
            let consumed = resampler.consumed_input();
            let fill_for_control = queue.len() as i64 + ring_frames(&resampler);
            match controller.observe(
                true_us as u64,
                fill_for_control,
                delivered_samples,
                consumed,
            ) {
                Ok(update) => {
                    last_update = update;
                    let _ = resampler.set_ratio(update.ratio);
                }
                Err(err) => panic!("observación válida rechazada: {err}"),
            }
        }

        true_us += tick_period;
    }

    Report {
        ratio: resampler.ratio(),
        measured_ppm: last_update.measured_ppm,
        resyncs: controller.resyncs(),
        peak_fill,
        min_fill,
        max_fill,
        dropped_frames,
        underruns,
        ring_overflows,
        produced_samples,
        delivered_samples,
        consumed_samples: resampler.consumed_input(),
        produced_output: resampler.produced_output(),
    }
}

fn report_stub() -> openrelay_audio_core::drift::DriftUpdate {
    openrelay_audio_core::drift::DriftUpdate {
        ratio: 1.0,
        measured_ppm: 0.0,
        fill_frames: 0,
        error_frames: 0,
        hard_resync: false,
    }
}

fn expected_ratio(producer_ppm: f64, consumer_ppm: f64) -> f64 {
    let relative = (1.0 + producer_ppm * 1e-6) / (1.0 + consumer_ppm * 1e-6) - 1.0;
    let delivered_fraction = 1.0 - 1.0 / DROP_EVERY as f64;
    1.0 / ((1.0 + relative) * delivered_fraction)
}

fn check_report(report: &Report, producer_ppm: f64, consumer_ppm: f64, tolerance: f64) {
    let expected = expected_ratio(producer_ppm, consumer_ppm);
    assert!(
        report.ratio.is_finite() && (RATIO_MIN..=RATIO_MAX).contains(&report.ratio),
        "ratio fuera de la ventana de corrección: {}",
        report.ratio
    );
    assert!(
        (report.ratio - expected).abs() < tolerance,
        "ratio {:.9} no converge a {:.9} (tolerancia {tolerance})",
        report.ratio,
        expected
    );
    assert_eq!(report.resyncs, 0, "no debe haber resync dentro de ventana");
    assert!(report.peak_fill <= QUEUE_CAP as i64);
    assert!(
        report.max_fill
            <= (DriftConfig::default().target_fill_frames + DriftConfig::default().window_frames)
                as i64,
        "fill máximo {} fuera de ventana",
        report.max_fill
    );
    assert!(report.min_fill >= 0);
    assert_eq!(report.underruns, 0, "el buffer no debe quedar seco");
    assert_eq!(report.ring_overflows, 0);
    let step = report.consumed_samples as f64 / report.produced_output.max(1) as f64;
    assert!(
        (step - 1.0 / report.ratio).abs() < 0.005,
        "el resampler no consumió el ratio: step={step} ratio={}",
        report.ratio
    );
    assert!(report.produced_samples > 0 && report.delivered_samples > 0);
}

fn print_report(label: &str, producer_ppm: f64, consumer_ppm: f64, report: &Report) {
    let relative = (1.0 + producer_ppm * 1e-6) / (1.0 + consumer_ppm * 1e-6) - 1.0;
    println!(
        "drift_1h[{label}] productor={producer_ppm:+.0} ppm consumidor={consumer_ppm:+.0} ppm relativo={:+.1} ppm",
        relative * 1e6
    );
    println!(
        "  ratio={:.9} ({:+.1} ppm) medido_ultimo={:+.1} ppm esperado={:.9}",
        report.ratio,
        (report.ratio - 1.0) * 1e6,
        report.measured_ppm,
        expected_ratio(producer_ppm, consumer_ppm)
    );
    println!(
        "  fill min/max/peak={} / {} / {} frames (cota {QUEUE_CAP})",
        report.min_fill, report.max_fill, report.peak_fill
    );
    println!(
        "  perdidos={} underruns={} overflows_ring={} resyncs={}",
        report.dropped_frames, report.underruns, report.ring_overflows, report.resyncs
    );
    println!(
        "  muestras producidas={} entregadas={} consumidas={} salida={}",
        report.produced_samples,
        report.delivered_samples,
        report.consumed_samples,
        report.produced_output
    );
}

#[test]
fn config_no_finita_se_rechaza() {
    let cfg = DriftConfig {
        kp_ratio_per_frame: f64::NAN,
        ..DriftConfig::default()
    };
    assert!(matches!(
        DriftController::new(cfg),
        Err(DriftError::NonFiniteConfig)
    ));

    let cfg = DriftConfig {
        target_fill_frames: 0,
        ..DriftConfig::default()
    };
    assert!(DriftController::new(cfg).is_err());
}

#[test]
fn observaciones_invalidas_no_paniquean() {
    let mut controller = DriftController::new(DriftConfig::default()).expect("config válida");
    controller
        .observe(1_000_000, 4, 48_000, 47_000)
        .expect("primera observación");
    assert!(controller.observe(1_000_000, 4, 96_000, 95_000).is_err());
    assert!(controller.observe(900_000, 4, 96_000, 95_000).is_err());
    assert!(controller.observe(2_000_000, 4, 47_000, 46_000).is_err());
    assert!(controller.ratio().is_finite());
}

#[test]
fn resampler_valida_ratio_y_rechaza_no_finito() {
    assert!(matches!(
        LinearResampler::new(f64::INFINITY),
        Err(ResampleError::NonFiniteRatio)
    ));
    let mut r = LinearResampler::new(1.0).expect("ratio válido");
    assert!(matches!(
        r.set_ratio(f64::NAN),
        Err(ResampleError::NonFiniteRatio)
    ));
    assert_eq!(r.set_ratio(5.0).expect("se limita"), RATIO_MAX);
    assert_eq!(r.set_ratio(0.01).expect("se limita"), RATIO_MIN);
}

#[test]
fn resampler_identidad_con_ratio_uno() {
    let mut samples = [0i16; SAMPLE_COUNT];
    for (i, s) in samples.iter_mut().enumerate() {
        *s = (i as i16) * 10;
    }
    let mut r = LinearResampler::new(1.0).expect("ratio válido");
    r.push_frame(&samples).expect("cabe en el ring");
    let mut out = [0i16; SAMPLE_COUNT];
    assert_eq!(r.pull_frame(&mut out), SAMPLE_COUNT);
    assert_eq!(out, samples);
}

#[test]
fn resampler_consume_de_verdad_el_ratio() {
    let mut samples = [0i16; SAMPLE_COUNT];
    for (i, s) in samples.iter_mut().enumerate() {
        *s = i as i16;
    }
    let mut slow = LinearResampler::new(1.0).expect("ratio válido");
    let mut fast = LinearResampler::new(1.0).expect("ratio válido");
    slow.set_ratio(0.999).expect("ratio válido");
    fast.set_ratio(1.001).expect("ratio válido");
    let mut out = [0i16; SAMPLE_COUNT];
    slow.push_frame(&samples).expect("cabe");
    fast.push_frame(&samples).expect("cabe");
    slow.push_frame(&samples).expect("cabe");
    fast.push_frame(&samples).expect("cabe");
    slow.pull_frame(&mut out);
    fast.pull_frame(&mut out);
    assert!(
        slow.consumed_input() > fast.consumed_input(),
        "un ratio menor debe consumir más entrada: {} vs {}",
        slow.consumed_input(),
        fast.consumed_input()
    );
}

#[test]
fn ring_del_resampler_es_acotado() {
    let samples = [0i16; SAMPLE_COUNT];
    let mut r = LinearResampler::new(1.0).expect("ratio válido");
    assert!(r.push_frame(&samples).is_ok());
    assert!(r.push_frame(&samples).is_ok());
    assert!(matches!(
        r.push_frame(&samples),
        Err(ResampleError::RingFull)
    ));
}

#[test]
fn drift_controlado_60_s() {
    let report = run_simulation(150.0, -100.0, 12_000);
    print_report("60s", 150.0, -100.0, &report);
    check_report(&report, 150.0, -100.0, 400e-6);
}

#[test]
#[ignore = "simulación virtual de 1 h; ejecutar con --ignored --nocapture"]
fn drift_1h_positivo() {
    let report = run_simulation(150.0, -100.0, TICKS_PER_HOUR);
    print_report("+250ppm", 150.0, -100.0, &report);
    check_report(&report, 150.0, -100.0, 150e-6);
}

#[test]
#[ignore = "simulación virtual de 1 h; ejecutar con --ignored --nocapture"]
fn drift_1h_negativo() {
    let report = run_simulation(-150.0, 100.0, TICKS_PER_HOUR);
    print_report("-250ppm", -150.0, 100.0, &report);
    check_report(&report, -150.0, 100.0, 150e-6);
}
