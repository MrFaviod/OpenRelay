/* Pruebas independientes del owner estable/lifetime.
 * Usan solo capture-owner.h (API privada exacta) y el core real.
 * Un proceso por caso: owner_tests <1|2read|2drain|3|4|5|6|7>.
 * Sincronizacion determinista con condvars y timeout de setup; sin sleeps.
 * Exit 0 PASS, 1 FAIL/RED, 2 SETUP_FAIL.
 * Ningun hilo vuelve al motor tras una liberacion defectuosa: el wrapper de
 * read/drain hace pthread_exit, y el hilo de prestamo no toca cap si el owner
 * ya lo liberó (caso 1). */
#define _POSIX_C_SOURCE 200809L

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

#include "capture-abi.h"
#include "capture-owner.h"
#include "capture-spsc.h"
#include "backend_fixture.h"
#include "owner_setup.h"

#define WAIT_S 5
#define NTHR 8
#define NITER 2000

enum loan_mode { LOAN_COUNTERS = 1, LOAN_READ, LOAN_DRAIN };

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int f_held;   /* prestamo retenido en el punto de espera */
static int f_go;     /* el principal permite continuar */
static int f_done;   /* prestatario terminado */
static int f_defect; /* el principal vio liberacion defectuosa */
static int hold_armed_read;
static int hold_armed_drain;

int32_t __real_openrelay_spsc_read(openrelay_spsc *ring, int16_t *dst, uint32_t tope,
                             uint32_t *read);
int32_t __real_openrelay_spsc_drain(openrelay_spsc *ring, uint32_t tope, uint32_t *dropped);

static void set_flag(int *flag) {
  pthread_mutex_lock(&mu);
  *flag = 1;
  pthread_cond_broadcast(&cv);
  pthread_mutex_unlock(&mu);
}

/* 0 si la bandera se puso dentro del plazo; -1 timeout (SETUP). */
static int wait_flag(int *flag) {
  struct timespec ts;
  int rc = 0;
  int ok;

  clock_gettime(CLOCK_REALTIME, &ts);
  ts.tv_sec += WAIT_S;
  pthread_mutex_lock(&mu);
  while (!*flag && rc == 0) rc = pthread_cond_timedwait(&cv, &mu, &ts);
  ok = *flag;
  pthread_mutex_unlock(&mu);
  return ok ? 0 : -1;
}

static int read_defect(void) {
  int d;
  pthread_mutex_lock(&mu);
  d = f_defect;
  pthread_mutex_unlock(&mu);
  return d;
}

/* Retiene al consumidor DENTRO de la API publica, tras el real del SPSC. */
static void hold_point(void) {
  pthread_mutex_lock(&mu);
  f_held = 1;
  pthread_cond_broadcast(&cv);
  while (!f_go) pthread_cond_wait(&cv, &mu);
  if (f_defect) {
    pthread_mutex_unlock(&mu);
    pthread_exit(NULL); /* motor liberado: no volver a el */
  }
  pthread_mutex_unlock(&mu);
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

struct loan {
  openrelay_capture_owner *ow;
  openrelay_capture *acq;
  int32_t rc_acq;
  int32_t rc_op;
  uint32_t n;
  int16_t dst[OT_NSAMP];
  int mode;
};

static void *borrower(void *arg) {
  struct loan *l = (struct loan *)arg;
  openrelay_capture_counters ct;

  l->acq = NULL;
  l->rc_acq = openrelay_owner_acquire(l->ow, &l->acq);
  if (l->rc_acq != OPENRELAY_CAPTURE_OK) {
    set_flag(&f_held); /* despierta al principal: SETUP con acquire fallido */
    return NULL;
  }
  if (l->mode == LOAN_COUNTERS) {
    /* Prestamo retenido ANTES de la primera llamada raw. */
    set_flag(&f_held);
    (void)wait_flag(&f_go);
    if (read_defect()) {
      /* Liberado por defecto: no tocar cap. */
      openrelay_owner_release(l->ow);
      set_flag(&f_done);
      return NULL;
    }
    l->rc_op = openrelay_capture_get_counters(l->acq, &ct);
  } else if (l->mode == LOAN_READ) {
    l->n = OT_NSAMP;
    l->rc_op = openrelay_capture_read(l->acq, l->dst, &l->n); /* wrap retiene */
  } else {
    l->n = OT_NSAMP;
    l->rc_op = openrelay_capture_drain(l->acq, &l->n); /* wrap retiene */
  }
  openrelay_owner_release(l->ow);
  set_flag(&f_done);
  return NULL;
}

/* Casos 1, 2read, 2drain: prestamo retenido, destroy BUSY, luego OK. */
static int case_loan(enum loan_mode mode, const char *name) {
  openrelay_capture *cap = NULL;
  openrelay_capture *before;
  openrelay_capture_owner ow;
  struct loan l;
  pthread_t th;
  int32_t rc;
  int fails = 0;

  memset(&l, 0, sizeof l);
  l.ow = &ow;
  l.mode = mode;
  if (setup_closed(&cap) != 0) return 2;
  before = cap;
  if (openrelay_owner_init(&ow, cap) != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL owner_init\n");
    return 2;
  }
  hold_armed_read = (mode == LOAN_READ);
  hold_armed_drain = (mode == LOAN_DRAIN);
  if (pthread_create(&th, NULL, borrower, &l) != 0) {
    printf("SETUP_FAIL pthread_create\n");
    return 2;
  }
  if (wait_flag(&f_held) != 0) {
    printf("SETUP_FAIL loan not held within %ds\n", WAIT_S);
    return 2;
  }
  if (l.rc_acq != OPENRELAY_CAPTURE_OK || l.acq != before) {
    printf("SETUP_FAIL acquire rc=%d\n", l.rc_acq);
    return 2;
  }
  printf("LOAN_HELD case=%s\n", name);

  rc = openrelay_owner_try_destroy(&ow);
  printf("DESTROY_HELD rc=%d owner_same=%d\n", rc, ow.cap == before);
  if (!(rc == OPENRELAY_CAPTURE_ERR_BUSY && ow.cap == before)) {
    if (rc == OPENRELAY_CAPTURE_OK && ow.cap == NULL) {
      printf("RED destroy OK while loan held (%s)\n", name);
      pthread_mutex_lock(&mu);
      f_defect = 1;
      pthread_mutex_unlock(&mu);
      set_flag(&f_go);
      pthread_join(th, NULL);
      return 1;
    }
    printf("FAIL expected BUSY with owner intact\n");
    fails++;
  }

  set_flag(&f_go);
  if (wait_flag(&f_done) != 0) {
    printf("SETUP_FAIL borrower did not finish\n");
    return 2;
  }
  pthread_join(th, NULL);

  if (mode == LOAN_COUNTERS) {
    if (l.rc_op != OPENRELAY_CAPTURE_OK) {
      printf("FAIL counters rc=%d\n", l.rc_op);
      fails++;
    }
  } else {
    if (l.rc_op != OPENRELAY_CAPTURE_OK || l.n != OT_NSAMP) {
      printf("FAIL consumer rc=%d n=%u\n", l.rc_op, (unsigned)l.n);
      fails++;
    }
    if (mode == LOAN_READ && !pattern_ok(l.dst)) {
      printf("FAIL payload mismatch\n");
      fails++;
    }
  }

  rc = openrelay_owner_try_destroy(&ow);
  printf("RETRY_DESTROY rc=%d owner_null=%d\n", rc, ow.cap == NULL);
  if (rc != OPENRELAY_CAPTURE_OK || ow.cap != NULL) fails++;
  printf(fails ? "RESULT=FAIL case=%s\n" : "RESULT=PASS case=%s\n", name);
  return fails ? 1 : 0;
}

/* Caso 3: tras cierre del owner, acquire BUSY con *out NULL, sin tocar cap. */
static int case3(void) {
  openrelay_capture *cap = NULL;
  openrelay_capture_owner ow;
  static int sentinel;
  openrelay_capture *out = (openrelay_capture *)(void *)&sentinel;
  int32_t rc;
  int fails = 0;

  if (setup_closed(&cap) != 0) return 2;
  if (openrelay_owner_init(&ow, cap) != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL owner_init\n");
    return 2;
  }
  rc = openrelay_owner_try_destroy(&ow);
  if (rc != OPENRELAY_CAPTURE_OK || ow.cap != NULL) {
    printf("SETUP_FAIL close rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_owner_acquire(&ow, &out);
  printf("AFTER_CLOSE_ACQUIRE rc=%d out_null=%d owner_cap_null=%d\n", rc,
         out == NULL, ow.cap == NULL);
  if (rc != OPENRELAY_CAPTURE_ERR_BUSY || out != NULL || ow.cap != NULL) fails++;
  printf(fails ? "RESULT=FAIL case=3\n" : "RESULT=PASS case=3\n");
  return fails ? 1 : 0;
}

/* Caso 4: retiro con flags. Core owner intacto; admision reabierta; acquire
 * posible; tras ack del fixture, destroy OK. */
static int case4(void) {
  openrelay_capture *cap = NULL;
  openrelay_capture *before;
  openrelay_capture_owner ow;
  openrelay_capture *out = NULL;
  uint64_t epoch;
  int32_t rc;
  int fails = 0;

  if (setup_closed(&cap) != 0) return 2;
  before = cap;
  if (openrelay_owner_init(&ow, cap) != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL owner_init\n");
    return 2;
  }
  epoch = openrelay_fx_enable_token();
  openrelay_fx_set_retire(epoch, OPENRELAY_BE_RETIRE_DATA);
  rc = openrelay_owner_try_destroy(&ow);
  printf("RETIRE_DESTROY rc=%d owner_same=%d\n", rc, ow.cap == before);
  if (rc != OPENRELAY_CAPTURE_ERR_BUSY || ow.cap != before) fails++;

  rc = openrelay_owner_acquire(&ow, &out);
  printf("REOPEN_ACQUIRE rc=%d out_same=%d\n", rc, out == before);
  if (rc != OPENRELAY_CAPTURE_OK || out != before) {
    fails++;
  } else {
    openrelay_owner_release(&ow);
  }

  rc = openrelay_owner_try_destroy(&ow);
  printf("RETIRE_DESTROY_AGAIN rc=%d owner_same=%d\n", rc, ow.cap == before);
  if (rc != OPENRELAY_CAPTURE_ERR_BUSY || ow.cap != before) fails++;

  openrelay_fx_ack_retire(epoch);
  printf("FIXTURE_ACK ack_epoch_ok=%d\n", openrelay_fx_ack_epoch() == epoch);
  if (openrelay_fx_ack_epoch() != epoch) fails++;
  rc = openrelay_owner_try_destroy(&ow);
  printf("RETRY_DESTROY rc=%d owner_null=%d\n", rc, ow.cap == NULL);
  if (rc != OPENRELAY_CAPTURE_OK || ow.cap != NULL) fails++;
  printf(fails ? "RESULT=FAIL case=4\n" : "RESULT=PASS case=4\n");
  return fails ? 1 : 0;
}

/* Caso 5: control quiescente. Destroy OK al primer intento, un solo release. */
static int case5(void) {
  openrelay_capture *cap = NULL;
  openrelay_capture_owner ow;
  openrelay_capture *out = NULL;
  int32_t rc;
  int fails = 0;

  if (setup_closed(&cap) != 0) return 2;
  if (openrelay_owner_init(&ow, cap) != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL owner_init\n");
    return 2;
  }
  rc = openrelay_owner_try_destroy(&ow);
  printf("QUIESCENT_DESTROY rc=%d owner_null=%d\n", rc, ow.cap == NULL);
  if (rc != OPENRELAY_CAPTURE_OK || ow.cap != NULL) fails++;
  rc = openrelay_owner_acquire(&ow, &out);
  if (rc != OPENRELAY_CAPTURE_ERR_BUSY || out != NULL) fails++;
  printf("POST_CLOSE_ACQUIRE rc=%d out_null=%d\n", rc, out == NULL);
  printf(fails ? "RESULT=FAIL case=5\n" : "RESULT=PASS case=5\n");
  return fails ? 1 : 0;
}

/* Caso 6: saturacion del gate con estado test-only en owner estable.
 * No produce CLOSED ni cambia cap. Mas abajo, aritmetica etiquetada. */
static int case6(void) {
  openrelay_capture *cap = NULL;
  openrelay_capture_owner ow;
  openrelay_capture *out = NULL;
  uint32_t carried;
  int32_t rc;
  int fails = 0;

  if (setup_closed(&cap) != 0) return 2;
  openrelay_capture *before = cap;
  if (openrelay_owner_init(&ow, cap) != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL owner_init\n");
    return 2;
  }
  atomic_store(&ow.gate, OPENRELAY_OWNER_MAX_BORROWS); /* test-only */
  rc = openrelay_owner_acquire(&ow, &out);
  printf("SATURATED_ACQUIRE rc=%d out_null=%d gate_max=%d\n", rc, out == NULL,
         atomic_load(&ow.gate) == OPENRELAY_OWNER_MAX_BORROWS);
  if (rc != OPENRELAY_CAPTURE_ERR_BUSY || out != NULL ||
      atomic_load(&ow.gate) != OPENRELAY_OWNER_MAX_BORROWS)
    fails++;
  rc = openrelay_owner_try_destroy(&ow);
  printf("SATURATED_DESTROY rc=%d owner_same=%d\n", rc, ow.cap == before);
  if (rc != OPENRELAY_CAPTURE_ERR_BUSY || ow.cap != before) fails++;

  /* Aritmetica pura (no es comportamiento del owner): MAX+1 cae en el bit CLOSED. */
  carried = (uint32_t)(OPENRELAY_OWNER_MAX_BORROWS + 1u);
  printf("ARITH_ONLY MAX+1==CLOSED %d\n", carried == OPENRELAY_OWNER_CLOSED);
  if (carried != OPENRELAY_OWNER_CLOSED) fails++;

  atomic_store(&ow.gate, 0u); /* restaura estado estable */
  rc = openrelay_owner_acquire(&ow, &out);
  if (rc != OPENRELAY_CAPTURE_OK || out != before) fails++;
  else openrelay_owner_release(&ow);
  rc = openrelay_owner_try_destroy(&ow);
  printf("RETRY_DESTROY rc=%d owner_null=%d\n", rc, ow.cap == NULL);
  if (rc != OPENRELAY_CAPTURE_OK || ow.cap != NULL) fails++;
  printf(fails ? "RESULT=FAIL case=6\n" : "RESULT=PASS case=6\n");
  return fails ? 1 : 0;
}

struct contender {
  openrelay_capture_owner *ow;
  openrelay_capture *cap;
  int ok;
  int busy;
  int bad;
};

static pthread_barrier_t bar;

static void *contender(void *arg) {
  struct contender *w = (struct contender *)arg;
  unsigned i;

  (void)pthread_barrier_wait(&bar);
  for (i = 0; i < NITER; i++) {
    openrelay_capture *out = (openrelay_capture *)(void *)w; /* centinela no nulo */
    int32_t rc = openrelay_owner_acquire(w->ow, &out);
    if (rc == OPENRELAY_CAPTURE_OK) {
      if (out == w->cap) {
        w->ok++;
      } else {
        w->bad++;
      }
      openrelay_owner_release(w->ow);
    } else if (rc == OPENRELAY_CAPTURE_ERR_BUSY && out == NULL) {
      w->busy++; /* contencion CAS: BUSY permitido */
    } else {
      w->bad++;
    }
  }
  return NULL;
}

/* Caso 7: acquire concurrente en owner estable. Sin liberacion propia del
 * owner; el almacenamiento vive hasta despues de los joins. */
static int case7(void) {
  openrelay_capture *cap = NULL;
  openrelay_capture_owner ow;
  struct contender w[NTHR];
  pthread_t th[NTHR];
  openrelay_capture *before;
  int ok = 0, busy = 0, bad = 0, i;
  int32_t rc;
  int fails = 0;

  if (setup_closed(&cap) != 0) return 2;
  before = cap;
  if (openrelay_owner_init(&ow, cap) != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL owner_init\n");
    return 2;
  }
  if (pthread_barrier_init(&bar, NULL, NTHR) != 0) {
    printf("SETUP_FAIL barrier\n");
    return 2;
  }
  for (i = 0; i < NTHR; i++) {
    memset(&w[i], 0, sizeof w[i]);
    w[i].ow = &ow;
    w[i].cap = before;
    if (pthread_create(&th[i], NULL, contender, &w[i]) != 0) {
      printf("SETUP_FAIL pthread_create %d\n", i);
      return 2;
    }
  }
  for (i = 0; i < NTHR; i++) {
    pthread_join(th[i], NULL);
    ok += w[i].ok;
    busy += w[i].busy;
    bad += w[i].bad;
  }
  pthread_barrier_destroy(&bar);
  printf("CONTENTION threads=%d iters=%d ok=%d busy=%d bad=%d\n", NTHR, NITER,
         ok, busy, bad);
  if (bad != 0 || ok + busy != NTHR * NITER || ok == 0) fails++;
  if (atomic_load(&ow.gate) != 0u || ow.cap != before) fails++;
  rc = openrelay_owner_try_destroy(&ow);
  printf("POST_CONTENTION_DESTROY rc=%d owner_null=%d\n", rc, ow.cap == NULL);
  if (rc != OPENRELAY_CAPTURE_OK || ow.cap != NULL) fails++;
  printf(fails ? "RESULT=FAIL case=7\n" : "RESULT=PASS case=7\n");
  return fails ? 1 : 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 2) {
    printf("SETUP_FAIL usage: owner_tests <1|2read|2drain|3|4|5|6|7>\n");
    return 2;
  }
  printf("CASE=%s\n", argv[1]);
  if (strcmp(argv[1], "1") == 0) return case_loan(LOAN_COUNTERS, "1");
  if (strcmp(argv[1], "2read") == 0) return case_loan(LOAN_READ, "2read");
  if (strcmp(argv[1], "2drain") == 0) return case_loan(LOAN_DRAIN, "2drain");
  if (strcmp(argv[1], "3") == 0) return case3();
  if (strcmp(argv[1], "4") == 0) return case4();
  if (strcmp(argv[1], "5") == 0) return case5();
  if (strcmp(argv[1], "6") == 0) return case6();
  if (strcmp(argv[1], "7") == 0) return case7();
  printf("SETUP_FAIL unknown case\n");
  return 2;
}
