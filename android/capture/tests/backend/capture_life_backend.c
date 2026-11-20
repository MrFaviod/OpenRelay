/* Pruebas C11 del lifecycle compartido con backend simulado.
 *
 * Mismo motor futuro: cada caso llama simbolos publicos openrelay_capture_*; el
 * fixture solo aporta hardware/formatos/contadores y el owner opaco. Este
 * archivo NO implementa una segunda maquina de estados.
 *
 * Casos lifecycle preparados: 6. Sondas de fixture (NO lifecycle): 1. Al
 * EJECUTAR se imprime el conteo real; hoy no hay motor, asi que el runner solo
 * compila y contrasta el conjunto exacto de simbolos ausentes (imprime 0
 * porque no enlaza). Un fallo de enlace por openrelay_capture_* ausente es
 * EXPECTED_MISSING_IMPLEMENTATION, no RED funcional. Los gaps en vuelo
 * (read/drain/getters concurrentes) se declaran en result.md.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "capture-abi.h"
#include "backend_fixture.h"

static int checks;
static int failures;

#define CHECK(c)                                                       \
  do {                                                                 \
    checks++;                                                          \
    if (!(c)) { failures++;                                            \
      printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c); }             \
  } while (0)

#define CHECK_EQ(g, w)                                                 \
  do {                                                                 \
    long long g_ = (long long)(g), w_ = (long long)(w);                 \
    checks++;                                                          \
    if (g_ != w_) { failures++;                                        \
      printf("FAIL %s:%d %s=%lld want %lld\n", __FILE__, __LINE__,     \
             #g, g_, w_); }                                            \
  } while (0)

#define BUF_SAMPLES (OPENRELAY_CAPTURE_FRAME_SAMPLES + 8u)
#define CANARY ((int16_t)0x5A5A)

static const openrelay_capture_format PCM = {OPENRELAY_CAPTURE_SAMPLE_RATE_HZ,
                                       OPENRELAY_CAPTURE_CHANNELS,
                                       OPENRELAY_CAPTURE_BYTES_PER_SAMPLE,
                                       OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u};

static void fill_canary(int16_t *b, size_t n) {
  size_t i;
  for (i = 0; i < n; i++) b[i] = CANARY;
}

static int canary_ok(const int16_t *b, size_t n) {
  size_t i;
  for (i = 0; i < n; i++) if (b[i] != CANARY) return 0;
  return 1;
}

/* Campos normativos, nunca padding. */
static int same_format(const openrelay_capture_format *a, const openrelay_capture_format *b) {
  return a->sample_rate_hz == b->sample_rate_hz &&
         a->channels == b->channels && a->bytes_per_sample == b->bytes_per_sample &&
         a->frame_samples == b->frame_samples && a->reserved0 == b->reserved0;
}

static int same_status(const openrelay_capture_status *a, const openrelay_capture_status *b) {
  return a->generation == b->generation && a->state == b->state &&
         a->last_error == b->last_error &&
         same_format(&a->requested, &b->requested) &&
         same_format(&a->effective, &b->effective);
}

/* Fixture listo: device neutro, prepare y effective en PCM valido. */
static void fx_pcm(void) {
  openrelay_fx_reset();
  openrelay_fx_set_prepare_format(PCM.sample_rate_hz, PCM.channels,
                            PCM.bytes_per_sample, PCM.frame_samples, 0u);
  openrelay_fx_set_effective_format(PCM.sample_rate_hz, PCM.channels,
                              PCM.bytes_per_sample, PCM.frame_samples, 0u);
}

static openrelay_capture *open_ring(void) {
  openrelay_capture *cap = NULL;
  CHECK_EQ(openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES), OPENRELAY_CAPTURE_OK);
  CHECK(cap != NULL);
  return cap;
}

/* 1. Camino completo y payload FIFO. stop no drena: lo que entro antes sigue
 * legible en STOPPED. No se fija generation inicial ni padding. */
static void test_open_start_push_read_stop_close(void) {
  openrelay_capture *cap;
  openrelay_capture_status st;
  int16_t in[8] = {10, 11, 12, 13, 20, 21, 22, 23};
  int16_t out[BUF_SAMPLES];
  uint32_t n;
  fx_pcm();
  cap = open_ring();
  if (!cap) return;
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_PREPARE), 1u);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_EFFECTIVE), 1u);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_ENABLE), 1u);
  CHECK(openrelay_fx_enabled() == 1);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_STARTED);
  CHECK_EQ(st.last_error, OPENRELAY_CAPTURE_OK);
  CHECK(same_format(&st.requested, &PCM));
  CHECK(same_format(&st.effective, &PCM));
  CHECK(st.generation == openrelay_fx_enable_token()); /* misma generation */
  CHECK_EQ(openrelay_capture_push(cap, st.generation, in, 4u), OPENRELAY_CAPTURE_OK);
  fill_canary(out, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, out, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 4u);
  CHECK(out[0] == 10 && out[1] == 11 && out[2] == 12 && out[3] == 13);
  CHECK(canary_ok(&out[4], BUF_SAMPLES - 4u));
  CHECK_EQ(openrelay_capture_push(cap, st.generation, &in[4], 4u), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_stop(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_DISABLE), 1u);
  CHECK(openrelay_fx_enabled() == 0);
  fill_canary(out, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, out, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 4u); /* stop no drena */
  CHECK(out[0] == 20 && out[1] == 21 && out[2] == 22 && out[3] == 23);
  CHECK(canary_ok(&out[4], BUF_SAMPLES - 4u));
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_CLOSE), 1u); /* cierra stream, no libera */
  CHECK(openrelay_fx_closed() == 1);
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK); /* idempotente */
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_RELEASE), 0u);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_CLOSED);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_RELEASE), 1u);
}

/* 2. Idempotencia sin mutacion; STOPPED->start generation nueva; un token
 * anterior no copia. Se drena por el consumidor antes del oraculo de token
 * viejo: no se impone un flush de control que la norma no fija. */
static void test_idempotence_and_generation(void) {
  openrelay_capture *cap;
  openrelay_capture_status before, after;
  int16_t in[4] = {1, 2, 3, 4};
  int16_t out[BUF_SAMPLES];
  uint32_t n, dropped;
  uint64_t gen1;
  fx_pcm();
  cap = open_ring();
  if (!cap) return;
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_OK);
  memset(&before, 0xFF, sizeof before);
  CHECK_EQ(openrelay_capture_get_status(cap, &before), OPENRELAY_CAPTURE_OK);
  gen1 = before.generation;
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_OK); /* idempotente */
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_ENABLE), 1u);        /* no re-habilita */
  memset(&after, 0xFF, sizeof after);
  CHECK_EQ(openrelay_capture_get_status(cap, &after), OPENRELAY_CAPTURE_OK);
  CHECK(same_status(&after, &before));
  CHECK_EQ(openrelay_capture_push(cap, gen1, in, 4u), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_stop(cap), OPENRELAY_CAPTURE_OK);
  memset(&before, 0xFF, sizeof before);
  CHECK_EQ(openrelay_capture_get_status(cap, &before), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(before.state, OPENRELAY_CAPTURE_STATE_STOPPED);
  CHECK_EQ(openrelay_capture_stop(cap), OPENRELAY_CAPTURE_OK); /* idempotente */
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_DISABLE), 1u);
  memset(&after, 0xFF, sizeof after);
  CHECK_EQ(openrelay_capture_get_status(cap, &after), OPENRELAY_CAPTURE_OK);
  CHECK(same_status(&after, &before));
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_OK);
  memset(&after, 0xFF, sizeof after);
  CHECK_EQ(openrelay_capture_get_status(cap, &after), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(after.state, OPENRELAY_CAPTURE_STATE_STARTED);
  CHECK(after.generation != gen1); /* generation nueva */
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_ENABLE), 2u);
  /* Tope de entrada = lo pendiente (4), no 0: drain descarta min(tope, disp). */
  dropped = 4u;
  CHECK_EQ(openrelay_capture_drain(cap, &dropped), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(dropped, 4u);
  CHECK_EQ(openrelay_capture_push(cap, gen1, in, 4u), OPENRELAY_CAPTURE_ERR_CLOSED);
  fill_canary(out, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, out, &n), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(n, 0u); /* token viejo no copio ni contamino */
  CHECK(canary_ok(out, BUF_SAMPLES));
  CHECK_EQ(openrelay_capture_stop(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* 3. effective del backend distinto de requested -> ERR_FORMAT y ERROR, sin
 * PCM y sin habilitar callback. requested valido permanece. */
static void test_format_mismatch(void) {
  openrelay_capture *cap;
  openrelay_capture_status st;
  int16_t out[BUF_SAMPLES];
  uint32_t n;
  fx_pcm();
  openrelay_fx_set_effective_format(44100u, OPENRELAY_CAPTURE_CHANNELS,
                              OPENRELAY_CAPTURE_BYTES_PER_SAMPLE,
                              OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u);
  cap = open_ring();
  if (!cap) return;
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_ERR_FORMAT);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_ENABLE), 0u); /* no habilita */
  CHECK(openrelay_fx_enabled() == 0);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(st.state, OPENRELAY_CAPTURE_STATE_ERROR);
  CHECK_EQ(st.last_error, OPENRELAY_CAPTURE_ERR_FORMAT);
  CHECK(same_format(&st.requested, &PCM)); /* solicitud intacta */
  fill_canary(out, BUF_SAMPLES);
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK_EQ(openrelay_capture_read(cap, out, &n), OPENRELAY_CAPTURE_ERR_CLOSED);
  CHECK_EQ(n, 0u);
  CHECK(canary_ok(out, BUF_SAMPLES));
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK); /* ERROR exige close */
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* 4. Evento de error/desconexion. Todo contador que un evento no debe mover: */
static const enum openrelay_fx_op SNAP_OPS[] = {
    OPENRELAY_FX_PREPARE, OPENRELAY_FX_EFFECTIVE, OPENRELAY_FX_ENABLE, OPENRELAY_FX_DISABLE,
    OPENRELAY_FX_CLOSE,   OPENRELAY_FX_RETIRE, OPENRELAY_FX_RELEASE};
#define SNAP_N (sizeof SNAP_OPS / sizeof SNAP_OPS[0])

static void snap_take(uint32_t *s) {
  size_t i;
  for (i = 0; i < SNAP_N; i++) s[i] = openrelay_fx_calls(SNAP_OPS[i]);
}

static void snap_expect(const uint32_t *s) {
  size_t i;
  for (i = 0; i < SNAP_N; i++) CHECK_EQ(openrelay_fx_calls(SNAP_OPS[i]), s[i]);
}

static void event_case(int disconnect);
static void event_case_error(void) { event_case(0); }
static void event_case_disconnect(void) { event_case(1); }

/* Token ajeno -> ERR_CLOSED sin mutacion; token actual se compone en status
 * por control, avanza generation e impide PCM. Contadores ANTES del evento y
 * comprobados justo despues, antes de get_status: note_* publica, nunca cierra
 * ni retira. last_error de DISCONNECTED no esta normado: no se asserta. */
static void event_case(int disconnect) {
  openrelay_capture *cap;
  openrelay_capture_status before, after;
  int16_t in[2] = {7, 8};
  uint32_t snap[SNAP_N];
  uint64_t stale;
  int32_t rc;
  fx_pcm();
  cap = open_ring();
  if (!cap) return;
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_OK);
  memset(&before, 0xFF, sizeof before);
  CHECK_EQ(openrelay_capture_get_status(cap, &before), OPENRELAY_CAPTURE_OK);
  stale = before.generation ^ UINT64_C(1); /* distinto incluso al wrap; inicial no normado */
  snap_take(snap);
  rc = disconnect ? openrelay_capture_note_disconnect(cap, stale)
                  : openrelay_capture_note_error(cap, stale, OPENRELAY_CAPTURE_ERR_STATE);
  CHECK_EQ(rc, OPENRELAY_CAPTURE_ERR_CLOSED);
  snap_expect(snap); /* token ajeno: no muto nada */
  memset(&after, 0xFF, sizeof after);
  CHECK_EQ(openrelay_capture_get_status(cap, &after), OPENRELAY_CAPTURE_OK);
  CHECK(same_status(&after, &before));
  snap_take(snap);
  rc = disconnect
           ? openrelay_capture_note_disconnect(cap, before.generation)
           : openrelay_capture_note_error(cap, before.generation,
                                    OPENRELAY_CAPTURE_ERR_STATE);
  CHECK_EQ(rc, OPENRELAY_CAPTURE_OK);
  snap_expect(snap); /* no cerro, no paro, no retiro, no libero */
  CHECK_EQ(openrelay_capture_push(cap, before.generation, in, 2u),
           OPENRELAY_CAPTURE_ERR_CLOSED);
  memset(&after, 0xFF, sizeof after);
  CHECK_EQ(openrelay_capture_get_status(cap, &after), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(after.state, disconnect ? OPENRELAY_CAPTURE_STATE_DISCONNECTED
                                   : OPENRELAY_CAPTURE_STATE_ERROR);
  if (!disconnect) CHECK_EQ(after.last_error, OPENRELAY_CAPTURE_ERR_STATE);
  CHECK(after.generation != before.generation); /* avanza */
  CHECK_EQ(openrelay_capture_push(cap, after.generation, in, 2u),
           OPENRELAY_CAPTURE_ERR_CLOSED); /* ni ERROR ni DISCONNECTED admiten PCM */
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_ERR_STATE); /* exige close */
  CHECK(cap != NULL);
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
}

/* 5. Retiro incompleto: destroy ERR_BUSY con owner igual hasta ACK de ese
 * objeto/epoch y sin obligaciones. MayorReenter mantiene ERR_BUSY. Ningun
 * alias se inspecciona despues de liberar. */
static void test_destroy_requires_retire(void) {
  openrelay_capture *cap;
  openrelay_capture *alias;
  openrelay_capture_status st;
  uint64_t gen;
  uint32_t retire_before;
  fx_pcm();
  cap = open_ring();
  if (!cap) return;
  CHECK_EQ(openrelay_capture_start(cap), OPENRELAY_CAPTURE_OK);
  memset(&st, 0xFF, sizeof st);
  CHECK_EQ(openrelay_capture_get_status(cap, &st), OPENRELAY_CAPTURE_OK);
  gen = st.generation;
  CHECK_EQ(openrelay_capture_stop(cap), OPENRELAY_CAPTURE_OK);
  openrelay_fx_set_retire(gen, OPENRELAY_BE_RETIRE_ALL);
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  CHECK(openrelay_fx_closed() == 1);
  alias = cap;
  retire_before = openrelay_fx_calls(OPENRELAY_FX_RETIRE);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_ERR_BUSY);
  CHECK(cap == alias); /* owner no cambia */
  openrelay_fx_ack_retire(gen); /* framework retiro datos/error/fw de ese epoch */
  CHECK_EQ(openrelay_fx_ack_epoch(), gen);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_ERR_BUSY); /* MAY_REENTER */
  CHECK(cap == alias);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_RETIRE), retire_before + 2u); /* dos intentos, sin imponer llamadas previas */
  openrelay_fx_set_retire(gen, 0u); /* sin obligaciones vivas */
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_RELEASE), 1u);
  /* alias no se inspecciona: no hay tabla de handles. */
}

/* Contrato backend sobre owner real: toma el backend que open ya creo.
 * No crea un segundo backend ni libera a espaldas del motor.
 * Se contabiliza aparte de los seis casos lifecycle. */
static void test_factory_owner_and_unknown_token(void) {
  openrelay_be *be = (openrelay_be *)0x1;
  openrelay_capture *cap;
  uint32_t flags = 0xDEADu;
  uint32_t pre;
  fx_pcm();
  openrelay_fx_set_prepare_format(44100u, OPENRELAY_CAPTURE_CHANNELS,
                            OPENRELAY_CAPTURE_BYTES_PER_SAMPLE,
                            OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u);
  CHECK_EQ(openrelay_capture_backend_create(&be, NULL), OPENRELAY_CAPTURE_ERR_NULL);
  CHECK(be == NULL);              /* owner nulo no crea backend */
  CHECK(openrelay_fx_has_owner() == 0);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_CREATE), 0u);
  cap = open_ring();
  if (!cap) return;
  be = openrelay_fx_backend(); /* backend ya creado y poseido por cap */
  CHECK(be != NULL);
  CHECK(openrelay_fx_has_owner() == 1);
  { /* prepare devuelve el formato CONFIGURADO, no el requested */
    openrelay_capture_format got = {0u, 0u, 0u, 0u, 0u};
    CHECK_EQ(be->ops->prepare(be, &PCM, &got), OPENRELAY_CAPTURE_OK);
    CHECK_EQ(got.sample_rate_hz, 44100u);
  }
  /* Sin enable previo no hay token valido: error y ALL, nunca limpio. */
  pre = openrelay_fx_calls(OPENRELAY_FX_RETIRE);
  flags = 0xDEADu;
  CHECK(be->ops->retire(be, 7u, &flags) != OPENRELAY_CAPTURE_OK);
  CHECK_EQ(flags, OPENRELAY_BE_RETIRE_ALL);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_RETIRE), pre + 1u);
  /* close/destroy del motor conservan ownership hasta el retiro. */
  CHECK_EQ(openrelay_capture_close(cap), OPENRELAY_CAPTURE_OK);
  CHECK_EQ(openrelay_capture_destroy(&cap), OPENRELAY_CAPTURE_OK);
  CHECK(cap == NULL);
  CHECK_EQ(openrelay_fx_calls(OPENRELAY_FX_RELEASE), 1u);
}

int main(void) {
  int lifecycle_cases = 0;
  void (*const cases[])(void) = {
      test_open_start_push_read_stop_close, test_idempotence_and_generation,
      test_format_mismatch, event_case_error, event_case_disconnect,
      test_destroy_requires_retire};
  size_t i;
  for (i = 0; i < sizeof cases / sizeof cases[0]; i++) {
    cases[i]();
    lifecycle_cases++;
  }
  /* Conteo real al EJECUTAR. El runner de preparacion imprime 0 porque no
   * puede enlazar este binario. */
  printf("LIFECYCLE_CASES_EXECUTED=%d\n", lifecycle_cases);
  test_factory_owner_and_unknown_token();
  printf("BACKEND_CONTRACT_CASES_EXECUTED=1\n");
  printf("%d checks, %d failures\n", checks, failures);
  return failures == 0 ? 0 : 1;
}
