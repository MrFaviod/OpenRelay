/* Fixture host de hardware (T13a S3c). Simula el adapter, no el motor.
 * No copia muestras, no reserva anillo y no inventa generacion: enable() solo
 * recuerda el token que el motor paso. Guarda el owner opaco sin mirarlo. */
#include "backend_fixture.h"

#include <stddef.h>

/* Objeto del backend: arl_be primero para que el motor solo vea ops. */
typedef struct arl_fx {
  arl_be be;
  arl_capture *owner;      /* referencia opaca, nunca dereferenciada */
  int has_owner;
  arl_capture_format configured; /* lo que el dispositivo negocia */
  arl_capture_format effective;  /* relectura en vigor              */
  uint64_t epoch;          /* ultimo token habilitado           */
  int has_epoch;           /* si hubo enable                    */
  uint32_t flags;
  uint64_t ack_epoch;
  int enabled;             /* callbacks habilitados ahora       */
  int closed;
  uint32_t calls[ARL_FX_OP_COUNT];
} arl_fx;

static arl_fx g_fx;
static int g_fx_live;

static arl_fx *fx_of(arl_be *be) { return (arl_fx *)be; }

/* Devuelve el formato CONFIGURADO del dispositivo, no el requested: si el
 * driver negocia otra cosa, el test debe poder verlo. */
static int32_t fx_prepare(arl_be *be, const arl_capture_format *requested,
                          arl_capture_format *effective) {
  arl_fx *fx = fx_of(be);
  fx->calls[ARL_FX_PREPARE]++;
  if (requested == NULL || effective == NULL) return ARL_CAPTURE_ERR_NULL;
  (void)requested; /* el requested lo conserva el motor, no el fixture */
  fx->closed = 0; /* prepare abre hardware, aun sin habilitar callbacks */
  *effective = fx->configured;
  return ARL_CAPTURE_OK;
}

static int32_t fx_effective(arl_be *be, arl_capture_format *out) {
  arl_fx *fx = fx_of(be);
  fx->calls[ARL_FX_EFFECTIVE]++;
  if (out == NULL) return ARL_CAPTURE_ERR_NO_OUTPUT;
  *out = fx->effective;
  return ARL_CAPTURE_OK;
}

static int32_t fx_enable(arl_be *be, uint64_t generation) {
  arl_fx *fx = fx_of(be);
  fx->calls[ARL_FX_ENABLE]++;
  /* Un solo epoch pendiente: habilitar otro pisaria obligaciones vivas. */
  if (fx->has_epoch && fx->flags != 0u && generation != fx->epoch)
    return ARL_CAPTURE_ERR_BUSY;
  fx->epoch = generation;
  fx->has_epoch = 1;
  fx->enabled = 1;
  fx->closed = 0;
  return ARL_CAPTURE_OK;
}

static int32_t fx_disable(arl_be *be) {
  arl_fx *fx = fx_of(be);
  fx->calls[ARL_FX_DISABLE]++;
  fx->enabled = 0;
  return ARL_CAPTURE_OK;
}

static int32_t fx_close(arl_be *be) {
  arl_fx *fx = fx_of(be);
  fx->calls[ARL_FX_CLOSE]++;
  fx->closed = 1; /* idempotente y NO toca flags */
  fx->enabled = 0;
  return ARL_CAPTURE_OK;
}

/* Fail-closed salvo instancia nunca habilitada, ya cerrada y sin obligaciones. */
static int32_t fx_retire(arl_be *be, uint64_t generation, uint32_t *flags) {
  arl_fx *fx = fx_of(be);
  fx->calls[ARL_FX_RETIRE]++;
  if (flags == NULL) return ARL_CAPTURE_ERR_NO_OUTPUT;
  if (!fx->has_epoch && fx->closed && fx->flags == 0u) {
    *flags = 0u;
    return ARL_CAPTURE_OK;
  }
  if (!fx->has_epoch || generation != fx->epoch) {
    *flags = ARL_BE_RETIRE_ALL; /* desconocido no es libre */
    return ARL_CAPTURE_ERR_ARGS;
  }
  *flags = fx->flags;
  return ARL_CAPTURE_OK;
}

static void fx_release(arl_be *be) {
  arl_fx *fx = fx_of(be);
  fx->calls[ARL_FX_RELEASE]++;
  fx->flags = 0u;
  fx->closed = 0;
  fx->enabled = 0;
  fx->has_epoch = 0;
  fx->has_owner = 0;
  fx->owner = NULL;
  g_fx_live = 0;
}

static const arl_be_ops g_fx_ops = {fx_prepare, fx_effective, fx_enable,
                                    fx_disable, fx_close,   fx_retire,
                                    fx_release};

/* Factory privada que enlaza el host. Un solo dispositivo simulado vivo.
 * owner nulo falla sin crear backend y deja *out null. */
int32_t arl_capture_backend_create(arl_be **out, arl_capture *owner) {
  if (out == NULL) return ARL_CAPTURE_ERR_NULL;
  *out = NULL;
  if (owner == NULL) return ARL_CAPTURE_ERR_NULL;
  if (g_fx_live) return ARL_CAPTURE_ERR_BUSY;
  g_fx.be.ops = &g_fx_ops;
  g_fx.owner = owner;
  g_fx.has_owner = 1;
  g_fx.closed = 1; /* factory no abre hardware */
  g_fx_live = 1;
  g_fx.calls[ARL_FX_CREATE]++;
  *out = &g_fx.be;
  return ARL_CAPTURE_OK;
}

void arl_fx_reset(void) {
  uint32_t i;
  g_fx_live = 0;
  g_fx.be.ops = &g_fx_ops;
  g_fx.owner = NULL;
  g_fx.has_owner = 0;
  g_fx.configured.sample_rate_hz = 0u;
  g_fx.configured.channels = 0u;
  g_fx.configured.bytes_per_sample = 0u;
  g_fx.configured.frame_samples = 0u;
  g_fx.configured.reserved0 = 0u;
  g_fx.effective = g_fx.configured;
  g_fx.epoch = 0u;
  g_fx.has_epoch = 0;
  g_fx.flags = 0u;
  g_fx.ack_epoch = 0u;
  g_fx.enabled = 0;
  g_fx.closed = 0;
  for (i = 0; i < (uint32_t)ARL_FX_OP_COUNT; i++) g_fx.calls[i] = 0u;
}

/* Formato que el dispositivo negocia. NO pisa el requested ni el effective:
 * para el caso mismatch se llama despues a arl_fx_set_effective_format. */
void arl_fx_set_prepare_format(uint32_t rate, uint16_t ch, uint16_t bps,
                               uint16_t frame, uint16_t reserved0) {
  g_fx.configured.sample_rate_hz = rate;
  g_fx.configured.channels = ch;
  g_fx.configured.bytes_per_sample = bps;
  g_fx.configured.frame_samples = frame;
  g_fx.configured.reserved0 = reserved0;
}

void arl_fx_set_effective_format(uint32_t rate, uint16_t ch, uint16_t bps,
                                 uint16_t frame, uint16_t reserved0) {
  g_fx.effective.sample_rate_hz = rate;
  g_fx.effective.channels = ch;
  g_fx.effective.bytes_per_sample = bps;
  g_fx.effective.frame_samples = frame;
  g_fx.effective.reserved0 = reserved0;
}

void arl_fx_set_retire(uint64_t epoch, uint32_t flags) {
  g_fx.epoch = epoch;
  g_fx.has_epoch = 1;
  g_fx.flags = flags & ARL_BE_RETIRE_ALL;
}

void arl_fx_ack_retire(uint64_t epoch) {
  if (!g_fx.has_epoch || epoch != g_fx.epoch) return;
  g_fx.flags &= ~(ARL_BE_RETIRE_DATA | ARL_BE_RETIRE_ERROR | ARL_BE_RETIRE_FW);
  g_fx.ack_epoch = epoch;
}

uint32_t arl_fx_calls(enum arl_fx_op op) {
  if (op < 0 || op >= ARL_FX_OP_COUNT) return 0u;
  return g_fx.calls[op];
}

int arl_fx_enabled(void) { return g_fx.enabled; }
uint64_t arl_fx_enable_token(void) { return g_fx.epoch; }
uint64_t arl_fx_ack_epoch(void) { return g_fx.ack_epoch; }
int arl_fx_has_owner(void) { return g_fx.has_owner; }
int arl_fx_closed(void) { return g_fx.closed; }
arl_be *arl_fx_backend(void) { return g_fx_live ? &g_fx.be : NULL; }
