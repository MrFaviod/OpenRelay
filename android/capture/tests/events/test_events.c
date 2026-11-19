/* test_events.c - eventos, getters admitidos y ventanas de publicacion.
 * Uso: test_events <caso>. Exit 0 PASS, 1 RED (aserto falla), 2 SETUP_FAIL.
 * Los hooks openrelay_test_hook(id) los insertan copias instrumentadas (instrument.py);
 * no hay stubs del motor ni cambios de logica. Esperas con condvar, sin sleeps. */
#define _POSIX_C_SOURCE 200809L
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "capture-abi.h"
#include "capture-backend.h"
#include "capture-spsc.h"
#include "backend_fixture.h"

#define WAIT_S 5
#define NS OPENRELAY_CAPTURE_FRAME_SAMPLES

static int fails;
#define CHECK(c)                                                              \
  do {                                                                        \
    if (!(c)) {                                                               \
      fails++;                                                                \
      fprintf(stderr, "CHECK FAIL line %d: %s\n", __LINE__, #c);              \
    }                                                                         \
  } while (0)
#define SETUP(c)                                                              \
  do {                                                                        \
    if (!(c)) {                                                               \
      fprintf(stderr, "SETUP_FAIL line %d: %s\n", __LINE__, #c);              \
      return 2;                                                               \
    }                                                                         \
  } while (0)

/* ---- hook con retencion (condvar, timeout fail-closed) ---- */
static pthread_mutex_t hmu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t hcv = PTHREAD_COND_INITIALIZER;
static const char *h_name;
static void (*h_fn)(void);
static int h_entered, h_released;

void openrelay_test_hook(const char *id) {
  void (*fn)(void) = NULL;
  pthread_mutex_lock(&hmu);
  if (h_name != NULL && strcmp(h_name, id) == 0) {
    fn = h_fn;
    h_name = NULL;
    h_fn = NULL;
  }
  pthread_mutex_unlock(&hmu);
  if (fn != NULL) {
    fn();
  }
}

static void deadline(struct timespec *ts) {
  clock_gettime(CLOCK_REALTIME, ts);
  ts->tv_sec += WAIT_S;
}

static void hold_arm(const char *name, void (*fn)(void)) {
  pthread_mutex_lock(&hmu);
  h_name = name;
  h_fn = fn;
  h_entered = 0;
  h_released = 0;
  pthread_mutex_unlock(&hmu);
}

static void hold_fn(void) {
  struct timespec ts;
  int rc = 0;
  deadline(&ts);
  pthread_mutex_lock(&hmu);
  h_entered = 1;
  pthread_cond_broadcast(&hcv);
  while (!h_released && rc == 0) {
    rc = pthread_cond_timedwait(&hcv, &hmu, &ts);
  }
  pthread_mutex_unlock(&hmu);
}

static int wait_entered(void) {
  struct timespec ts;
  int ok;
  int rc = 0;
  deadline(&ts);
  pthread_mutex_lock(&hmu);
  while (!h_entered && rc == 0) {
    rc = pthread_cond_timedwait(&hcv, &hmu, &ts);
  }
  ok = h_entered;
  pthread_mutex_unlock(&hmu);
  return ok;
}

static void release_hold(void) {
  pthread_mutex_lock(&hmu);
  h_released = 1;
  pthread_cond_broadcast(&hcv);
  pthread_mutex_unlock(&hmu);
}

/* ---- estado de caso ---- */
static openrelay_capture *cap;
static int16_t buf[NS];
static atomic_int stop_done;

struct job {
  openrelay_capture *cap;
  uint64_t gen;
  int32_t rc;
  openrelay_capture_counters ctr;
};

static void *job_counters(void *p) {
  struct job *j = p;
  j->rc = openrelay_capture_get_counters(j->cap, &j->ctr);
  return NULL;
}

static void *job_push(void *p) {
  struct job *j = p;
  j->rc = openrelay_capture_push(j->cap, j->gen, buf, NS);
  return NULL;
}

static void *job_stop(void *p) {
  struct job *j = p;
  j->rc = openrelay_capture_stop(j->cap);
  atomic_store(&stop_done, 1);
  return NULL;
}

static int setup(void) {
  openrelay_fx_reset();
  cap = NULL;
  openrelay_fx_set_prepare_format(48000u, 1u, 2u, 240u, 0u);
  openrelay_fx_set_effective_format(48000u, 1u, 2u, 240u, 0u);
  hold_arm(NULL, NULL);
  atomic_store(&stop_done, 0);
  return openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES) == OPENRELAY_CAPTURE_OK &&
         cap != NULL;
}

static int report(const char *name) {
  if (fails) {
    printf("RESULT=RED %s fails=%d\n", name, fails);
    return 1;
  }
  printf("RESULT=PASS %s\n", name);
  return 0;
}

/* Caso 1: get_counters retenido dentro de la operacion. destroy -> BUSY y owner
 * igual; al liberar el retenido, get_counters OK y destroy OK con owner NULL. */
static int case_counters_retained(void) {
  struct job g;
  pthread_t t;
  openrelay_capture *own;
  int32_t rc;

  SETUP(setup());
  SETUP(openrelay_capture_start(cap) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_stop(cap) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_close(cap) == OPENRELAY_CAPTURE_OK);
  g.cap = cap;
  hold_arm("counters_admitted", hold_fn);
  SETUP(pthread_create(&t, NULL, job_counters, &g) == 0);
  SETUP(wait_entered());
  own = cap;
  rc = openrelay_capture_destroy(&own);
  printf("ORACLE destroy_while_counters rc=%d owner_unchanged=%d\n", rc, own == cap);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_BUSY);
  CHECK(own == cap);
  release_hold();
  pthread_join(t, NULL);
  printf("ORACLE counters_after_release rc=%d gen=%llu written=%llu\n", g.rc,
         (unsigned long long)g.ctr.generation, (unsigned long long)g.ctr.samples_written);
  CHECK(g.rc == OPENRELAY_CAPTURE_OK);
  CHECK(g.ctr.generation == 2u);
  CHECK(g.ctr.samples_written == 0u);
  rc = openrelay_capture_destroy(&own);
  printf("ORACLE destroy_retry rc=%d owner_null=%d\n", rc, own == NULL);
  CHECK(rc == OPENRELAY_CAPTURE_OK);
  CHECK(own == NULL);
  return report("counters_retained_busy_then_ok");
}

/* Caso 2: notas con token viejo, cero o desconocido no mutan; el token vigente
 * se aplica en el siguiente control (get_status). */
static int case_stale_token(void) {
  openrelay_capture_status st;
  int32_t rc;
  uint32_t n;
  int16_t dst[NS];

  SETUP(setup());
  SETUP(openrelay_capture_start(cap) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_stop(cap) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_start(cap) == OPENRELAY_CAPTURE_OK); /* generacion 3 */
  CHECK(openrelay_capture_note_error(cap, 1u, -42) == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(openrelay_capture_note_disconnect(cap, 1u) == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(openrelay_capture_note_error(cap, 0u, -42) == OPENRELAY_CAPTURE_ERR_CLOSED);
  SETUP(openrelay_capture_get_status(cap, &st) == OPENRELAY_CAPTURE_OK);
  printf("ORACLE stale_notes state=%u gen=%llu last_error=%d\n", st.state,
         (unsigned long long)st.generation, st.last_error);
  CHECK(st.state == OPENRELAY_CAPTURE_STATE_STARTED);
  CHECK(st.generation == 3u);
  CHECK(st.last_error == OPENRELAY_CAPTURE_OK);
  CHECK(openrelay_capture_push(cap, 1u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(openrelay_capture_push(cap, 3u, buf, NS) == OPENRELAY_CAPTURE_OK);
  CHECK(openrelay_capture_note_error(cap, 3u, -42) == OPENRELAY_CAPTURE_OK);
  CHECK(openrelay_capture_push(cap, 3u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  SETUP(openrelay_capture_get_status(cap, &st) == OPENRELAY_CAPTURE_OK);
  printf("ORACLE current_note state=%u gen=%llu last_error=%d\n", st.state,
         (unsigned long long)st.generation, st.last_error);
  CHECK(st.state == OPENRELAY_CAPTURE_STATE_ERROR);
  CHECK(st.generation == 4u);
  CHECK(st.last_error == -42);
  n = NS;
  rc = openrelay_capture_read(cap, dst, &n);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(n == 0u);
  SETUP(openrelay_capture_close(cap) == OPENRELAY_CAPTURE_OK);
  rc = openrelay_capture_destroy(&cap);
  CHECK(rc == OPENRELAY_CAPTURE_OK && cap == NULL);
  return report("stale_token_no_mutation_current_applied");
}

/* Caso 3: disconnect vigente cierra el gate; stop aplica el evento antes de
 * decidir y falla con ERR_STATE sin cambiar a STOPPED. */
static int case_disconnect_control(void) {
  openrelay_capture_status st;
  int32_t rc;
  uint32_t n;
  int16_t dst[NS];

  SETUP(setup());
  SETUP(openrelay_capture_start(cap) == OPENRELAY_CAPTURE_OK);
  CHECK(openrelay_capture_note_disconnect(cap, 1u) == OPENRELAY_CAPTURE_OK);
  CHECK(openrelay_capture_push(cap, 1u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(openrelay_capture_note_disconnect(cap, 1u) == OPENRELAY_CAPTURE_ERR_CLOSED);
  rc = openrelay_capture_stop(cap);
  printf("ORACLE stop_applies_event rc=%d\n", rc);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_STATE);
  SETUP(openrelay_capture_get_status(cap, &st) == OPENRELAY_CAPTURE_OK);
  printf("ORACLE disconnect state=%u gen=%llu last_error=%d\n", st.state,
         (unsigned long long)st.generation, st.last_error);
  CHECK(st.state == OPENRELAY_CAPTURE_STATE_DISCONNECTED);
  CHECK(st.generation == 2u);
  CHECK(st.last_error == OPENRELAY_CAPTURE_ERR_CLOSED);
  n = NS;
  rc = openrelay_capture_read(cap, dst, &n);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(n == 0u);
  SETUP(openrelay_capture_close(cap) == OPENRELAY_CAPTURE_OK);
  rc = openrelay_capture_destroy(&cap);
  CHECK(rc == OPENRELAY_CAPTURE_OK && cap == NULL);
  return report("note_disconnect_applied_by_control");
}

/* Caso 4: push admitido con gate valido retenido dentro; stop no retorna hasta
 * que ese push sale. Despues de STOPPED, token viejo no publica; un nuevo start
 * (generacion 3) publica solo con su token. */
static int case_stop_window(void) {
  struct job p, s;
  pthread_t tp, ts;
  openrelay_capture_counters ctr;
  openrelay_capture_status st;
  int i;
  openrelay_capture *own;
  int32_t rc;

  SETUP(setup());
  SETUP(openrelay_capture_start(cap) == OPENRELAY_CAPTURE_OK);
  p.cap = cap;
  p.gen = 1u;
  hold_arm("push_gate_ok", hold_fn);
  SETUP(pthread_create(&tp, NULL, job_push, &p) == 0);
  SETUP(wait_entered());
  s.cap = cap;
  SETUP(pthread_create(&ts, NULL, job_stop, &s) == 0);
  for (i = 0; i < 200000 && atomic_load(&stop_done) == 0; i++) {
    sched_yield();
  }
  printf("ORACLE stop_while_push_held stop_done=%d\n", atomic_load(&stop_done));
  CHECK(atomic_load(&stop_done) == 0);
  release_hold();
  pthread_join(tp, NULL);
  pthread_join(ts, NULL);
  printf("ORACLE window push_rc=%d stop_rc=%d\n", p.rc, s.rc);
  CHECK(p.rc == OPENRELAY_CAPTURE_OK);
  CHECK(s.rc == OPENRELAY_CAPTURE_OK);
  CHECK(atomic_load(&stop_done) == 1);
  SETUP(openrelay_capture_get_status(cap, &st) == OPENRELAY_CAPTURE_OK);
  CHECK(st.state == OPENRELAY_CAPTURE_STATE_STOPPED);
  CHECK(st.generation == 2u);
  SETUP(openrelay_capture_get_counters(cap, &ctr) == OPENRELAY_CAPTURE_OK);
  CHECK(ctr.samples_written == NS);
  CHECK(openrelay_capture_push(cap, 1u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  SETUP(openrelay_capture_start(cap) == OPENRELAY_CAPTURE_OK); /* generacion 3 */
  CHECK(openrelay_capture_push(cap, 1u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(openrelay_capture_push(cap, 3u, buf, NS) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_stop(cap) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_close(cap) == OPENRELAY_CAPTURE_OK);
  own = cap;
  rc = openrelay_capture_destroy(&own);
  CHECK(rc == OPENRELAY_CAPTURE_OK && own == NULL);
  return report("publish_window_vs_stop_restart");
}

static int32_t g_enable_push_rc = -99;
static uint64_t g_enable_tok;

static void enable_cb(void) {
  g_enable_tok = openrelay_fx_enable_token();
  g_enable_push_rc = openrelay_capture_push(cap, g_enable_tok, buf, NS);
}

/* Caso 5: push desde el callback durante enable (estado STARTING, antes de
 * STARTED) no publica muestras ni cuenta callbacks. */
static int case_enable_callback(void) {
  openrelay_capture_counters ctr;
  int32_t rc;

  SETUP(setup());
  hold_arm("enable_done", enable_cb);
  rc = openrelay_capture_start(cap);
  printf("ORACLE enable_cb tok=%llu push_rc=%d start_rc=%d\n",
         (unsigned long long)g_enable_tok, g_enable_push_rc, rc);
  CHECK(rc == OPENRELAY_CAPTURE_OK);
  CHECK(g_enable_tok == 1u);
  CHECK(g_enable_push_rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  SETUP(openrelay_capture_get_counters(cap, &ctr) == OPENRELAY_CAPTURE_OK);
  printf("ORACLE enable_cb counters written=%llu callbacks=%llu\n",
         (unsigned long long)ctr.samples_written, (unsigned long long)ctr.callbacks);
  CHECK(ctr.samples_written == 0u);
  CHECK(ctr.callbacks == 0u);
  SETUP(openrelay_capture_stop(cap) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_close(cap) == OPENRELAY_CAPTURE_OK);
  rc = openrelay_capture_destroy(&cap);
  CHECK(rc == OPENRELAY_CAPTURE_OK && cap == NULL);
  return report("enable_callback_push_not_published");
}

/* Caso 6: effective distinto de requested -> ERR_FORMAT, sin enable ni
 * generacion nueva ni publicacion. */
static int case_effective_mismatch(void) {
  openrelay_capture_status st;
  openrelay_capture_counters ctr;
  int32_t rc;

  SETUP(setup());
  openrelay_fx_set_effective_format(44100u, 1u, 2u, 240u, 0u);
  rc = openrelay_capture_start(cap);
  printf("ORACLE mismatch start_rc=%d enable_calls=%u\n", rc, openrelay_fx_calls(OPENRELAY_FX_ENABLE));
  CHECK(rc == OPENRELAY_CAPTURE_ERR_FORMAT);
  CHECK(openrelay_fx_calls(OPENRELAY_FX_ENABLE) == 0u);
  CHECK(openrelay_fx_enabled() == 0);
  SETUP(openrelay_capture_get_status(cap, &st) == OPENRELAY_CAPTURE_OK);
  CHECK(st.state == OPENRELAY_CAPTURE_STATE_ERROR);
  CHECK(st.generation == 0u);
  CHECK(st.last_error == OPENRELAY_CAPTURE_ERR_FORMAT);
  CHECK(openrelay_capture_push(cap, 0u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(openrelay_capture_push(cap, 1u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  SETUP(openrelay_capture_get_counters(cap, &ctr) == OPENRELAY_CAPTURE_OK);
  CHECK(ctr.samples_written == 0u);
  SETUP(openrelay_capture_close(cap) == OPENRELAY_CAPTURE_OK);
  rc = openrelay_capture_destroy(&cap);
  CHECK(rc == OPENRELAY_CAPTURE_OK && cap == NULL);
  return report("effective_mismatch_no_publish_no_enable");
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "uso: test_events <caso>\n");
    return 2;
  }
  if (strcmp(argv[1], "counters_retained") == 0) return case_counters_retained();
  if (strcmp(argv[1], "stale_token") == 0) return case_stale_token();
  if (strcmp(argv[1], "disconnect_control") == 0) return case_disconnect_control();
  if (strcmp(argv[1], "stop_window") == 0) return case_stop_window();
  if (strcmp(argv[1], "enable_callback") == 0) return case_enable_callback();
  if (strcmp(argv[1], "effective_mismatch") == 0) return case_effective_mismatch();
  fprintf(stderr, "caso desconocido: %s\n", argv[1]);
  return 2;
}
