/* Fixture host de hardware (T13a S3c). NO es Oboe ni Android y NO tiene
 * estados del motor ni generacion propia: solo formato negociado, contadores
 * de llamadas y posibilidades de entrada que el test declara. El token que
 * apunta es el que el motor paso como argumento, no uno inventado aqui.
 * Un solo dispositivo simulado: crear dos backends vivos da ERR_BUSY.
 * Guarda el owner opaco que la factory recibe; no lo dereferencia. */
#ifndef ARL_BACKEND_FIXTURE_H
#define ARL_BACKEND_FIXTURE_H

#include <stdint.h>

#include "capture-backend.h"

#if defined(__cplusplus)
extern "C" {
#endif

enum arl_fx_op {
  ARL_FX_CREATE = 0,
  ARL_FX_PREPARE,
  ARL_FX_EFFECTIVE,
  ARL_FX_ENABLE,
  ARL_FX_DISABLE,
  ARL_FX_CLOSE,
  ARL_FX_RETIRE,
  ARL_FX_RELEASE,
  ARL_FX_OP_COUNT
};

/* Estado neutro: dispositivo cerrado, sin flags, contadores a 0. */
void arl_fx_reset(void);
/* Formato CONFIGURADO que el dispositivo negocia en prepare(). No pisa
 * requested ni effective: se guardan aparte. */
void arl_fx_set_prepare_format(uint32_t rate, uint16_t ch, uint16_t bps,
                               uint16_t frame, uint16_t reserved0);
/* Formato que effective() relee. Una discrepancia la rechaza el motor. */
void arl_fx_set_effective_format(uint32_t rate, uint16_t ch, uint16_t bps,
                                 uint16_t frame, uint16_t reserved0);
/* Declara obligaciones vivas para ese token. */
void arl_fx_set_retire(uint64_t epoch, uint32_t flags);
/* ACK del framework para ese token: retira solo esas y anota ack_epoch. */
void arl_fx_ack_retire(uint64_t epoch);
uint32_t arl_fx_calls(enum arl_fx_op op);
/* Hechos de hardware observados, no estado del motor. */
int arl_fx_enabled(void);
uint64_t arl_fx_enable_token(void);
uint64_t arl_fx_ack_epoch(void);
int arl_fx_has_owner(void);
int arl_fx_closed(void);
/* Vista del backend YA poseido por el motor; nunca crea otro ni transfiere ownership. */
arl_be *arl_fx_backend(void);

#if defined(__cplusplus)
}
#endif
#endif /* ARL_BACKEND_FIXTURE_H */
