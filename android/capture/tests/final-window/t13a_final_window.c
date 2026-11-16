/* t13a_final_window.c - dos oraculos deterministas de ventanas residuales de
 * lifecycle bajo owner. Solo test: no es producto ni cambia el core.
 *
 *   window : la ventana gate abierto (gate_token=tok) antes de set_state(STARTED).
 *            Un push con el token vigente, hecho desde el hook de control entre
 *            ambas lineas y con prestamo owner, debe rechazarse (ERR_CLOSED) y no
 *            publicar muestras ni callbacks mientras STARTING. Control positivo:
 *            tras start OK, push con el mismo token se acepta.
 *   reject : un borrower con prestamo owner entra en la rama de rechazo de push
 *            (token viejo, gate cerrado), queda pausado ANTES de adm_leave (fetch_sub
 *            pendiente). owner try_destroy debe ser BUSY sin llamar al core destroy
 *            (retire no se invoca). Tras liberar el prestamo, retry OK, cap NULL.
 *
 * Exit: 0 PASS, 10 RED (oraculo window falla: ventana reproducida), 1 FAIL/SETUP.
 * Hooks arl_t13a_hook_* solo existen en la copia instrumentada (instrument.py).
 */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "capture-abi.h"
#include "capture-owner.h"
#include "backend_fixture.h"

#define WAIT_S 10
#define MODE_NONE 0
#define MODE_WINDOW 1
#define MODE_REJECT 2

static int fails = 0;
#define CHECK(c, msg)                                   \
  do {                                                  \
    if (!(c)) {                                         \
      fails++;                                          \
      printf("FAIL %s\n", msg);                         \
    } else {                                            \
      printf("ok %s\n", msg);                           \
    }                                                   \
  } while (0)

static arl_capture_owner g_owner;
static int g_mode = MODE_NONE;
static int16_t g_samples[ARL_CAPTURE_FRAME_SAMPLES];

/* Estado del oraculo window (solo hilo de control). */
static int g_win_hits = 0;
static int32_t g_win_push_rc = 99;
static int32_t g_win_owner_rc = 99;
static uint64_t g_win_written = 99u;
static uint64_t g_win_cb = 99u;

/* Sincronia del oraculo reject. */
static pthread_mutex_t g_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_cv = PTHREAD_COND_INITIALIZER;
static int g_paused = 0;
static int g_resume = 0;
static int g_hook_timeout = 0;
static int32_t g_b_acq_rc = 99;
static int32_t g_b_push_rc = 99;
static uint64_t g_stale_gen = 1u; /* token del primer start: ya no vigente */

/* Hook en la ventana gate-abierto/STARTING. Corre en el hilo de control, dentro de
 * arl_capture_start, con un prestamo owner propio (anidado al del control). */
void arl_t13a_hook_start_gate_open(arl_capture *cap, uint64_t tok) {
  arl_capture *c = NULL;
  arl_capture_counters k;
  int32_t rc;

  if (g_mode != MODE_WINDOW) return;
  g_win_hits++;
  rc = arl_owner_acquire(&g_owner, &c);
  g_win_owner_rc = rc;
  if (rc != ARL_CAPTURE_OK || c != cap) return;
  g_win_push_rc = arl_capture_push(c, tok, g_samples, ARL_CAPTURE_FRAME_SAMPLES);
  arl_owner_release(&g_owner);
  if (arl_capture_get_counters(cap, &k) == ARL_CAPTURE_OK) {
    g_win_written = k.samples_written;
    g_win_cb = k.callbacks;
  }
}

/* Hook en la rama de rechazo de push: adm_enter ya admitido, adm_leave pendiente.
 * Pausa el borrower hasta que el hilo principal libera (timeout acotado). */
void arl_t13a_hook_push_reject(arl_capture *cap, uint64_t generation) {
  struct timespec ts;
  int rc = 0;

  (void)cap;
  (void)generation;
  if (g_mode != MODE_REJECT) return;
  pthread_mutex_lock(&g_mu);
  g_paused = 1;
  pthread_cond_broadcast(&g_cv);
  clock_gettime(CLOCK_REALTIME, &ts);
  ts.tv_sec += WAIT_S;
  while (!g_resume && rc == 0) {
    rc = pthread_cond_timedwait(&g_cv, &g_mu, &ts);
  }
  if (!g_resume) g_hook_timeout = 1;
  pthread_mutex_unlock(&g_mu);
}

static void *borrower(void *arg) {
  arl_capture *c = NULL;
  (void)arg;
  g_b_acq_rc = arl_owner_acquire(&g_owner, &c);
  if (g_b_acq_rc == ARL_CAPTURE_OK) {
    g_b_push_rc = arl_capture_push(c, g_stale_gen, g_samples, ARL_CAPTURE_FRAME_SAMPLES);
    arl_owner_release(&g_owner);
  }
  return NULL;
}

static void set_pcm_format(void) {
  arl_fx_set_prepare_format(ARL_CAPTURE_SAMPLE_RATE_HZ, ARL_CAPTURE_CHANNELS,
                            ARL_CAPTURE_BYTES_PER_SAMPLE, ARL_CAPTURE_FRAME_SAMPLES, 0u);
  arl_fx_set_effective_format(ARL_CAPTURE_SAMPLE_RATE_HZ, ARL_CAPTURE_CHANNELS,
                              ARL_CAPTURE_BYTES_PER_SAMPLE, ARL_CAPTURE_FRAME_SAMPLES, 0u);
}

static int run_window(void) {
  arl_capture *cap = NULL;
  arl_capture *c = NULL;
  arl_capture_status st;
  arl_capture_counters k;
  int32_t rc;
  int window_ok;

  memset(g_samples, 0, sizeof g_samples);
  arl_fx_reset();
  set_pcm_format();
  rc = arl_capture_open(&cap, ARL_CAPTURE_RING_SAMPLES);
  CHECK(rc == ARL_CAPTURE_OK && cap != NULL, "open capture");
  CHECK(arl_owner_init(&g_owner, cap) == ARL_CAPTURE_OK, "owner init");

  g_mode = MODE_WINDOW;
  CHECK(arl_owner_acquire(&g_owner, &c) == ARL_CAPTURE_OK && c == cap, "control acquire");
  rc = arl_capture_start(c);
  arl_owner_release(&g_owner);
  g_mode = MODE_NONE;
  CHECK(rc == ARL_CAPTURE_OK, "start returns OK");
  CHECK(g_win_hits == 1, "window hook ran exactly once");
  CHECK(g_win_owner_rc == ARL_CAPTURE_OK, "producer owner loan in window");

  /* Oraculo window: durante STARTING el push debe rechazarse sin publicar. */
  window_ok = (g_win_push_rc == ARL_CAPTURE_ERR_CLOSED) && g_win_written == 0u && g_win_cb == 0u;
  printf("WINDOW push_rc=%d written=%llu callbacks=%llu\n", (int)g_win_push_rc,
         (unsigned long long)g_win_written, (unsigned long long)g_win_cb);
  if (window_ok) {
    printf("ok window push in STARTING rejected, 0 samples, 0 callbacks\n");
  } else {
    printf("RED window gate open before STARTED: push accepted during STARTING\n");
  }

  /* Control positivo: tras start OK, un push con el token vigente se acepta. */
  CHECK(arl_owner_acquire(&g_owner, &c) == ARL_CAPTURE_OK, "control positive acquire");
  CHECK(arl_capture_get_status(c, &st) == ARL_CAPTURE_OK && st.state == ARL_CAPTURE_STATE_STARTED,
        "state STARTED after start OK");
  rc = arl_capture_push(c, st.generation, g_samples, ARL_CAPTURE_FRAME_SAMPLES);
  CHECK(rc == ARL_CAPTURE_OK, "control positive push OK after STARTED");
  CHECK(arl_capture_get_counters(c, &k) == ARL_CAPTURE_OK && k.callbacks >= 1u &&
            k.samples_written >= ARL_CAPTURE_FRAME_SAMPLES,
        "control positive counters advance after push");
  /* Limpieza: stop, close, destroy, todo por owner. */
  CHECK(arl_capture_stop(c) == ARL_CAPTURE_OK, "stop via loan");
  arl_owner_release(&g_owner);
  CHECK(arl_owner_acquire(&g_owner, &c) == ARL_CAPTURE_OK, "acquire for close");
  CHECK(arl_capture_close(c) == ARL_CAPTURE_OK, "close via loan");
  arl_owner_release(&g_owner);
  CHECK(arl_owner_try_destroy(&g_owner) == ARL_CAPTURE_OK && g_owner.cap == NULL,
        "destroy OK when quiescent, cap NULL");

  if (fails) return 1;
  if (!window_ok) return 10;
  return 0;
}

static int run_reject(void) {
  arl_capture *cap = NULL;
  arl_capture *c = NULL;
  pthread_t th;
  uint32_t retire_before, retire_after, release_before, release_after;
  int32_t rc;

  memset(g_samples, 0, sizeof g_samples);
  arl_fx_reset();
  set_pcm_format();
  rc = arl_capture_open(&cap, ARL_CAPTURE_RING_SAMPLES);
  CHECK(rc == ARL_CAPTURE_OK && cap != NULL, "open capture (reject)");
  CHECK(arl_owner_init(&g_owner, cap) == ARL_CAPTURE_OK, "owner init (reject)");

  /* Start normal (sin hook), luego stop/close: queda gate_token=0 y token viejo. */
  CHECK(arl_owner_acquire(&g_owner, &c) == ARL_CAPTURE_OK, "acquire start");
  CHECK(arl_capture_start(c) == ARL_CAPTURE_OK, "start (reject setup)");
  CHECK(arl_capture_stop(c) == ARL_CAPTURE_OK, "stop (reject setup)");
  CHECK(arl_capture_close(c) == ARL_CAPTURE_OK, "close (reject setup)");
  arl_owner_release(&g_owner);

  /* Borrower: push con token viejo -> rama de rechazo, pausado antes de adm_leave. */
  g_mode = MODE_REJECT;
  CHECK(pthread_create(&th, NULL, borrower, NULL) == 0, "borrower thread created");
  pthread_mutex_lock(&g_mu);
  {
    struct timespec ts;
    int wrc = 0;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_sec += WAIT_S;
    while (!g_paused && wrc == 0) wrc = pthread_cond_timedwait(&g_cv, &g_mu, &ts);
  }
  pthread_mutex_unlock(&g_mu);
  CHECK(g_paused == 1, "borrower paused in reject branch before adm_leave");
  if (g_paused != 1) {
    /* Libera el hilo si quedo colgado para no bloquear el join. */
    pthread_mutex_lock(&g_mu);
    g_resume = 1;
    pthread_cond_broadcast(&g_cv);
    pthread_mutex_unlock(&g_mu);
    pthread_join(th, NULL);
    return 1;
  }

  /* Oraculo reject, parte 1: destroy BUSY por prestamo vivo, sin llegar al core. */
  retire_before = arl_fx_calls(ARL_FX_RETIRE);
  release_before = arl_fx_calls(ARL_FX_RELEASE);
  rc = arl_owner_try_destroy(&g_owner);
  retire_after = arl_fx_calls(ARL_FX_RETIRE);
  release_after = arl_fx_calls(ARL_FX_RELEASE);
  CHECK(rc == ARL_CAPTURE_ERR_BUSY, "owner try_destroy BUSY with pending borrower");
  CHECK(g_owner.cap == cap, "owner cap unchanged after BUSY");
  CHECK(retire_after == retire_before && release_after == release_before,
        "core destroy not reached (no retire/release calls)");

  /* Libera al borrower y espera al join. */
  pthread_mutex_lock(&g_mu);
  g_resume = 1;
  pthread_cond_broadcast(&g_cv);
  pthread_mutex_unlock(&g_mu);
  pthread_join(th, NULL);
  g_mode = MODE_NONE;
  CHECK(!g_hook_timeout, "reject hook did not timeout");
  CHECK(g_b_acq_rc == ARL_CAPTURE_OK, "borrower acquire OK");
  CHECK(g_b_push_rc == ARL_CAPTURE_ERR_CLOSED, "borrower stale-token push rejected");

  /* Oraculo reject, parte 2: tras liberar el prestamo, retry OK y cap NULL. */
  rc = arl_owner_try_destroy(&g_owner);
  CHECK(rc == ARL_CAPTURE_OK && g_owner.cap == NULL, "retry try_destroy OK, cap NULL");
  CHECK(arl_fx_calls(ARL_FX_RELEASE) == release_before + 1u, "core release called once on retry");

  printf("OWNER_SMOKE_CASES fails=%d\n", fails);
  return fails ? 1 : 0;
}

int main(int argc, char **argv) {
  int rc;
  const char *which = argc > 1 ? argv[1] : "";

  if (strcmp(which, "window") == 0) {
    rc = run_window();
    if (rc == 0) printf("RESULT=PASS window\n");
    else if (rc == 10) printf("RESULT=RED window gate-before-STARTED reproduced\n");
    else printf("RESULT=FAIL window setup/control\n");
    return rc;
  }
  if (strcmp(which, "reject") == 0) {
    rc = run_reject();
    printf(rc ? "RESULT=FAIL reject\n" : "RESULT=PASS reject\n");
    return rc;
  }
  printf("usage: t13a_final_window window|reject\n");
  return 1;
}
