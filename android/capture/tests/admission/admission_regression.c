/* admission_regression.c - regresion determinista de falsa contencion en adm_enter.
 * Usa el core publico real (openrelay_capture_*) y el owner privado. Los hooks
 * openrelay_test_* solo existen en la copia instrumentada de life.c y solo cambian
 * el scheduling: un getter REAL (openrelay_capture_get_counters) se retiene dentro de
 * su admision mientras push intenta su CAS, para que el CAS de push falle.
 * No se fabrican retornos ni se toca adm directamente. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "capture-abi.h"
#include "capture-owner.h"

static int checks, failures;
#define CHECK(c, msg)                                                  \
  do {                                                                 \
    checks++;                                                          \
    if (!(c)) { failures++; printf("FAIL %s\n", msg); }               \
    else printf("ok %s\n", msg);                                        \
  } while (0)

#define ADM_PUSH_UNIT UINT64_C(1)

/* ---- handshake getter retenido (solo scheduling) ---- */
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int g_armed;        /* hooks activos (solo caso forzado) */
static int g_forced_left;  /* fallos CAS de push pendientes de forzar */
static int g_hook_idx;
static int g_req_enter, g_served, g_inside, g_leave_req, g_done, g_stop;
static __thread int tl_is_getter;
static openrelay_capture *g_cap;

/* Llamado en el anclaje de adm_enter, antes del CAS. */
void openrelay_test_before_adm_cas(openrelay_capture *cap, uint64_t unit) {
  int k, d;
  if (!g_armed || unit != ADM_PUSH_UNIT || g_forced_left == 0 || tl_is_getter) return;
  (void)cap;
  g_forced_left--;
  k = g_hook_idx++;
  pthread_mutex_lock(&mu);
  if (k % 2 == 0) { /* entra el getter: adm cambia, CAS de push falla */
    g_req_enter++;
    pthread_cond_broadcast(&cv);
    while (!g_inside) pthread_cond_wait(&cv, &mu);
  } else { /* sale el getter: adm vuelve, CAS de push falla */
    d = g_done;
    g_leave_req = 1;
    pthread_cond_broadcast(&cv);
    while (g_done == d) pthread_cond_wait(&cv, &mu);
  }
  pthread_mutex_unlock(&mu);
}

/* Llamado dentro de la ventana admitida de get_counters (tras adm_enter OK). */
void openrelay_test_getter_window(openrelay_capture *cap) {
  (void)cap;
  if (!g_armed || !tl_is_getter) return;
  pthread_mutex_lock(&mu);
  g_inside = 1;
  pthread_cond_broadcast(&cv);
  while (!g_leave_req) pthread_cond_wait(&cv, &mu);
  g_leave_req = 0;
  g_inside = 0;
  pthread_mutex_unlock(&mu);
}

static void *getter_main(void *arg) {
  openrelay_capture_counters c;
  tl_is_getter = 1;
  (void)arg;
  for (;;) {
    pthread_mutex_lock(&mu);
    while (g_req_enter == g_served && !g_stop) pthread_cond_wait(&cv, &mu);
    if (g_stop) { pthread_mutex_unlock(&mu); break; }
    g_served++;
    pthread_mutex_unlock(&mu);
    (void)openrelay_capture_get_counters(g_cap, &c);
    pthread_mutex_lock(&mu);
    g_done++;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
  }
  return NULL;
}

static void hook_reset(void) {
  g_armed = 0; g_forced_left = 0; g_hook_idx = 0;
  g_req_enter = g_served = g_inside = g_leave_req = g_done = g_stop = 0;
}

static const int16_t PAT[240] = {0};
static int16_t samples_buf[240];

static int32_t stop_close(openrelay_capture *cap) {
  if (openrelay_capture_stop(cap) != OPENRELAY_CAPTURE_OK) return -99;
  return openrelay_capture_close(cap);
}

static openrelay_capture *start_cap(void) {
  openrelay_capture *cap = NULL;
  openrelay_capture_status st;
  extern void openrelay_fx_reset(void);
  extern void openrelay_fx_set_prepare_format(uint32_t, uint16_t, uint16_t, uint16_t, uint16_t);
  extern void openrelay_fx_set_effective_format(uint32_t, uint16_t, uint16_t, uint16_t, uint16_t);
  openrelay_fx_reset();
  openrelay_fx_set_prepare_format(48000u, 1u, 2u, 240u, 0u);
  openrelay_fx_set_effective_format(48000u, 1u, 2u, 240u, 0u);
  if (openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES) != OPENRELAY_CAPTURE_OK) return NULL;
  if (openrelay_capture_start(cap) != OPENRELAY_CAPTURE_OK) return NULL;
  if (openrelay_capture_get_status(cap, &st) != OPENRELAY_CAPTURE_OK) return NULL;
  return cap;
}

/* Caso forzado: n fallos CAS de push con getter retenido. Devuelve rc de push. */
static int32_t forced_push(openrelay_capture_owner *ow, openrelay_capture *cap, uint64_t gen, int nfail, uint32_t *written_out) {
  openrelay_capture_counters c;
  openrelay_capture *got = NULL;
  pthread_t th;
  int32_t rc;
  memset(samples_buf, 0, sizeof samples_buf);
  g_cap = cap;
  hook_reset();
  pthread_create(&th, NULL, getter_main, NULL);
  g_armed = 1;
  g_forced_left = nfail;
  if (openrelay_owner_acquire(ow, &got) != OPENRELAY_CAPTURE_OK) { g_armed = 0; rc = OPENRELAY_CAPTURE_ERR_BUSY; }
  else { rc = openrelay_capture_push(got, gen, PAT, 240u); openrelay_owner_release(ow); }
  g_armed = 0;
  /* libera getter retenido y une el hilo antes de cualquier free */
  pthread_mutex_lock(&mu);
  if (g_inside) { g_leave_req = 1; pthread_cond_broadcast(&cv); }
  pthread_mutex_unlock(&mu);
  while (1) {
    pthread_mutex_lock(&mu);
    int idle = (g_served == g_done) && !g_inside;
    if (idle) g_stop = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    if (idle) break;
  }
  pthread_join(th, NULL);
  (void)openrelay_capture_get_counters(cap, &c);
  if (written_out) *written_out = (uint32_t)c.samples_written;
  return rc;
}

/* Stress valido: 1 productor, 1 consumidor, 1 getter, todos con owner. */
typedef struct { openrelay_capture_owner *ow; uint64_t gen; long ok_samples; long rej; } prod_t;
typedef struct { openrelay_capture_owner *ow; long read_samples; } cons_t;
typedef struct { openrelay_capture_owner *ow; } getter_t;
#define STRESS_ITERS 2000

static void *prod_run(void *p) {
  prod_t *a = p; int i; int16_t buf[240];
  memset(buf, 1, sizeof buf);
  for (i = 0; i < STRESS_ITERS; i++) {
    openrelay_capture *got = NULL;
    int32_t rc;
    if (openrelay_owner_acquire(a->ow, &got) != OPENRELAY_CAPTURE_OK) { a->rej++; continue; }
    rc = openrelay_capture_push(got, a->gen, buf, 240u);
    openrelay_owner_release(a->ow);
    if (rc == OPENRELAY_CAPTURE_OK) a->ok_samples += 240; else a->rej++;
  }
  return NULL;
}
static void *cons_run(void *p) {
  cons_t *a = p; int i; int16_t buf[240];
  for (i = 0; i < STRESS_ITERS; i++) {
    openrelay_capture *got = NULL; uint32_t n = 240u; int32_t rc;
    if (openrelay_owner_acquire(a->ow, &got) != OPENRELAY_CAPTURE_OK) continue;
    rc = openrelay_capture_read(got, buf, &n);
    openrelay_owner_release(a->ow);
    if (rc == OPENRELAY_CAPTURE_OK) a->read_samples += n;
  }
  return NULL;
}
static void *getter_run(void *p) {
  getter_t *a = p; int i; openrelay_capture_counters c;
  for (i = 0; i < STRESS_ITERS; i++) {
    openrelay_capture *got = NULL;
    if (openrelay_owner_acquire(a->ow, &got) != OPENRELAY_CAPTURE_OK) continue;
    (void)openrelay_capture_get_counters(got, &c);
    openrelay_owner_release(a->ow);
  }
  return NULL;
}

int main(void) {
  openrelay_capture *cap;
  openrelay_capture_owner ow;
  openrelay_capture_counters ctr;
  openrelay_capture_owner own1, own2;
  uint64_t gen;
  uint32_t written = 0;
  int32_t rc;
  pthread_t tp, tc, tg;
  prod_t pr = {0};
  cons_t co = {0};
  getter_t gt = {0};
  int forced_ok = 1;

  /* P1: positivo: 7 fallos CAS, el 8.o intento admite. */
  cap = start_cap();
  CHECK(cap != NULL, "P1 start");
  if (cap) {
    gen = 0;
    {
      openrelay_capture_status st; openrelay_capture_get_status(cap, &st); gen = st.generation;
    }
    openrelay_owner_init(&own1, cap);
    rc = forced_push(&own1, cap, gen, 7, &written);
    CHECK(rc == OPENRELAY_CAPTURE_OK, "P1 forced7 push OK (7 CAS fallos, 8.o intento)");
    CHECK(written == 240u, "P1 forced7 samples_written=240");
    CHECK(stop_close(cap) == OPENRELAY_CAPTURE_OK, "P1 close");
    CHECK(openrelay_owner_try_destroy(&own1) == OPENRELAY_CAPTURE_OK && own1.cap == NULL, "P1 owner destroy");
  }

  /* R1: reproduccion: 8 fallos CAS con STARTED y token valido. */
  cap = start_cap();
  CHECK(cap != NULL, "R1 start");
  if (cap) {
    openrelay_capture_status st; openrelay_capture_get_status(cap, &st); gen = st.generation;
    openrelay_owner_init(&own2, cap);
    rc = forced_push(&own2, cap, gen, 8, &written);
    if (rc != OPENRELAY_CAPTURE_OK) {
      forced_ok = 0;
      printf("RED forced8 push rc=%d want 0 (STARTED, token valido, sin destroy)\n", rc);
    } else {
      printf("ok R1 forced8 push OK\n");
    }
    CHECK(stop_close(cap) == OPENRELAY_CAPTURE_OK, "R1 close");
    CHECK(openrelay_owner_try_destroy(&own2) == OPENRELAY_CAPTURE_OK && own2.cap == NULL, "R1 owner destroy");
  }

  /* N1: negativa fail-closed: token viejo rechazado, sin copia. */
  cap = start_cap();
  CHECK(cap != NULL, "N1 start");
  if (cap) {
    openrelay_capture_status st; openrelay_capture_get_status(cap, &st);
    memset(samples_buf, 0x5A, sizeof samples_buf);
    rc = openrelay_capture_push(cap, st.generation + 1u, PAT, 240u);
    CHECK(rc == OPENRELAY_CAPTURE_ERR_CLOSED, "N1 stale token => ERR_CLOSED");
    CHECK(openrelay_capture_get_counters(cap, &ctr) == OPENRELAY_CAPTURE_OK && ctr.samples_written == 0u,
          "N1 stale token no escribe");
    CHECK(stop_close(cap) == OPENRELAY_CAPTURE_OK, "N1 close");
    rc = openrelay_capture_push(cap, st.generation, PAT, 240u);
    CHECK(rc == OPENRELAY_CAPTURE_ERR_CLOSED, "N1 push tras close => ERR_CLOSED");
    CHECK(openrelay_capture_destroy(&cap) == OPENRELAY_CAPTURE_OK && cap == NULL, "N1 destroy");
  }

  /* S1: stress valido con owner: 1 productor, 1 consumidor, 1 getter. */
  cap = start_cap();
  CHECK(cap != NULL, "S1 start");
  if (cap) {
    openrelay_capture_status st; openrelay_capture_get_status(cap, &st);
    openrelay_owner_init(&ow, cap);
    pr.ow = &ow; pr.gen = st.generation; co.ow = &ow; gt.ow = &ow;
    pthread_create(&tp, NULL, prod_run, &pr);
    pthread_create(&tc, NULL, cons_run, &co);
    pthread_create(&tg, NULL, getter_run, &gt);
    pthread_join(tp, NULL); pthread_join(tc, NULL); pthread_join(tg, NULL);
    openrelay_capture_get_counters(cap, &ctr);
    printf("STRESS_FALSE_REJECTS=%ld of %d pushes (informativo, no oraculo)\n", pr.rej, STRESS_ITERS);
    CHECK(ctr.samples_written + ctr.samples_dropped == (uint64_t)pr.ok_samples,
          "S1 oraculo: written+dropped == muestras de push OK");
    CHECK(ctr.samples_read == (uint64_t)co.read_samples, "S1 oraculo: samples_read == leidas");
    stop_close(cap);
    CHECK(openrelay_owner_try_destroy(&ow) == OPENRELAY_CAPTURE_OK && ow.cap == NULL, "S1 owner destroy after joins");
  }

  printf("ADMISSION_CASES checks=%d fails=%d\n", checks, failures);
  if (failures) { printf("RESULT=FAIL admission regression\n"); return 1; }
  if (!forced_ok) { printf("RESULT=RED forced8 false containment reproduced\n"); return 2; }
  printf("RESULT=PASS admission regression\n");
  return 0;
}
