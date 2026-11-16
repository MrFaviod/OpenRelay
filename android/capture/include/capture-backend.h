/* arl capture - FRONTERA BACKEND PRIVADA (T13a S3c, tests-first).
 * Header INTERNO: no es ABI publico, no se instala, no declara arl_capture_*.
 * El unico motor es el futuro arl_capture_* sobre el SPSC real; aqui solo se
 * fija como habla ese motor con el hardware detras de su objeto opaco.
 *
 * ENLACE Y OWNER. arl_capture_open no recibe backend. El motor reserva su
 * objeto y su anillo, y solo despues llama a la factory privada
 * arl_capture_backend_create(&be, owner): el host enlaza el fixture y el
 * producto enlazara el adapter real (Oboe/AAudio). El owner es la referencia
 * opaca al objeto del motor que el backend conserva para atar sus callbacks a
 * esa instancia; el backend NO mira su layout ni llama al motor desde la
 * factory. Si owner es nulo la factory falla y no crea backend; *out valido
 * queda a NULL. Si la factory falla, el motor libera lo que reservo. Simbolo
 * interno, linkage C, visibilidad hidden. No hay segundo motor ni ring aqui.
 *
 * ORDEN EN START. prepare() abre y negocia con el formato configurado del
 * dispositivo, que NO tiene por que coincidir con requested; devuelve su
 * primer effective; el motor conserva requested aparte. effective() RELEE el formato
 * ya en vigor, asi que un motor que solo validara el primer valor cae en el
 * caso de discrepancia. Solo con la relectura validada contra requested arma
 * el motor su generacion y llama enable(). El effective lo dicta el backend,
 * nunca se hardcodea en el motor; una discrepancia da ERR_FORMAT/ERROR sin
 * publicar muestras ni habilitar callbacks. open no toca hardware.
 *
 * PROPIEDAD Y CONTROL. El backend posee arl_be y su ctx hasta release().
 * prepare/effective/enable/disable/close/retire corren en serie con el worker
 * de control; el callback de datos solo empuja muestras con su token y nunca
 * llama a estas ops. close() cierra el stream conservando el objeto y es
 * distinto de disable() (para el flujo) y de release() (libera memoria): no
 * habilita callbacks nuevos, es idempotente y NO borra obligaciones pendientes,
 * asi que un close correcto no prueba por si solo que el retiro termino.
 *
 * RETIRO. retire() responde una consulta: su codigo OK solo dice que hubo
 * respuesta, NUNCA que *flags sea 0. Las obligaciones vivas van por
 * instancia/token, no por el generation de status (que avanza al parar y en
 * eventos). Un token que no fue habilitado, o anterior a uno conocido, da
 * error y ARL_BE_RETIRE_ALL. Excepcion: instancia NUNCA habilitada, ya cerrada
 * y sin obligaciones, acredita retiro por instancia sin inventar token.
 * Que hoy de 0 tampoco prueba
 * que no vuelva a entrar; por eso MAY_REENTER sigue siendo obligacion. Ni un
 * contador a 0, ni un token viejo, ni un close exitoso demuestran retiro
 * definitivo. destroy() solo libera con flags a 0; si no, ERR_BUSY y *owner no
 * cambia. El mapa del backend NO cubre consumidor/read/drain/getters: esa
 * quiescencia es obligacion del core con su caller y se declara aparte; el
 * mapa a 0 no implica que esos accesos acabaran. Antes de release() el motor
 * deja de aceptar accesos sobre ese objeto, y un alias raw copiado antes no se
 * valida ni se inspecciona despues.
 */
#ifndef ARL_CAPTURE_BACKEND_H
#define ARL_CAPTURE_BACKEND_H

#include <stdint.h>

#include "capture-abi.h"

#if defined(__cplusplus)
extern "C" {
#endif

#if defined(__GNUC__) || defined(__clang__)
#define ARL_BE_INTERNAL __attribute__((visibility("hidden")))
#else
#define ARL_BE_INTERNAL
#endif

/* Obligaciones de retiro, por instancia/token de enable. */
#define ARL_BE_RETIRE_DATA 0x1u        /* callback de datos dentro          */
#define ARL_BE_RETIRE_ERROR 0x2u       /* hilo de error dentro              */
#define ARL_BE_RETIRE_FW 0x4u          /* framework no retiro sus callbacks */
#define ARL_BE_RETIRE_MAY_REENTER 0x8u /* aun podria entregar una entrada   */
#define ARL_BE_RETIRE_ALL 0xFu

/* Opaco para el motor: solo guarda el puntero y lee ops. */
typedef struct arl_be arl_be;

typedef struct arl_be_ops {
  /* Abre hardware y negocia formato. NO habilita callbacks. */
  int32_t (*prepare)(arl_be *be, const arl_capture_format *requested,
                     arl_capture_format *effective);
  /* Relee el formato en vigor. Es lo que el motor valida contra requested. */
  int32_t (*effective)(arl_be *be, arl_capture_format *out);
  /* Habilita callbacks para generation, ya validado el effective. */
  int32_t (*enable)(arl_be *be, uint64_t generation);
  /* Para el flujo y retira callbacks. No drena: los datos siguen en el anillo. */
  int32_t (*disable)(arl_be *be);
  /* Cierra el stream conservando el objeto. Idempotente, no borra flags. */
  int32_t (*close)(arl_be *be);
  /* Obligaciones vivas de ese token. OK no implica *flags==0; token
   * desconocido da error y ARL_BE_RETIRE_ALL. */
  int32_t (*retire)(arl_be *be, uint64_t generation, uint32_t *flags);
  /* Libera be y su ctx. Solo con flags 0 y sin accesos del core en vuelo. */
  void (*release)(arl_be *be);
} arl_be_ops;

/* Primer miembro del objeto del backend: lo embebe y el motor solo lee ops. */
struct arl_be {
  const arl_be_ops *ops;
};

/* Costura unica de enlace. out primero, owner segundo. OK y *out, o error y
 * *out null. owner nulo falla sin crear backend. */
ARL_BE_INTERNAL int32_t arl_capture_backend_create(struct arl_be **out,
                                                  arl_capture *owner);

#if defined(__cplusplus)
}
#endif
#endif /* ARL_CAPTURE_BACKEND_H */
