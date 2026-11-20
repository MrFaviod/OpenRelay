/* Fixture host de hardware. NO es Oboe ni Android y NO tiene
 * estados del motor ni generacion propia: solo formato negociado, contadores
 * de llamadas y posibilidades de entrada que el test declara. El token que
 * apunta es el que el motor paso como argumento, no uno inventado aqui.
 * Un solo dispositivo simulado: crear dos backends vivos da ERR_BUSY.
 * Guarda el owner opaco que la factory recibe; no lo dereferencia. */
#ifndef OPENRELAY_BACKEND_FIXTURE_H
#define OPENRELAY_BACKEND_FIXTURE_H

#include <stdint.h>

#include "capture-backend.h"

#if defined(__cplusplus)
extern "C" {
#endif

enum openrelay_fx_op {
  OPENRELAY_FX_CREATE = 0,
  OPENRELAY_FX_PREPARE,
  OPENRELAY_FX_EFFECTIVE,
  OPENRELAY_FX_ENABLE,
  OPENRELAY_FX_DISABLE,
  OPENRELAY_FX_CLOSE,
  OPENRELAY_FX_RETIRE,
  OPENRELAY_FX_RELEASE,
  OPENRELAY_FX_OP_COUNT
};

/* Estado neutro: dispositivo cerrado, sin flags, contadores a 0. */
void openrelay_fx_reset(void);
/* Formato CONFIGURADO que el dispositivo negocia en prepare(). No pisa
 * requested ni effective: se guardan aparte. */
void openrelay_fx_set_prepare_format(uint32_t rate, uint16_t ch, uint16_t bps,
                               uint16_t frame, uint16_t reserved0);
/* Formato que effective() relee. Una discrepancia la rechaza el motor. */
void openrelay_fx_set_effective_format(uint32_t rate, uint16_t ch, uint16_t bps,
                                 uint16_t frame, uint16_t reserved0);
/* Declara obligaciones vivas para ese token. */
void openrelay_fx_set_retire(uint64_t epoch, uint32_t flags);
/* ACK del framework para ese token: retira solo esas y anota ack_epoch. */
void openrelay_fx_ack_retire(uint64_t epoch);
uint32_t openrelay_fx_calls(enum openrelay_fx_op op);
/* Hechos de hardware observados, no estado del motor. */
int openrelay_fx_enabled(void);
uint64_t openrelay_fx_enable_token(void);
uint64_t openrelay_fx_ack_epoch(void);
int openrelay_fx_has_owner(void);
int openrelay_fx_closed(void);
/* Vista del backend YA poseido por el motor; nunca crea otro ni transfiere ownership. */
openrelay_be *openrelay_fx_backend(void);

#if defined(__cplusplus)
}
#endif
#endif /* OPENRELAY_BACKEND_FIXTURE_H */
