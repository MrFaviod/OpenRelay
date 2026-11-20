/* openrelay capture ABI v1 — baseline para implementar y probar. No es ejecución.
 * Declaraciones únicamente. Sin implementación, sin Oboe, sin JNI.
 * Autocontenido: C99, anchos fijos, sin cabeceras del producto.
 *
 * PCM exigido: 48 kHz, mono, S16LE, frame de cable de 240 muestras / 5 ms.
 * El callback entrega un número variable de muestras. Ese número no es el
 * frame de cable y no tiene tope de 480. La copia está acotada por el
 * hueco del anillo, no por un máximo inventado del callback.
 *
 * Vida: close deja el objeto vivo. destroy es la única llamada que
 * libera, y sólo con precondiciones válidas. Recibe openrelay_capture ** y,
 * al volver OK, escribe *owner = null. No hay tabla de handles: un
 * alias previo queda inválido y no se inspecciona. La generación
 * descarta audio viejo; no decide si un puntero sigue vivo.
 *
 * Hilos: un productor (callback RT) y un consumidor (worker de audio).
 * Un worker de control posee open/start/stop/close/destroy. El callback
 * no llama a JNI, red, alloc, free, locks, log, IO, stop, close ni destroy.
 */
#ifndef OPENRELAY_CAPTURE_ABI_H
#define OPENRELAY_CAPTURE_ABI_H

#include <stddef.h>
#include <stdint.h>

#define OPENRELAY_CAPTURE_ABI_MAJOR 1u
#define OPENRELAY_CAPTURE_ABI_MINOR 3u

#define OPENRELAY_CAPTURE_SAMPLE_RATE_HZ 48000u
#define OPENRELAY_CAPTURE_CHANNELS 1u
#define OPENRELAY_CAPTURE_FRAME_SAMPLES 240u
#define OPENRELAY_CAPTURE_FRAME_US 5000u
#define OPENRELAY_CAPTURE_BYTES_PER_SAMPLE 2u
#define OPENRELAY_CAPTURE_FRAME_BYTES 480u

/* Capacidad fija en muestras, no en frames. 20 ms = 4 frames de cable.
 * Propuesta de este corte: el contrato de PCM no fija este número. */
#define OPENRELAY_CAPTURE_RING_SAMPLES 960u

#define OPENRELAY_CAPTURE_OK 0
#define OPENRELAY_CAPTURE_ERR_NULL (-1)
#define OPENRELAY_CAPTURE_ERR_STATE (-2)
#define OPENRELAY_CAPTURE_ERR_ARGS (-3)
#define OPENRELAY_CAPTURE_ERR_FORMAT (-4)
#define OPENRELAY_CAPTURE_ERR_BUSY (-5)
#define OPENRELAY_CAPTURE_ERR_CLOSED (-6)
/* El llamador pasó un destino null donde la llamada escribe un conteo. */
#define OPENRELAY_CAPTURE_ERR_NO_OUTPUT (-7)

#define OPENRELAY_CAPTURE_STATE_CLOSED 0u
#define OPENRELAY_CAPTURE_STATE_OPEN 1u
#define OPENRELAY_CAPTURE_STATE_STARTING 2u
#define OPENRELAY_CAPTURE_STATE_STARTED 3u
#define OPENRELAY_CAPTURE_STATE_STOPPING 4u
#define OPENRELAY_CAPTURE_STATE_STOPPED 5u
#define OPENRELAY_CAPTURE_STATE_ERROR 6u
#define OPENRELAY_CAPTURE_STATE_DISCONNECTED 7u

/* 0 en todo campo que el contrato no usa. Un valor distinto es ERR_ARGS. */
typedef struct openrelay_capture_format {
  uint32_t sample_rate_hz;
  uint16_t channels;
  uint16_t bytes_per_sample;
  uint16_t frame_samples;
  uint16_t reserved0;
} openrelay_capture_format;

/* Seis contadores. Cada lectura copia los seis. Esa copia NO es
 * simultánea: dos campos pueden corresponder a instantes distintos y
 * no se relacionan con state ni con generation. */
typedef struct openrelay_capture_counters {
  uint64_t samples_written;
  uint64_t samples_read;
  uint64_t samples_dropped;
  uint64_t underrun_samples;
  uint64_t callbacks;
  uint64_t generation;
} openrelay_capture_counters;

typedef struct openrelay_capture_status {
  uint64_t generation;
  uint32_t state;
  int32_t last_error;
  openrelay_capture_format requested;
  openrelay_capture_format effective;
} openrelay_capture_status;

/* Opaco. El dueño guarda openrelay_capture * y lo pasa como openrelay_capture **
 * a destroy. Tras OK, ese puntero es null. */
typedef struct openrelay_capture openrelay_capture;

#if defined(__cplusplus)
extern "C" {
#endif

/* Worker de control. Crea el objeto en OPEN y reserva cap_samples.
 * v1 acepta sólo OPENRELAY_CAPTURE_RING_SAMPLES; otro valor es ERR_ARGS y
 * *out queda null. out null es ERR_NULL y *out no se escribe.
 * No consulta RECORD_AUDIO: el permiso lo gestiona la capa de UI. */
int32_t openrelay_capture_open(openrelay_capture **out, uint32_t cap_samples);

/* Worker de control. Tabla normativa en acceptance.json/lifecycle.
 * start admite OPEN o STOPPED; STARTED de nuevo devuelve OK sin mutar.
 * stop admite STARTED; STOPPED de nuevo devuelve OK sin mutar.
 * start en CLOSED devuelve ERR_CLOSED: destruir y crear otro objeto.
 * Los demás orígenes inválidos devuelven ERR_STATE sin mutación.
 * start valida effective del adapter, no inventa formato/permiso:
 * discrepancia -> ERROR/ERR_FORMAT, sin publicar muestras. No convierte.
 * start prepara una generación nueva antes de habilitar callbacks;
 * stop invalida esa generación y retira/espera accesos antes de STOPPED.
 * El host sólo puede simular el adapter, no observa permiso ni hardware. */
int32_t openrelay_capture_start(openrelay_capture *cap);
int32_t openrelay_capture_stop(openrelay_capture *cap);

/* Cierra el stream y deja el objeto vivo, en CLOSED, con la misma
 * generación. Origen: OPEN, STOPPED, ERROR o DISCONNECTED. Idempotente si
 * ya está CLOSED. ERR_STATE en otro origen. No libera. read y drain
 * siguen siendo válidos en CLOSED. */
int32_t openrelay_capture_close(openrelay_capture *cap);

/* Única llamada que libera. owner null o *owner null es ERR_NULL y
 * no escribe nada.
 * Estado distinto de CLOSED -> ERR_STATE, sin mutar *owner.
 * Demás precondiciones, todas: callback de datos fuera;
 * callback de error fuera; consumidor fuera (read, drain y getters);
 * y, en el camino Oboe, el framework ya cerró el stream y retiró los
 * dos callbacks. Sin framework abierto (objeto sólo creado) esa condición
 * se cumple sin llamar a Oboe. Si falta una, ERR_BUSY y *owner no cambia.
 * ERROR o DISCONNECTED no bastan: primero close.
 * Antes de liberar, la implementación deja de aceptar push, read,
 * drain y getters sobre ese objeto. Al volver OK escribe *owner = null.
 * Un alias copiado antes queda inválido. No se inspecciona y no hay
 * código de retorno para él: no existe tabla de handles.
 * La generación no se consulta. No decide la vida del objeto. */
int32_t openrelay_capture_destroy(openrelay_capture **owner);

/* Consumidor, fuera de RT. *n es tope y resultado.
 * Copia min(*n, disponibles, muestras contiguas) y escribe esa cantidad
 * en *n. Si hay menos, no rellena silencio.
 * n null es ERR_NO_OUTPUT y no consume. Si n existe, en cualquier error
 * *n=0. Tope de entrada >240 es ERR_ARGS, sin consumo. n=0 acepta dst null
 * y devuelve OK. dst null con entrada positiva es ERR_NULL sin consumo.
 * cap null es ERR_NULL. OPEN/STARTED/STOPPED/CLOSED permiten read;
 * STARTING/STOPPING devuelven ERR_BUSY; ERROR/DISCONNECTED ERR_CLOSED. */
int32_t openrelay_capture_read(openrelay_capture *cap, int16_t *dst, uint32_t *n);

/* Consumidor, fuera de RT. Descarta min(*dropped, disponibles) y
 * escribe cuántas descartó. No bloquea. No se llama desde el callback.
 * dropped null es ERR_NO_OUTPUT y no descarta. cap null es ERR_NULL.
 * Si dropped existe, en cualquier error *dropped=0. Mismos estados que read.
 * Sólo el consumidor modifica su cursor; stop/close no drenan desde control. */
int32_t openrelay_capture_drain(openrelay_capture *cap, uint32_t *dropped);

/* Sólo el worker de control, en serie con start/stop/close/destroy.
 * Copia state, generation, last_error, requested y effective de un
 * único instante. No promete relación con los contadores. out null es
 * ERR_NO_OUTPUT; cap null es ERR_NULL. En error no escribe el struct.
 * note_error/disconnect sólo publican un evento acotado; el control lo
 * aplica antes del snapshot. El hilo de error no muta este estado compuesto. */
int32_t openrelay_capture_get_status(const openrelay_capture *cap, openrelay_capture_status *out);

/* Copia los seis contadores. Los seis NO son simultáneos entre sí ni
 * con el status. out null es ERR_NO_OUTPUT. cap null es ERR_NULL. */
int32_t openrelay_capture_get_counters(const openrelay_capture *cap,
                                 openrelay_capture_counters *out);

/* Productor RT. Copia min(n, hueco) muestras y suma el resto a
 * samples_dropped. No rechaza n grande: un callback de 1024 con hueco
 * 960 escribe 960 y cuenta 64. El trabajo es proporcional al hueco, no
 * a n. samples null con n>0 es ERR_NULL y no cuenta callback.
 * generation distinta de la vigente, o estado que no admita datos:
 * no copia, devuelve ERR_CLOSED. Sólo STARTED admite push normal.
 * cap null -> ERR_NULL. n=0 y samples null es válido sin copia.
 * Eso sólo vale mientras el objeto sigue vivo. El caller garantiza n
 * muestras legibles cuando n>0: esta ABI no valida punteros arbitrarios. */
int32_t openrelay_capture_push(openrelay_capture *cap, uint64_t generation,
                         const int16_t *samples, uint32_t n);

/* Hilo de error, nunca el callback de datos. Publica ERROR o
 * DISCONNECTED y avanza la generación. No cierra, no destruye y no
 * retira callbacks: quien cierra depende del camino (ver result.md). */
/* Token del stream que originó el evento. Evento viejo -> ERR_CLOSED sin
 * mutar la sesión actual. Publicación acotada, sin alloc ni locks RT. */
int32_t openrelay_capture_note_error(openrelay_capture *cap, uint64_t generation, int32_t code);
int32_t openrelay_capture_note_disconnect(openrelay_capture *cap, uint64_t generation);

#if defined(__cplusplus)
}
#endif

#endif /* OPENRELAY_CAPTURE_ABI_H */
