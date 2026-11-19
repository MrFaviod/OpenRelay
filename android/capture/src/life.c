/* src/life.c - motor lifecycle openrelay_capture_* sobre el SPSC real (T13a S3c).
 *
 * Unico motor. Reserva el objeto y el anillo openrelay_spsc, y luego llama a la
 * factory privada openrelay_capture_backend_create(&be, owner). Sin stubs.
 *
 * Hilos: control (open/start/stop/close/destroy/get_status serializados),
 * productor RT (push), consumidor (read/drain/get_counters), error
 * (note_error/note_disconnect). Atomics lock-free comprobados en open.
 *
 * Gate de publicacion: gate_token guarda el token vigente del productor
 * (0 = gate cerrado; los tokens reales son >= 1). push acepta solo si
 * gate_token == generation. stop y note_* lo ponen a 0 con seq_cst; stop
 * espera a que la cuenta de push de adm sea 0 antes de disable (ningun push en vuelo
 * sobrevive al stop). note_* no espera: solo cierra el gate y publica un
 * evento acotado, que el control aplica en su siguiente operacion.
 *
 * Token de backend: last_token se guarda aparte de generation (status).
 * retire() consulta last_token, no generation.
 */
#define _POSIX_C_SOURCE 200809L

#include <sched.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

#include "capture-abi.h"
#include "capture-backend.h"
#include "capture-spsc.h"

static const openrelay_capture_format k_pcm = {
    OPENRELAY_CAPTURE_SAMPLE_RATE_HZ, OPENRELAY_CAPTURE_CHANNELS,
    OPENRELAY_CAPTURE_BYTES_PER_SAMPLE, OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u};

#define EVT_ERROR 1u
#define EVT_DISCONNECT 2u

struct openrelay_capture {
  openrelay_spsc *ring;
  struct openrelay_be *be;

  /* Solo control. */
  int32_t last_error;
  openrelay_capture_format effective;
  uint64_t last_token; /* ultimo token entregado al backend (enable) */
  int has_token;       /* 0 = nunca habilitado */

  /* Compartido. */
  _Atomic uint32_t state;
  _Atomic uint64_t generation;  /* status: cambia en stop y en evento */
  _Atomic uint64_t gate_token;  /* token vigente del productor, 0 = cerrado */
  /* Admision coordinada (ver adm_enter). Bits 0..31: push en vuelo.
   * Bits 32..62: read/drain/get_counters/note_* en vuelo. Bit 63: destroy
   * ha cerrado la admision. Destroy sólo cierra con la palabra en 0. */
  _Atomic uint64_t adm;
  _Atomic uint32_t ev_pending;  /* evento de error sin aplicar */
  _Atomic uint32_t ev_kind;
  _Atomic int32_t ev_code;

  /* Contadores atomicos individuales; no simultaneos entre si. */
  _Atomic uint64_t c_written;
  _Atomic uint64_t c_read;
  _Atomic uint64_t c_dropped;
  _Atomic uint64_t c_callbacks;
};

#define ADM_PUSH UINT64_C(1)
#define ADM_OTHER (UINT64_C(1) << 32)
#define ADM_PUSH_MASK UINT64_C(0xFFFFFFFF)
#define ADM_CLOSED (UINT64_C(1) << 63)

/* Admision: un solo fetch_add, sin reintentos ni CAS. Rechaza (y deshace su
 * suma) si el bit CLOSED estaba puesto o si el campo ya esta saturado. Un
 * rechazo nunca se debe a contencion: solo a cierre o saturacion. Limite:
 * el ABI no fija numero de actores; el campo de 15 bits no se defiende contra
 * mas de 32767 entradas simultaneas (no alcanzable con 1 productor y consumidor
 * + error/control). Acceso raw seguro solo bajo prestamo del owner
 * (capture-owner.c), que impide llamadas tras destroy OK. */
#define ADM_OTHER_MASK (UINT64_C(0x7FFFFFFF) << 32)

static int adm_enter(openrelay_capture *cap, uint64_t unit) {
  uint64_t mask = (unit == ADM_PUSH) ? ADM_PUSH_MASK : ADM_OTHER_MASK;
  uint64_t old = atomic_load(&cap->adm);

  if ((old & ADM_CLOSED) != 0u || (old & mask) == mask) {
    return 0; /* cerrada o saturada: sin mutacion */
  }
  old = atomic_fetch_add(&cap->adm, unit);
  if ((old & ADM_CLOSED) != 0u || (old & mask) == mask) {
    atomic_fetch_sub(&cap->adm, unit); /* deshace su propia suma */
    return 0;
  }
  return 1;
}

static void adm_leave(openrelay_capture *cap, uint64_t unit) {
  atomic_fetch_sub(&cap->adm, unit);
}

static int format_eq(const openrelay_capture_format *a, const openrelay_capture_format *b) {
  return a->sample_rate_hz == b->sample_rate_hz && a->channels == b->channels &&
         a->bytes_per_sample == b->bytes_per_sample &&
         a->frame_samples == b->frame_samples && a->reserved0 == b->reserved0;
}

static int atomics_lock_free(void) {
  _Atomic uint32_t a32;
  _Atomic uint64_t a64;
  return atomic_is_lock_free(&a32) && atomic_is_lock_free(&a64);
}

static uint32_t state_of(const openrelay_capture *cap) {
  return atomic_load(&cap->state);
}

static void set_state(openrelay_capture *cap, uint32_t s) {
  atomic_store(&cap->state, s);
}

/* Aplica el evento de error publicado por el hilo de error. Solo control. */
static void apply_event(openrelay_capture *cap) {
  if (atomic_load_explicit(&cap->ev_pending, memory_order_acquire) == 0u) {
    return;
  }
  if (atomic_load(&cap->ev_kind) == EVT_DISCONNECT) {
    set_state(cap, OPENRELAY_CAPTURE_STATE_DISCONNECTED);
  } else {
    set_state(cap, OPENRELAY_CAPTURE_STATE_ERROR);
  }
  cap->last_error = atomic_load(&cap->ev_code);
  atomic_fetch_add(&cap->generation, 1u);
  atomic_store_explicit(&cap->ev_pending, 0u, memory_order_release);
}

/* Cierra el gate y espera a que ningun push siga dentro. Solo control. */
static void gate_close_wait(openrelay_capture *cap) {
  atomic_store(&cap->gate_token, 0u);
  while ((atomic_load(&cap->adm) & ADM_PUSH_MASK) != 0u) {
    sched_yield();
  }
}

static int32_t fail_with(openrelay_capture *cap, int32_t code) {
  cap->last_error = code;
  set_state(cap, OPENRELAY_CAPTURE_STATE_ERROR);
  return code;
}

int32_t openrelay_capture_open(openrelay_capture **out, uint32_t cap_samples) {
  openrelay_capture *cap;
  int32_t rc;

  if (out == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  *out = NULL;
  if (cap_samples != OPENRELAY_CAPTURE_RING_SAMPLES) {
    return OPENRELAY_CAPTURE_ERR_ARGS;
  }
  if (!atomics_lock_free()) {
    return OPENRELAY_CAPTURE_ERR_STATE;
  }
  cap = (openrelay_capture *)calloc(1, sizeof(*cap));
  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_STATE;
  }
  atomic_init(&cap->state, OPENRELAY_CAPTURE_STATE_OPEN);
  atomic_init(&cap->generation, 0u);
  atomic_init(&cap->gate_token, 0u);
  atomic_init(&cap->adm, 0u);
  atomic_init(&cap->ev_pending, 0u);
  atomic_init(&cap->ev_kind, 0u);
  atomic_init(&cap->ev_code, 0);
  atomic_init(&cap->c_written, 0u);
  atomic_init(&cap->c_read, 0u);
  atomic_init(&cap->c_dropped, 0u);
  atomic_init(&cap->c_callbacks, 0u);
  cap->last_error = OPENRELAY_CAPTURE_OK;

  rc = openrelay_spsc_create(&cap->ring);
  if (rc != OPENRELAY_CAPTURE_OK) {
    free(cap);
    return rc;
  }
  rc = openrelay_capture_backend_create(&cap->be, cap);
  if (rc != OPENRELAY_CAPTURE_OK || cap->be == NULL) {
    openrelay_spsc_destroy(&cap->ring);
    free(cap);
    return rc != OPENRELAY_CAPTURE_OK ? rc : OPENRELAY_CAPTURE_ERR_STATE;
  }
  *out = cap;
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_start(openrelay_capture *cap) {
  openrelay_capture_format eff;
  uint64_t tok;
  int32_t rc;

  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  apply_event(cap);
  switch (state_of(cap)) {
  case OPENRELAY_CAPTURE_STATE_CLOSED:
    return OPENRELAY_CAPTURE_ERR_CLOSED;
  case OPENRELAY_CAPTURE_STATE_STARTED:
    return OPENRELAY_CAPTURE_OK;
  case OPENRELAY_CAPTURE_STATE_OPEN:
  case OPENRELAY_CAPTURE_STATE_STOPPED:
    break;
  default:
    return OPENRELAY_CAPTURE_ERR_STATE;
  }

  set_state(cap, OPENRELAY_CAPTURE_STATE_STARTING);
  rc = cap->be->ops->prepare(cap->be, &k_pcm, &eff);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return fail_with(cap, rc);
  }
  /* Relectura del formato en vigor: esto es lo que se valida. */
  rc = cap->be->ops->effective(cap->be, &cap->effective);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return fail_with(cap, rc);
  }
  if (!format_eq(&cap->effective, &k_pcm)) {
    return fail_with(cap, OPENRELAY_CAPTURE_ERR_FORMAT);
  }

  /* Generacion nueva antes de habilitar callbacks. */
  tok = atomic_load(&cap->generation) + 1u;
  atomic_store(&cap->generation, tok);
  cap->last_token = tok;
  cap->has_token = 1;

  atomic_store(&cap->gate_token, tok);
  rc = cap->be->ops->enable(cap->be, tok);
  if (rc != OPENRELAY_CAPTURE_OK) {
    gate_close_wait(cap);
    return fail_with(cap, rc);
  }
  /* STARTED solo tras enable OK: push durante STARTING se rechaza por estado. */
  cap->last_error = OPENRELAY_CAPTURE_OK;
  set_state(cap, OPENRELAY_CAPTURE_STATE_STARTED);
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_stop(openrelay_capture *cap) {
  int32_t rc;

  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  apply_event(cap);
  switch (state_of(cap)) {
  case OPENRELAY_CAPTURE_STATE_STOPPED:
    return OPENRELAY_CAPTURE_OK;
  case OPENRELAY_CAPTURE_STATE_STARTED:
    break;
  default:
    return OPENRELAY_CAPTURE_ERR_STATE;
  }

  set_state(cap, OPENRELAY_CAPTURE_STATE_STOPPING);
  gate_close_wait(cap);
  /* El stop invalida la generacion: audio viejo no se acepta. */
  atomic_fetch_add(&cap->generation, 1u);
  rc = cap->be->ops->disable(cap->be);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return fail_with(cap, rc);
  }
  set_state(cap, OPENRELAY_CAPTURE_STATE_STOPPED);
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_close(openrelay_capture *cap) {
  int32_t rc;

  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  apply_event(cap);
  switch (state_of(cap)) {
  case OPENRELAY_CAPTURE_STATE_CLOSED:
    return OPENRELAY_CAPTURE_OK;
  case OPENRELAY_CAPTURE_STATE_OPEN:
  case OPENRELAY_CAPTURE_STATE_STOPPED:
  case OPENRELAY_CAPTURE_STATE_ERROR:
  case OPENRELAY_CAPTURE_STATE_DISCONNECTED:
    break;
  default:
    return OPENRELAY_CAPTURE_ERR_STATE;
  }

  gate_close_wait(cap);
  rc = cap->be->ops->close(cap->be);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc; /* estado intacto */
  }
  set_state(cap, OPENRELAY_CAPTURE_STATE_CLOSED);
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_destroy(openrelay_capture **owner) {
  openrelay_capture *cap;
  uint32_t flags = 0u;
  int32_t rc;

  if (owner == NULL || *owner == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  cap = *owner;
  apply_event(cap);
  if (state_of(cap) != OPENRELAY_CAPTURE_STATE_CLOSED) {
    return OPENRELAY_CAPTURE_ERR_STATE;
  }
  /* Punto de linearizacion de destroy: CAS 0 -> CLOSED. Exito solo si
   * ninguna entrada (push, read, drain, getters, note_*) esta admitida, y
   * desde ese momento todas las nuevas se rechazan. Sin espera. */
  {
    uint64_t expected = 0u;
    if (!atomic_compare_exchange_strong(&cap->adm, &expected, ADM_CLOSED)) {
      return OPENRELAY_CAPTURE_ERR_BUSY; /* *owner no cambia, admision intacta */
    }
  }
  if (atomic_load(&cap->ev_pending) != 0u) {
    atomic_fetch_and(&cap->adm, ~ADM_CLOSED); /* rollback BUSY */
    return OPENRELAY_CAPTURE_ERR_BUSY;
  }
  /* Retiro por token entregado. Instancia nunca habilitada: sin token. */
  if (cap->has_token) {
    rc = cap->be->ops->retire(cap->be, cap->last_token, &flags);
  } else {
    rc = cap->be->ops->retire(cap->be, 0u, &flags);
  }
  if (rc != OPENRELAY_CAPTURE_OK || flags != 0u) {
    /* Rollback: reabre la admision sin perder cuentas de otras entradas
     * (fetch_and conserva los contadores). *owner no cambia. */
    atomic_fetch_and(&cap->adm, ~ADM_CLOSED);
    return OPENRELAY_CAPTURE_ERR_BUSY;
  }
  cap->be->ops->release(cap->be);
  openrelay_spsc_destroy(&cap->ring);
  free(cap);
  *owner = NULL;
  return OPENRELAY_CAPTURE_OK;
}

static int32_t read_admitted(openrelay_capture *cap, int16_t *dst, uint32_t tope,
                             uint32_t *n) {
  int32_t rc;

  switch (state_of(cap)) {
  case OPENRELAY_CAPTURE_STATE_STARTING:
  case OPENRELAY_CAPTURE_STATE_STOPPING:
    return OPENRELAY_CAPTURE_ERR_BUSY;
  case OPENRELAY_CAPTURE_STATE_ERROR:
  case OPENRELAY_CAPTURE_STATE_DISCONNECTED:
    return OPENRELAY_CAPTURE_ERR_CLOSED;
  default:
    break;
  }
  if (tope == 0u) {
    return OPENRELAY_CAPTURE_OK;
  }
  if (dst == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  rc = openrelay_spsc_read(cap->ring, dst, tope, n);
  if (rc != OPENRELAY_CAPTURE_OK) {
    *n = 0u;
    return rc;
  }
  atomic_fetch_add(&cap->c_read, (uint64_t)*n);
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_read(openrelay_capture *cap, int16_t *dst, uint32_t *n) {
  uint32_t tope;
  int32_t rc;

  if (n == NULL) {
    return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  }
  tope = *n;
  *n = 0u;
  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (tope > OPENRELAY_CAPTURE_FRAME_SAMPLES) {
    return OPENRELAY_CAPTURE_ERR_ARGS;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return OPENRELAY_CAPTURE_ERR_BUSY; /* destroy en curso o cerrado: *n = 0 */
  }
  rc = read_admitted(cap, dst, tope, n);
  adm_leave(cap, ADM_OTHER);
  return rc;
}

static int32_t drain_admitted(openrelay_capture *cap, uint32_t tope,
                              uint32_t *dropped) {
  int32_t rc;

  switch (state_of(cap)) {
  case OPENRELAY_CAPTURE_STATE_STARTING:
  case OPENRELAY_CAPTURE_STATE_STOPPING:
    return OPENRELAY_CAPTURE_ERR_BUSY;
  case OPENRELAY_CAPTURE_STATE_ERROR:
  case OPENRELAY_CAPTURE_STATE_DISCONNECTED:
    return OPENRELAY_CAPTURE_ERR_CLOSED;
  default:
    break;
  }
  rc = openrelay_spsc_drain(cap->ring, tope, dropped);
  if (rc != OPENRELAY_CAPTURE_OK) {
    *dropped = 0u;
  }
  return rc;
}

int32_t openrelay_capture_drain(openrelay_capture *cap, uint32_t *dropped) {
  uint32_t tope;
  int32_t rc;

  if (dropped == NULL) {
    return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  }
  tope = *dropped;
  *dropped = 0u;
  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return OPENRELAY_CAPTURE_ERR_BUSY;
  }
  rc = drain_admitted(cap, tope, dropped);
  adm_leave(cap, ADM_OTHER);
  return rc;
}

int32_t openrelay_capture_get_status(const openrelay_capture *cap_c,
                               openrelay_capture_status *out) {
  openrelay_capture *cap = (openrelay_capture *)cap_c; /* control serializado */

  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (out == NULL) {
    return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  }
  apply_event(cap);
  out->generation = atomic_load(&cap->generation);
  out->state = state_of(cap);
  out->last_error = cap->last_error;
  out->requested = k_pcm;
  out->effective = cap->effective;
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_get_counters(const openrelay_capture *cap_c,
                                 openrelay_capture_counters *out) {
  openrelay_capture *cap = (openrelay_capture *)cap_c;

  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (out == NULL) {
    return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return OPENRELAY_CAPTURE_ERR_BUSY; /* *out no se escribe */
  }
  out->samples_written = atomic_load(&cap->c_written);
  out->samples_read = atomic_load(&cap->c_read);
  out->samples_dropped = atomic_load(&cap->c_dropped);
  out->underrun_samples = 0u; /* sin semantica normada: ver result.md */
  out->callbacks = atomic_load(&cap->c_callbacks);
  out->generation = atomic_load(&cap->generation);
  adm_leave(cap, ADM_OTHER);
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_push(openrelay_capture *cap, uint64_t generation,
                         const int16_t *samples, uint32_t n) {
  openrelay_spsc_push_result res;
  int32_t rc;

  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (samples == NULL && n > 0u) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (!adm_enter(cap, ADM_PUSH)) {
    return OPENRELAY_CAPTURE_ERR_CLOSED; /* destroy en curso o cerrado */
  }
  if (generation == 0u || atomic_load(&cap->gate_token) != generation ||
      state_of(cap) != OPENRELAY_CAPTURE_STATE_STARTED) {
    adm_leave(cap, ADM_PUSH);
    return OPENRELAY_CAPTURE_ERR_CLOSED;
  }
  rc = openrelay_spsc_push(cap->ring, samples, n, &res);
  if (rc == OPENRELAY_CAPTURE_OK) {
    atomic_fetch_add(&cap->c_written, (uint64_t)res.written);
    atomic_fetch_add(&cap->c_dropped, (uint64_t)res.dropped);
    atomic_fetch_add(&cap->c_callbacks, 1u);
  }
  adm_leave(cap, ADM_PUSH);
  return rc;
}

static int32_t publish_event(openrelay_capture *cap, uint64_t generation,
                             uint32_t kind, int32_t code) {
  if (cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (generation == 0u) {
    return OPENRELAY_CAPTURE_ERR_CLOSED;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return OPENRELAY_CAPTURE_ERR_CLOSED; /* destroy en curso o cerrado */
  }
  /* Solo el token vigente cierra el gate, y solo una vez por token. */
  {
    uint64_t expected = generation;
    if (!atomic_compare_exchange_strong(&cap->gate_token, &expected, 0u)) {
      adm_leave(cap, ADM_OTHER);
      return OPENRELAY_CAPTURE_ERR_CLOSED; /* obsoleto: sin mutacion */
    }
  }
  atomic_store(&cap->ev_kind, kind);
  atomic_store(&cap->ev_code, code);
  atomic_store_explicit(&cap->ev_pending, 1u, memory_order_release);
  adm_leave(cap, ADM_OTHER);
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_note_error(openrelay_capture *cap, uint64_t generation,
                               int32_t code) {
  return publish_event(cap, generation, EVT_ERROR, code);
}

int32_t openrelay_capture_note_disconnect(openrelay_capture *cap, uint64_t generation) {
  return publish_event(cap, generation, EVT_DISCONNECT,
                       OPENRELAY_CAPTURE_ERR_CLOSED);
}
