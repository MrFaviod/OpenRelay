/* capture-owned.h - API PRIVADA del modulo (no ABI). Wrappers sobre
 * openrelay_capture_* que toman prestamo de openrelay_capture_owner por llamada.
 * Acquire con exito -> llamada raw -> release, en todos los caminos.
 * Si el acquire falla, devuelve OPENRELAY_OWNED_ERR_OWNER_BUSY (separado de los
 * codigos raw) y no toca ningun output, salvo read/drain que ponen *n=0 /
 * *dropped=0 si el puntero existe. Un solo prestamo por llamada.
 * Control (start/stop/close/get_status) sigue serializado por el llamador.
 */
#ifndef OPENRELAY_CAPTURE_OWNED_H
#define OPENRELAY_CAPTURE_OWNED_H
#include <stdint.h>
#include "capture-abi.h"
#include "capture-owner.h"
#define OPENRELAY_OWNED_ERR_OWNER_BUSY (-8)
#if defined(__cplusplus)
extern "C" {
#endif
int32_t openrelay_capture_owned_start(openrelay_capture_owner *o);
int32_t openrelay_capture_owned_stop(openrelay_capture_owner *o);
int32_t openrelay_capture_owned_close(openrelay_capture_owner *o);
int32_t openrelay_capture_owned_read(openrelay_capture_owner *o, int16_t *dst, uint32_t *n);
int32_t openrelay_capture_owned_drain(openrelay_capture_owner *o, uint32_t *dropped);
int32_t openrelay_capture_owned_get_status(openrelay_capture_owner *o, openrelay_capture_status *out);
int32_t openrelay_capture_owned_get_counters(openrelay_capture_owner *o, openrelay_capture_counters *out);
int32_t openrelay_capture_owned_push(openrelay_capture_owner *o, uint64_t generation,
                               const int16_t *samples, uint32_t n);
int32_t openrelay_capture_owned_note_error(openrelay_capture_owner *o, uint64_t generation, int32_t code);
int32_t openrelay_capture_owned_note_disconnect(openrelay_capture_owner *o, uint64_t generation);
#if defined(__cplusplus)
}
#endif
#endif
