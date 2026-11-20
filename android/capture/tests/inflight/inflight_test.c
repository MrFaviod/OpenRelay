/* Life inflight regressions (read/drain en vuelo frente a destroy).
 *
 * Un proceso = un escenario:
 *   read     : consumidor retenido DENTRO de openrelay_capture_read (tras __real_openrelay_spsc_read).
 *   drain    : consumidor retenido DENTRO de openrelay_capture_drain (tras __real_openrelay_spsc_drain).
 *   control  : sin retencion; consumidor termina, luego destroy debe ser OK.
 *
 * Costura: --wrap de openrelay_spsc_read/drain. El wrapper ejecuta la operacion real
 * y espera con mutex/condvar de test ANTES de devolver al motor. El motor usa
 * el objeto real; no hay maquina de estados ni flags de backend como sustituto.
 *
 * Codigos de salida: 0 PASS, 1 FAIL de aserto (comportamiento), 2 SETUP_FAIL
 * (harness: no se alcanzo el wrapper, timeout, paso de setup fallido).
 * Limpieza RED: si destroy libera con el consumidor dentro, el wrapper no
 * vuelve al motor liberado; hace pthread_exit tras despertar.
 */
#define _POSIX_C_SOURCE 200809L

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "capture-abi.h"
#include "capture-spsc.h"
#include "backend_fixture.h"

#define WAIT_S 5
#define NSAMP OPENRELAY_CAPTURE_FRAME_SAMPLES

enum scen { SC_NONE = 0, SC_READ, SC_DRAIN, SC_CONTROL };

static pthread_mutex_t hold_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t hold_cv = PTHREAD_COND_INITIALIZER;
static int hold_armed_read;
static int hold_armed_drain;
static int hold_entered;
static int hold_released;
static int hold_release_defective;

int32_t __real_openrelay_spsc_read(openrelay_spsc *ring, int16_t *dst, uint32_t tope,
                             uint32_t *read);
int32_t __real_openrelay_spsc_drain(openrelay_spsc *ring, uint32_t tope, uint32_t *dropped);

/* Retiene al consumidor en el punto de llamada, ya dentro de la API publica. */
static void hold_point(void) {
  pthread_mutex_lock(&hold_mu);
  hold_entered = 1;
  pthread_cond_broadcast(&hold_cv);
  while (!hold_released) {
    pthread_cond_wait(&hold_cv, &hold_mu);
  }
  if (hold_release_defective) {
    /* Motor ya liberado: no volver a el. */
    pthread_mutex_unlock(&hold_mu);
    pthread_exit(NULL);
  }
  pthread_mutex_unlock(&hold_mu);
}

int32_t __wrap_openrelay_spsc_read(openrelay_spsc *ring, int16_t *dst, uint32_t tope,
                             uint32_t *read) {
  int32_t rc = __real_openrelay_spsc_read(ring, dst, tope, read);
  if (hold_armed_read) {
    hold_armed_read = 0;
    hold_point();
  }
  return rc;
}

int32_t __wrap_openrelay_spsc_drain(openrelay_spsc *ring, uint32_t tope,
                              uint32_t *dropped) {
  int32_t rc = __real_openrelay_spsc_drain(ring, tope, dropped);
  if (hold_armed_drain) {
    hold_armed_drain = 0;
    hold_point();
  }
  return rc;
}

struct consumer {
  openrelay_capture *cap;
  enum scen kind;
  int32_t rc;
  uint32_t n;
  int16_t dst[NSAMP];
};

static void *consumer_main(void *arg) {
  struct consumer *c = (struct consumer *)arg;
  c->n = NSAMP;
  if (c->kind == SC_DRAIN) {
    c->rc = openrelay_capture_drain(c->cap, &c->n);
  } else {
    c->rc = openrelay_capture_read(c->cap, c->dst, &c->n);
  }
  return NULL;
}

static const openrelay_capture_format k_pcm = {
    OPENRELAY_CAPTURE_SAMPLE_RATE_HZ, OPENRELAY_CAPTURE_CHANNELS,
    OPENRELAY_CAPTURE_BYTES_PER_SAMPLE, OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u};

/* Setup valido: open/start/push/stop/close conservando el anillo. */
static int setup_ring(openrelay_capture **cap) {
  openrelay_capture_status st;
  int16_t pat[NSAMP];
  uint32_t i;
  int32_t rc;

  for (i = 0; i < NSAMP; i++) pat[i] = (int16_t)(i + 1u);
  openrelay_fx_reset();
  openrelay_fx_set_prepare_format(k_pcm.sample_rate_hz, k_pcm.channels,
                            k_pcm.bytes_per_sample, k_pcm.frame_samples, 0u);
  openrelay_fx_set_effective_format(k_pcm.sample_rate_hz, k_pcm.channels,
                              k_pcm.bytes_per_sample, k_pcm.frame_samples, 0u);
  rc = openrelay_capture_open(cap, OPENRELAY_CAPTURE_RING_SAMPLES);
  if (rc != OPENRELAY_CAPTURE_OK || *cap == NULL) {
    printf("SETUP_FAIL open rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_start(*cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL start rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_get_status(*cap, &st);
  if (rc != OPENRELAY_CAPTURE_OK || st.state != OPENRELAY_CAPTURE_STATE_STARTED) {
    printf("SETUP_FAIL status rc=%d state=%u\n", rc, (unsigned)st.state);
    return 2;
  }
  rc = openrelay_capture_push(*cap, st.generation, pat, NSAMP);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL push rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_stop(*cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL stop rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_close(*cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL close rc=%d\n", rc);
    return 2;
  }
  return 0;
}

static int pattern_ok(const int16_t *d) {
  uint32_t i;
  for (i = 0; i < NSAMP; i++) {
    if (d[i] != (int16_t)(i + 1u)) return 0;
  }
  return 1;
}

static void release_consumer(int defective) {
  pthread_mutex_lock(&hold_mu);
  hold_release_defective = defective;
  hold_released = 1;
  pthread_cond_broadcast(&hold_cv);
  pthread_mutex_unlock(&hold_mu);
}

int main(int argc, char **argv) {
  enum scen kind = SC_NONE;
  openrelay_capture *cap = NULL;
  openrelay_capture *before;
  struct consumer c;
  pthread_t th;
  struct timespec ts;
  int32_t rc_d;
  int rc_wait = 0;
  int fails = 0;

  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 2) {
    printf("SETUP_FAIL usage: inflight <read|drain|control>\n");
    return 2;
  }
  if (strcmp(argv[1], "read") == 0) kind = SC_READ;
  else if (strcmp(argv[1], "drain") == 0) kind = SC_DRAIN;
  else if (strcmp(argv[1], "control") == 0) kind = SC_CONTROL;
  else {
    printf("SETUP_FAIL unknown scenario\n");
    return 2;
  }
  printf("SCENARIO=%s\n", argv[1]);

  if (setup_ring(&cap) != 0) return 2;
  before = cap;
  memset(&c, 0, sizeof c);
  c.cap = cap;
  c.kind = kind;

  if (kind == SC_CONTROL) {
    if (pthread_create(&th, NULL, consumer_main, &c) != 0) {
      printf("SETUP_FAIL pthread_create\n");
      return 2;
    }
    pthread_join(th, NULL);
    printf("CONSUMER_API_CALLED=openrelay_capture_read rc=%d n=%u\n", c.rc,
           (unsigned)c.n);
    if (c.rc != OPENRELAY_CAPTURE_OK || c.n != NSAMP || !pattern_ok(c.dst)) {
      printf("FAIL control consumer read rc=%d n=%u\n", c.rc, (unsigned)c.n);
      return 1;
    }
    rc_d = openrelay_capture_destroy(&cap);
    printf("CONTROL_DESTROY rc=%d owner_after_null=%d\n", rc_d, cap == NULL);
    if (rc_d == OPENRELAY_CAPTURE_OK && cap == NULL) {
      printf("RESULT=PASS control destroy OK when quiescent\n");
      return 0;
    }
    printf("RESULT=FAIL control destroy not OK when quiescent\n");
    return 1;
  }

  /* Escenarios read/drain: consumidor retenido dentro de la API publica. */
  hold_armed_read = (kind == SC_READ);
  hold_armed_drain = (kind == SC_DRAIN);
  if (pthread_create(&th, NULL, consumer_main, &c) != 0) {
    printf("SETUP_FAIL pthread_create\n");
    return 2;
  }
  clock_gettime(CLOCK_REALTIME, &ts);
  ts.tv_sec += WAIT_S;
  pthread_mutex_lock(&hold_mu);
  while (!hold_entered && rc_wait == 0) {
    rc_wait = pthread_cond_timedwait(&hold_cv, &hold_mu, &ts);
  }
  pthread_mutex_unlock(&hold_mu);
  if (!hold_entered) {
    printf("SETUP_FAIL wrapper not reached within %ds (timeout/harness)\n",
           WAIT_S);
    return 2;
  }
  printf("WRAP_REACHED op=%s consumer_inside_public_api=1\n", argv[1]);

  printf("OWNER_BEFORE_DESTROY=%p\n", (void *)before);
  rc_d = openrelay_capture_destroy(&cap);
  /* No evaluar before después de una posible liberación. */
  printf("DESTROY_WHILE_HELD rc=%d owner_after=%p\n", rc_d, (void *)cap);

  if (rc_d == OPENRELAY_CAPTURE_ERR_BUSY && cap == before) {
    /* Comportamiento correcto: soltar, join, luego destroy OK. */
    release_consumer(0);
    pthread_join(th, NULL);
    printf("CONSUMER_API_CALLED=openrelay_capture_%s rc=%d n=%u\n",
           kind == SC_DRAIN ? "drain" : "read", c.rc, (unsigned)c.n);
    if (c.rc != OPENRELAY_CAPTURE_OK || c.n != NSAMP) {
      printf("FAIL consumer rc=%d n=%u\n", c.rc, (unsigned)c.n);
      fails++;
    }
    if (kind == SC_READ && !pattern_ok(c.dst)) {
      printf("FAIL consumer read payload mismatch\n");
      fails++;
    }
    rc_d = openrelay_capture_destroy(&cap);
    printf("POST_RELEASE_DESTROY rc=%d owner_after_null=%d\n", rc_d,
           cap == NULL);
    if (rc_d != OPENRELAY_CAPTURE_OK || cap != NULL) fails++;
    printf(fails ? "RESULT=FAIL post-release checks\n"
                 : "RESULT=PASS destroy BUSY while consumer in flight\n");
    return fails ? 1 : 0;
  }

  /* Destroy no dio BUSY+owner intacto. Captura hecha; ahora limpieza segura. */
  if (rc_d == OPENRELAY_CAPTURE_OK && cap == NULL) {
    printf("RESULT=RED destroy returned OK and owner NULL while %s consumer "
           "still inside openrelay_capture_%s (expected ERR_BUSY, owner unchanged)\n",
           argv[1], argv[1]);
    release_consumer(1); /* wrapper: pthread_exit sin volver al motor */
    pthread_join(th, NULL);
    printf("CLEANUP_DEFECTIVE consumer exited via wrapper, no engine return\n");
    return 1;
  }
  printf("RESULT=FAIL destroy rc=%d owner_changed=%d (expected ERR_BUSY)\n",
         rc_d, cap != before);
  release_consumer(0);
  pthread_join(th, NULL);
  return 1;
}
