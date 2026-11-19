/* test_enable_events.c - note_* con token vigente DENTRO de enable y control
 * positivo tras STARTED. Uso: test_enable_events <caso>.
 * Exit 0 PASS, 1 RED (aserto de comportamiento), 2 SETUP_FAIL (incluye no
 * disparo del hook enable_done: fail-closed).
 *
 * Hook: solo el punto enable_done del fixture (tras fx->enabled=1, antes de
 * volver de enable). Dentro del hook: push serial del productor (STARTING),
 * notas con token viejo/desconocido y una nota con el token vigente desde un
 * hilo de error con join antes de volver. No se llama a get_status dentro de
 * enable: el control solo lee el estado tras start. */
#define _POSIX_C_SOURCE 200809L
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "capture-abi.h"
#include "capture-backend.h"
#include "capture-spsc.h"
#include "backend_fixture.h"

#define NS OPENRELAY_CAPTURE_FRAME_SAMPLES
#define KIND_ERROR 1
#define KIND_DISCONNECT 2

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

/* ---- hook del fixture (mismo hilo que start, dentro de enable) ---- */
static void (*g_enable_hook)(void);
static int g_hook_fired;

void openrelay_test_hook(const char *id) {
  void (*fn)(void);
  if (strcmp(id, "enable_done") != 0 || g_enable_hook == NULL) {
    return;
  }
  fn = g_enable_hook;
  g_enable_hook = NULL; /* una sola vez */
  fn();
}

/* ---- estado de caso (estatico: vive hasta los joins) ---- */
static openrelay_capture *cap;
static int16_t buf[NS];
static int g_kind;

/* Resultados capturados dentro de enable; se leen tras start. */
static uint64_t h_tok;
static int32_t h_push_rc;
static int32_t h_stale0_rc;
static int32_t h_stale_unk_rc;
static int32_t h_note_rc;

struct note_job {
  int kind;
  uint64_t gen;
  int32_t code;
  int32_t rc;
};
static struct note_job g_job;

static void *job_note(void *p) {
  struct note_job *j = p;
  if (j->kind == KIND_ERROR) {
    j->rc = openrelay_capture_note_error(cap, j->gen, j->code);
  } else {
    j->rc = openrelay_capture_note_disconnect(cap, j->gen);
  }
  return NULL;
}

static void enable_note_hook(void) {
  pthread_t t;

  h_tok = openrelay_fx_enable_token();
  g_hook_fired = 1;
  /* Productor serial dentro de enable: el gate no debe estar abierto. */
  h_push_rc = openrelay_capture_push(cap, h_tok, buf, NS);
  /* Notas con token viejo (0) y desconocido (tok+5): sin mutacion. */
  h_stale0_rc = openrelay_capture_note_error(cap, 0u, -42);
  h_stale_unk_rc = openrelay_capture_note_disconnect(cap, h_tok + 5u);
  /* Nota con el token vigente desde el hilo de error, con join antes de volver. */
  g_job.kind = g_kind;
  g_job.gen = h_tok;
  g_job.code = -42;
  g_job.rc = -99;
  if (pthread_create(&t, NULL, job_note, &g_job) != 0) {
    h_note_rc = -98;
    return;
  }
  pthread_join(t, NULL);
  h_note_rc = g_job.rc;
}

static int setup(void) {
  openrelay_fx_reset();
  cap = NULL;
  g_enable_hook = NULL;
  g_hook_fired = 0;
  openrelay_fx_set_prepare_format(48000u, 1u, 2u, 240u, 0u);
  openrelay_fx_set_effective_format(48000u, 1u, 2u, 240u, 0u);
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

/* Cierre de limpieza sin aserto de setup: el caso ya puede estar en rojo. */
static void cleanup(openrelay_capture_status *st) {
  openrelay_capture *own;
  if (cap == NULL) {
    return;
  }
  if (st->state == OPENRELAY_CAPTURE_STATE_STARTED) {
    (void)openrelay_capture_stop(cap);
  }
  (void)openrelay_capture_close(cap);
  own = cap;
  CHECK(openrelay_capture_destroy(&own) == OPENRELAY_CAPTURE_OK && own == NULL);
  if (own == NULL) {
    cap = NULL;
  }
}

/* Casos 1 y 2: nota con token vigente dentro de enable (error / disconnect). */
static int enable_case(int kind, const char *name, uint32_t want_state,
                       int32_t want_last) {
  openrelay_capture_status st;
  openrelay_capture_counters ctr;
  int32_t rc;
  uint32_t n;
  int16_t dst[NS];

  SETUP(setup());
  g_kind = kind;
  g_enable_hook = enable_note_hook;
  rc = openrelay_capture_start(cap);
  SETUP(g_hook_fired == 1);                 /* hook enable_done disparado */
  SETUP(openrelay_fx_calls(OPENRELAY_FX_ENABLE) == 1u); /* enable real una vez */
  printf("ORACLE %s in_enable tok=%llu push=%d stale_err0=%d stale_disc_unk=%d note=%d\n",
         name, (unsigned long long)h_tok, h_push_rc, h_stale0_rc,
         h_stale_unk_rc, h_note_rc);
  CHECK(h_push_rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(h_stale0_rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(h_stale_unk_rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(h_note_rc == OPENRELAY_CAPTURE_OK);
  printf("ORACLE %s start_rc=%d\n", name, rc);
  CHECK(rc == OPENRELAY_CAPTURE_OK);

  /* Control: aplica el evento publicado durante enable. */
  SETUP(openrelay_capture_get_status(cap, &st) == OPENRELAY_CAPTURE_OK);
  printf("ORACLE %s status state=%u gen=%llu last_error=%d\n", name, st.state,
         (unsigned long long)st.generation, st.last_error);
  CHECK(st.state == want_state);
  CHECK(st.generation == 2u);
  CHECK(st.last_error == want_last);

  /* Gate no reabierto por start: push con el token vigente es ERR_CLOSED. */
  CHECK(openrelay_capture_push(cap, h_tok, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  n = NS;
  rc = openrelay_capture_read(cap, dst, &n);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(n == 0u);
  SETUP(openrelay_capture_get_counters(cap, &ctr) == OPENRELAY_CAPTURE_OK);
  printf("ORACLE %s counters written=%llu callbacks=%llu\n", name,
         (unsigned long long)ctr.samples_written,
         (unsigned long long)ctr.callbacks);
  CHECK(ctr.samples_written == 0u);
  CHECK(ctr.callbacks == 0u);

  cleanup(&st);
  return report(name);
}

/* Caso 3: control positivo tras STARTED, sin hook. */
static int control_case(void) {
  openrelay_capture_status st;
  openrelay_capture_counters ctr;
  int32_t rc;
  uint32_t n;
  int16_t dst[NS];

  SETUP(setup());
  SETUP(openrelay_capture_start(cap) == OPENRELAY_CAPTURE_OK);
  CHECK(openrelay_capture_push(cap, 1u, buf, NS) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_get_counters(cap, &ctr) == OPENRELAY_CAPTURE_OK);
  CHECK(ctr.samples_written == NS);
  CHECK(ctr.callbacks == 1u);
  CHECK(openrelay_capture_note_error(cap, 1u, -7) == OPENRELAY_CAPTURE_OK);
  SETUP(openrelay_capture_get_status(cap, &st) == OPENRELAY_CAPTURE_OK);
  printf("ORACLE control status state=%u gen=%llu last_error=%d\n", st.state,
         (unsigned long long)st.generation, st.last_error);
  CHECK(st.state == OPENRELAY_CAPTURE_STATE_ERROR);
  CHECK(st.generation == 2u);
  CHECK(st.last_error == -7);
  CHECK(openrelay_capture_push(cap, 1u, buf, NS) == OPENRELAY_CAPTURE_ERR_CLOSED);
  n = NS;
  rc = openrelay_capture_read(cap, dst, &n);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK(n == 0u);
  cleanup(&st);
  return report("control_started_note_close_destroy");
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "uso: test_enable_events <caso>\n");
    return 2;
  }
  if (strcmp(argv[1], "enable_error") == 0) {
    return enable_case(KIND_ERROR, "enable_error", OPENRELAY_CAPTURE_STATE_ERROR, -42);
  }
  if (strcmp(argv[1], "enable_disconnect") == 0) {
    return enable_case(KIND_DISCONNECT, "enable_disconnect",
                       OPENRELAY_CAPTURE_STATE_DISCONNECTED, OPENRELAY_CAPTURE_ERR_CLOSED);
  }
  if (strcmp(argv[1], "control_positive") == 0) {
    return control_case();
  }
  fprintf(stderr, "caso desconocido: %s\n", argv[1]);
  return 2;
}
