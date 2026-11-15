/* MIT — contrato del SPSC compartido T13a. Implementación aparte.
 * C11 interno; cliente C/C++ mediante esta interfaz C. PCM mono int16_t.
 * Un productor y un consumidor; capacidad fija de 960 muestras.
 * create/destroy sólo fuera de RT. destroy exige actores detenidos y
 * sin futuras entradas: esta cola no es un registro de handles.
 * push/read/drain no asignan, liberan, bloquean ni hacen IO/logs.
 */
#ifndef ARL_CAPTURE_SPSC_H
#define ARL_CAPTURE_SPSC_H
#include "capture-abi.h"
#ifdef __cplusplus
extern "C" {
#endif
typedef struct arl_spsc arl_spsc;
typedef struct arl_spsc_push_result {
    uint32_t written;
    uint32_t dropped;
} arl_spsc_push_result;
/* out null -> ERR_NO_OUTPUT. En cualquier fallo con out válido, *out=null. */
int32_t arl_spsc_create(arl_spsc **out);
/* owner null -> ERR_NULL; *owner null -> OK. Requiere quiescencia externa.
 * En OK libera fuera de RT y deja *owner=null. Aliases previos inválidos. */
int32_t arl_spsc_destroy(arl_spsc **owner);
/* Copia el prefijo min(n,libres), cuenta resto en resultado. Trabajo <=960.
 * out null -> ERR_NO_OUTPUT; con out válido, errores lo ponen a {0,0}.
 * ring null o samples null con n>0 -> ERR_NULL, sin mutación.
 * n=0 acepta samples null y devuelve {0,0}. n>0 exige n muestras legibles.
 * Sólo productor. n no tiene tope480; no recorre muestras descartadas. */
int32_t arl_spsc_push(arl_spsc *ring, const int16_t *samples, uint32_t n,
                      arl_spsc_push_result *out);
/* Sólo consumidor. Máximo240 por read, copia FIFO cruzando wrap hasta
 * min(tope,disponibles), sin rellenar silencio ni tocar resto de dst.
 * read null -> ERR_NO_OUTPUT. Error con read válido -> *read=0.
 * tope>240 -> ERR_ARGS; ring null o dst null con tope>0 -> ERR_NULL.
 * tope0 acepta dst null y no consume. */
int32_t arl_spsc_read(arl_spsc *ring, int16_t *dst, uint32_t tope, uint32_t *read);
/* Sólo consumidor. Descarta min(tope,disponibles), sin leer payload.
 * dropped null -> ERR_NO_OUTPUT; otros errores lo ponen a0.
 * ring null -> ERR_NULL. */
int32_t arl_spsc_drain(arl_spsc *ring, uint32_t tope, uint32_t *dropped);
/* Observacional para productor; free es una lectura acotada, no snapshot
 * transaccional con otras métricas. out null -> ERR_NO_OUTPUT;
 * ring null -> ERR_NULL y *out=0. */
int32_t arl_spsc_free(const arl_spsc *ring, uint32_t *out);
#ifdef __cplusplus
}
#endif
#endif
