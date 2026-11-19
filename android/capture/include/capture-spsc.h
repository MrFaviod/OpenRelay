/* MIT — contrato del SPSC compartido T13a. Implementación aparte.
 * C11 interno; cliente C/C++ mediante esta interfaz C. PCM mono int16_t.
 * Un productor y un consumidor; capacidad fija de 960 muestras.
 * create/destroy sólo fuera de RT. destroy exige actores detenidos y
 * sin futuras entradas: esta cola no es un registro de handles.
 * push/read/drain no asignan, liberan, bloquean ni hacen IO/logs.
 */
#ifndef OPENRELAY_CAPTURE_SPSC_H
#define OPENRELAY_CAPTURE_SPSC_H
#include "capture-abi.h"
#ifdef __cplusplus
extern "C" {
#endif
typedef struct openrelay_spsc openrelay_spsc;
typedef struct openrelay_spsc_push_result {
    uint32_t written;
    uint32_t dropped;
} openrelay_spsc_push_result;
/* out null -> ERR_NO_OUTPUT. En cualquier fallo con out válido, *out=null. */
int32_t openrelay_spsc_create(openrelay_spsc **out);
/* owner null -> ERR_NULL; *owner null -> OK. Requiere quiescencia externa.
 * En OK libera fuera de RT y deja *owner=null. Aliases previos inválidos. */
int32_t openrelay_spsc_destroy(openrelay_spsc **owner);
/* Copia el prefijo min(n,libres), cuenta resto en resultado. Trabajo <=960.
 * out null -> ERR_NO_OUTPUT; con out válido, errores lo ponen a {0,0}.
 * ring null o samples null con n>0 -> ERR_NULL, sin mutación.
 * n=0 acepta samples null y devuelve {0,0}. n>0 exige n muestras legibles.
 * Sólo productor. n no tiene tope480; no recorre muestras descartadas. */
int32_t openrelay_spsc_push(openrelay_spsc *ring, const int16_t *samples, uint32_t n,
                      openrelay_spsc_push_result *out);
/* Sólo consumidor. Máximo240 por read, copia FIFO cruzando wrap hasta
 * min(tope,disponibles), sin rellenar silencio ni tocar resto de dst.
 * read null -> ERR_NO_OUTPUT. Error con read válido -> *read=0.
 * tope>240 -> ERR_ARGS; ring null o dst null con tope>0 -> ERR_NULL.
 * tope0 acepta dst null y no consume. */
int32_t openrelay_spsc_read(openrelay_spsc *ring, int16_t *dst, uint32_t tope, uint32_t *read);
/* Sólo consumidor. Descarta min(tope,disponibles), sin leer payload.
 * dropped null -> ERR_NO_OUTPUT; otros errores lo ponen a0.
 * ring null -> ERR_NULL. */
int32_t openrelay_spsc_drain(openrelay_spsc *ring, uint32_t tope, uint32_t *dropped);
/* Observacional para productor; free es una lectura acotada, no snapshot
 * transaccional con otras métricas. out null -> ERR_NO_OUTPUT;
 * ring null -> ERR_NULL y *out=0. */
int32_t openrelay_spsc_free(const openrelay_spsc *ring, uint32_t *out);
#ifdef __cplusplus
}
#endif
#endif
