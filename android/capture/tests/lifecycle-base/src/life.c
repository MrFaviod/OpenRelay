/* src/life.c - motor lifecycle arl_capture_* sobre el SPSC real (T13a S3c).
 *
 * Unico motor. Reserva el objeto y el anillo arl_spsc, y luego llama a la
 * factory privada arl_capture_backend_create(&be, owner). Sin stubs.
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

static const arl_capture_format k_pcm = {
    ARL_CAPTURE_SAMPLE_RATE_HZ, ARL_CAPTURE_CHANNELS,
    ARL_CAPTURE_BYTES_PER_SAMPLE, ARL_CAPTURE_FRAME_SAMPLES, 0u};

#define EVT_ERROR 1u
#define EVT_DISCONNECT 2u

struct arl_capture {
  arl_spsc *ring;
  struct arl_be *be;

  /* Solo control. */
  int32_t last_error;
  arl_capture_format effective;
  uint64_t last_token; /* ultimo token entregado al backend (enable) */
  int has_token;       /* 0 = nunca habilitado */

  /* Compartido. */
  _Atomic uint32_t state;
  _Atomic uint64_t generation;  /* status: cambia en stop y en evento */
  _Atomic uint64_t gate_token;  /* token vigente del productor, 0 = cerrado */
  /* Admision coordinada (ver adm_enter). Bits 0..15: push en vuelo.
   * Bits 16..30: read/drain/get_counters/note_* en vuelo. Bit 31: destroy
   * ha cerrado la admision. Destroy sólo cierra con la palabra en 0. */
  _Atomic uint32_t adm;
  _Atomic uint32_t ev_pending;  /* evento de error sin aplicar */
  _Atomic uint32_t ev_kind;
  _Atomic int32_t ev_code;

  /* Contadores atomicos individuales; no simultaneos entre si. */
  _Atomic uint64_t c_written;
  _Atomic uint64_t c_read;
  _Atomic uint64_t c_dropped;
  _Atomic uint64_t c_callbacks;
};

#define ADM_PUSH 1u
#define ADM_OTHER (1u << 16)
#define ADM_PUSH_MASK 0xFFFFu
#define ADM_CLOSED (1u << 31)

/* Admite una entrada en la unidad indicada. Si destroy ya cerro la admision,
 * deshace su propio incremento y devuelve 0 sin mas accesos al objeto.
 * Punto de linearizacion: el fetch_add. Es wait-free; no hay bucle. */
static int adm_enter(arl_capture *cap, uint32_t unit) {
  uint32_t old = atomic_fetch_add(&cap->adm, unit);
  if ((old & ADM_CLOSED) != 0u) {
    atomic_fetch_sub(&cap->adm, unit);
    return 0;
  }
  return 1;
}

static void adm_leave(arl_capture *cap, uint32_t unit) {
  atomic_fetch_sub(&cap->adm, unit);
}

static int format_eq(const arl_capture_format *a, const arl_capture_format *b) {
  return a->sample_rate_hz == b->sample_rate_hz && a->channels == b->channels &&
         a->bytes_per_sample == b->bytes_per_sample &&
         a->frame_samples == b->frame_samples && a->reserved0 == b->reserved0;
}

static int atomics_lock_free(void) {
  _Atomic uint32_t a32;
  _Atomic uint64_t a64;
  return atomic_is_lock_free(&a32) && atomic_is_lock_free(&a64);
}

static uint32_t state_of(const arl_capture *cap) {
  return atomic_load(&cap->state);
}

static void set_state(arl_capture *cap, uint32_t s) {
  atomic_store(&cap->state, s);
}

/* Aplica el evento de error publicado por el hilo de error. Solo control. */
static void apply_event(arl_capture *cap) {
  if (atomic_load_explicit(&cap->ev_pending, memory_order_acquire) == 0u) {
    return;
  }
  if (atomic_load(&cap->ev_kind) == EVT_DISCONNECT) {
    set_state(cap, ARL_CAPTURE_STATE_DISCONNECTED);
  } else {
    set_state(cap, ARL_CAPTURE_STATE_ERROR);
  }
  cap->last_error = atomic_load(&cap->ev_code);
  atomic_fetch_add(&cap->generation, 1u);
  atomic_store_explicit(&cap->ev_pending, 0u, memory_order_release);
}

/* Cierra el gate y espera a que ningun push siga dentro. Solo control. */
static void gate_close_wait(arl_capture *cap) {
  atomic_store(&cap->gate_token, 0u);
  while ((atomic_load(&cap->adm) & ADM_PUSH_MASK) != 0u) {
    sched_yield();
  }
}

static int32_t fail_with(arl_capture *cap, int32_t code) {
  cap->last_error = code;
  set_state(cap, ARL_CAPTURE_STATE_ERROR);
  return code;
}

int32_t arl_capture_open(arl_capture **out, uint32_t cap_samples) {
  arl_capture *cap;
  int32_t rc;

  if (out == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  *out = NULL;
  if (cap_samples != ARL_CAPTURE_RING_SAMPLES) {
    return ARL_CAPTURE_ERR_ARGS;
  }
  if (!atomics_lock_free()) {
    return ARL_CAPTURE_ERR_STATE;
  }
  cap = (arl_capture *)calloc(1, sizeof(*cap));
  if (cap == NULL) {
    return ARL_CAPTURE_ERR_STATE;
  }
  atomic_init(&cap->state, ARL_CAPTURE_STATE_OPEN);
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
  cap->last_error = ARL_CAPTURE_OK;

  rc = arl_spsc_create(&cap->ring);
  if (rc != ARL_CAPTURE_OK) {
    free(cap);
    return rc;
  }
  rc = arl_capture_backend_create(&cap->be, cap);
  if (rc != ARL_CAPTURE_OK || cap->be == NULL) {
    arl_spsc_destroy(&cap->ring);
    free(cap);
    return rc != ARL_CAPTURE_OK ? rc : ARL_CAPTURE_ERR_STATE;
  }
  *out = cap;
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_start(arl_capture *cap) {
  arl_capture_format eff;
  uint64_t tok;
  int32_t rc;

  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  apply_event(cap);
  switch (state_of(cap)) {
  case ARL_CAPTURE_STATE_CLOSED:
    return ARL_CAPTURE_ERR_CLOSED;
  case ARL_CAPTURE_STATE_STARTED:
    return ARL_CAPTURE_OK;
  case ARL_CAPTURE_STATE_OPEN:
  case ARL_CAPTURE_STATE_STOPPED:
    break;
  default:
    return ARL_CAPTURE_ERR_STATE;
  }

  set_state(cap, ARL_CAPTURE_STATE_STARTING);
  rc = cap->be->ops->prepare(cap->be, &k_pcm, &eff);
  if (rc != ARL_CAPTURE_OK) {
    return fail_with(cap, rc);
  }
  /* Relectura del formato en vigor: esto es lo que se valida. */
  rc = cap->be->ops->effective(cap->be, &cap->effective);
  if (rc != ARL_CAPTURE_OK) {
    return fail_with(cap, rc);
  }
  if (!format_eq(&cap->effective, &k_pcm)) {
    return fail_with(cap, ARL_CAPTURE_ERR_FORMAT);
  }

  /* Generacion nueva antes de habilitar callbacks. */
  tok = atomic_load(&cap->generation) + 1u;
  atomic_store(&cap->generation, tok);
  cap->last_token = tok;
  cap->has_token = 1;
  atomic_store(&cap->gate_token, tok);

  rc = cap->be->ops->enable(cap->be, tok);
  if (rc != ARL_CAPTURE_OK) {
    gate_close_wait(cap);
    return fail_with(cap, rc);
  }
  cap->last_error = ARL_CAPTURE_OK;
  set_state(cap, ARL_CAPTURE_STATE_STARTED);
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_stop(arl_capture *cap) {
  int32_t rc;

  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  apply_event(cap);
  switch (state_of(cap)) {
  case ARL_CAPTURE_STATE_STOPPED:
    return ARL_CAPTURE_OK;
  case ARL_CAPTURE_STATE_STARTED:
    break;
  default:
    return ARL_CAPTURE_ERR_STATE;
  }

  set_state(cap, ARL_CAPTURE_STATE_STOPPING);
  gate_close_wait(cap);
  /* El stop invalida la generacion: audio viejo no se acepta. */
  atomic_fetch_add(&cap->generation, 1u);
  rc = cap->be->ops->disable(cap->be);
  if (rc != ARL_CAPTURE_OK) {
    return fail_with(cap, rc);
  }
  set_state(cap, ARL_CAPTURE_STATE_STOPPED);
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_close(arl_capture *cap) {
  int32_t rc;

  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  apply_event(cap);
  switch (state_of(cap)) {
  case ARL_CAPTURE_STATE_CLOSED:
    return ARL_CAPTURE_OK;
  case ARL_CAPTURE_STATE_OPEN:
  case ARL_CAPTURE_STATE_STOPPED:
  case ARL_CAPTURE_STATE_ERROR:
  case ARL_CAPTURE_STATE_DISCONNECTED:
    break;
  default:
    return ARL_CAPTURE_ERR_STATE;
  }

  gate_close_wait(cap);
  rc = cap->be->ops->close(cap->be);
  if (rc != ARL_CAPTURE_OK) {
    return rc; /* estado intacto */
  }
  set_state(cap, ARL_CAPTURE_STATE_CLOSED);
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_destroy(arl_capture **owner) {
  arl_capture *cap;
  uint32_t flags = 0u;
  int32_t rc;

  if (owner == NULL || *owner == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  cap = *owner;
  apply_event(cap);
  if (state_of(cap) != ARL_CAPTURE_STATE_CLOSED) {
    return ARL_CAPTURE_ERR_STATE;
  }
  /* Punto de linearizacion de destroy: CAS 0 -> CLOSED. Exito solo si
   * ninguna entrada (push, read, drain, getters, note_*) esta admitida, y
   * desde ese momento todas las nuevas se rechazan. Sin espera. */
  {
    uint32_t expected = 0u;
    if (!atomic_compare_exchange_strong(&cap->adm, &expected, ADM_CLOSED)) {
      return ARL_CAPTURE_ERR_BUSY; /* *owner no cambia, admision intacta */
    }
  }
  if (atomic_load(&cap->ev_pending) != 0u) {
    atomic_fetch_and(&cap->adm, ~ADM_CLOSED); /* rollback BUSY */
    return ARL_CAPTURE_ERR_BUSY;
  }
  /* Retiro por token entregado. Instancia nunca habilitada: sin token. */
  if (cap->has_token) {
    rc = cap->be->ops->retire(cap->be, cap->last_token, &flags);
  } else {
    rc = cap->be->ops->retire(cap->be, 0u, &flags);
  }
  if (rc != ARL_CAPTURE_OK || flags != 0u) {
    /* Rollback: reabre la admision sin perder cuentas de otras entradas
     * (fetch_and conserva los contadores). *owner no cambia. */
    atomic_fetch_and(&cap->adm, ~ADM_CLOSED);
    return ARL_CAPTURE_ERR_BUSY;
  }
  cap->be->ops->release(cap->be);
  arl_spsc_destroy(&cap->ring);
  free(cap);
  *owner = NULL;
  return ARL_CAPTURE_OK;
}

static int32_t read_admitted(arl_capture *cap, int16_t *dst, uint32_t tope,
                             uint32_t *n) {
  int32_t rc;

  switch (state_of(cap)) {
  case ARL_CAPTURE_STATE_STARTING:
  case ARL_CAPTURE_STATE_STOPPING:
    return ARL_CAPTURE_ERR_BUSY;
  case ARL_CAPTURE_STATE_ERROR:
  case ARL_CAPTURE_STATE_DISCONNECTED:
    return ARL_CAPTURE_ERR_CLOSED;
  default:
    break;
  }
  if (tope == 0u) {
    return ARL_CAPTURE_OK;
  }
  if (dst == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  rc = arl_spsc_read(cap->ring, dst, tope, n);
  if (rc != ARL_CAPTURE_OK) {
    *n = 0u;
    return rc;
  }
  atomic_fetch_add(&cap->c_read, (uint64_t)*n);
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_read(arl_capture *cap, int16_t *dst, uint32_t *n) {
  uint32_t tope;
  int32_t rc;

  if (n == NULL) {
    return ARL_CAPTURE_ERR_NO_OUTPUT;
  }
  tope = *n;
  *n = 0u;
  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (tope > ARL_CAPTURE_FRAME_SAMPLES) {
    return ARL_CAPTURE_ERR_ARGS;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return ARL_CAPTURE_ERR_BUSY; /* destroy en curso o cerrado: *n = 0 */
  }
  rc = read_admitted(cap, dst, tope, n);
  adm_leave(cap, ADM_OTHER);
  return rc;
}

static int32_t drain_admitted(arl_capture *cap, uint32_t tope,
                              uint32_t *dropped) {
  int32_t rc;

  switch (state_of(cap)) {
  case ARL_CAPTURE_STATE_STARTING:
  case ARL_CAPTURE_STATE_STOPPING:
    return ARL_CAPTURE_ERR_BUSY;
  case ARL_CAPTURE_STATE_ERROR:
  case ARL_CAPTURE_STATE_DISCONNECTED:
    return ARL_CAPTURE_ERR_CLOSED;
  default:
    break;
  }
  rc = arl_spsc_drain(cap->ring, tope, dropped);
  if (rc != ARL_CAPTURE_OK) {
    *dropped = 0u;
  }
  return rc;
}

int32_t arl_capture_drain(arl_capture *cap, uint32_t *dropped) {
  uint32_t tope;
  int32_t rc;

  if (dropped == NULL) {
    return ARL_CAPTURE_ERR_NO_OUTPUT;
  }
  tope = *dropped;
  *dropped = 0u;
  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return ARL_CAPTURE_ERR_BUSY;
  }
  rc = drain_admitted(cap, tope, dropped);
  adm_leave(cap, ADM_OTHER);
  return rc;
}

int32_t arl_capture_get_status(const arl_capture *cap_c,
                               arl_capture_status *out) {
  arl_capture *cap = (arl_capture *)cap_c; /* control serializado */

  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (out == NULL) {
    return ARL_CAPTURE_ERR_NO_OUTPUT;
  }
  apply_event(cap);
  out->generation = atomic_load(&cap->generation);
  out->state = state_of(cap);
  out->last_error = cap->last_error;
  out->requested = k_pcm;
  out->effective = cap->effective;
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_get_counters(const arl_capture *cap_c,
                                 arl_capture_counters *out) {
  arl_capture *cap = (arl_capture *)cap_c;

  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (out == NULL) {
    return ARL_CAPTURE_ERR_NO_OUTPUT;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return ARL_CAPTURE_ERR_BUSY; /* *out no se escribe */
  }
  out->samples_written = atomic_load(&cap->c_written);
  out->samples_read = atomic_load(&cap->c_read);
  out->samples_dropped = atomic_load(&cap->c_dropped);
  out->underrun_samples = 0u; /* sin semantica normada: ver result.md */
  out->callbacks = atomic_load(&cap->c_callbacks);
  out->generation = atomic_load(&cap->generation);
  adm_leave(cap, ADM_OTHER);
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_push(arl_capture *cap, uint64_t generation,
                         const int16_t *samples, uint32_t n) {
  arl_spsc_push_result res;
  int32_t rc;

  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (samples == NULL && n > 0u) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (!adm_enter(cap, ADM_PUSH)) {
    return ARL_CAPTURE_ERR_CLOSED; /* destroy en curso o cerrado */
  }
  if (generation == 0u || atomic_load(&cap->gate_token) != generation) {
    adm_leave(cap, ADM_PUSH);
    return ARL_CAPTURE_ERR_CLOSED;
  }
  rc = arl_spsc_push(cap->ring, samples, n, &res);
  if (rc == ARL_CAPTURE_OK) {
    atomic_fetch_add(&cap->c_written, (uint64_t)res.written);
    atomic_fetch_add(&cap->c_dropped, (uint64_t)res.dropped);
    atomic_fetch_add(&cap->c_callbacks, 1u);
  }
  adm_leave(cap, ADM_PUSH);
  return rc;
}

static int32_t publish_event(arl_capture *cap, uint64_t generation,
                             uint32_t kind, int32_t code) {
  if (cap == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (generation == 0u) {
    return ARL_CAPTURE_ERR_CLOSED;
  }
  if (!adm_enter(cap, ADM_OTHER)) {
    return ARL_CAPTURE_ERR_CLOSED; /* destroy en curso o cerrado */
  }
  /* Solo el token vigente cierra el gate, y solo una vez por token. */
  {
    uint64_t expected = generation;
    if (!atomic_compare_exchange_strong(&cap->gate_token, &expected, 0u)) {
      adm_leave(cap, ADM_OTHER);
      return ARL_CAPTURE_ERR_CLOSED; /* obsoleto: sin mutacion */
    }
  }
  atomic_store(&cap->ev_kind, kind);
  atomic_store(&cap->ev_code, code);
  atomic_store_explicit(&cap->ev_pending, 1u, memory_order_release);
  adm_leave(cap, ADM_OTHER);
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_note_error(arl_capture *cap, uint64_t generation,
                               int32_t code) {
  return publish_event(cap, generation, EVT_ERROR, code);
}

int32_t arl_capture_note_disconnect(arl_capture *cap, uint64_t generation) {
  return publish_event(cap, generation, EVT_DISCONNECT,
                       ARL_CAPTURE_ERR_CLOSED);
}
