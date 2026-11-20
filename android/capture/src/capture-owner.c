/* src/capture-owner.c - owner privado de vida caller-owned.
 *
 * gate: bits 0..30 cuentan prestamos vivos, bit 31 = cerrado. Un prestamo
 * exige gate abierto y un unico compare_exchange_strong de cuenta+1. release
 * resta con release. try_destroy hace CAS 0 -> CLOSED, sin espera; solo con
 * esa cerradura libre llama a openrelay_capture_destroy. Si el core falla, reabre
 * la admision sin tocar cap. Exito: cap NULL y owner cerrado.
 *
 * Linealizacion: el CAS de acquire (exito) y el CAS 0->CLOSED de try_destroy.
 * Un prestamo y un destroy no coexisten: cada uno cambia el gate de forma
 * atomica. Los accesos a cap se hacen solo con prestamo o con CLOSED
 * exclusivo. Este modulo no basta por si solo para seguridad raw: el
 * llamador debe respetar que todo acceso a cap pasa por acquire.
 */
#include <stdatomic.h>
#include <stdint.h>

#include "capture-abi.h"
#include "capture-owner.h"

#define OWNER_BORROWS_MAX OPENRELAY_OWNER_MAX_BORROWS

int32_t openrelay_owner_init(openrelay_capture_owner *owner, openrelay_capture *cap) {
  if (owner == NULL || cap == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (!atomic_is_lock_free(&owner->gate)) {
    return OPENRELAY_CAPTURE_ERR_STATE;
  }
  atomic_store(&owner->gate, 0u);
  owner->cap = cap;
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_owner_acquire(openrelay_capture_owner *owner, openrelay_capture **out) {
  uint32_t g;
  uint32_t expected;
  openrelay_capture *cap;

  if (owner == NULL || out == NULL) {
    if (out != NULL) {
      *out = NULL;
    }
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  *out = NULL;
  g = atomic_load_explicit(&owner->gate, memory_order_relaxed);
  if ((g & OPENRELAY_OWNER_CLOSED) != 0u || (g & OWNER_BORROWS_MAX) == OWNER_BORROWS_MAX) {
    return OPENRELAY_CAPTURE_ERR_BUSY;
  }
  expected = g;
  if (!atomic_compare_exchange_strong_explicit(&owner->gate, &expected, g + 1u,
                                               memory_order_acquire,
                                               memory_order_relaxed)) {
    return OPENRELAY_CAPTURE_ERR_BUSY; /* un solo intento; cap intacto */
  }
  cap = owner->cap; /* solo tras prestamo exitoso */
  *out = cap;
  return OPENRELAY_CAPTURE_OK;
}

void openrelay_owner_release(openrelay_capture_owner *owner) {
  atomic_fetch_sub_explicit(&owner->gate, 1u, memory_order_release);
}

int32_t openrelay_owner_try_destroy(openrelay_capture_owner *owner) {
  uint32_t expected = 0u;
  openrelay_capture *cap;
  int32_t rc;

  if (owner == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if ((atomic_load_explicit(&owner->gate, memory_order_acquire) & OPENRELAY_OWNER_CLOSED) != 0u) {
    return OPENRELAY_CAPTURE_ERR_NULL; /* ya cerrado: cap es NULL */
  }
  if (!atomic_compare_exchange_strong_explicit(&owner->gate, &expected,
                                               OPENRELAY_OWNER_CLOSED,
                                               memory_order_acq_rel,
                                               memory_order_acquire)) {
    return OPENRELAY_CAPTURE_ERR_BUSY; /* hay prestamos; no espera */
  }
  cap = owner->cap;
  rc = openrelay_capture_destroy(&cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    /* Core fallo: reabre admision. cap no cambia. */
    atomic_fetch_and_explicit(&owner->gate, ~OPENRELAY_OWNER_CLOSED, memory_order_release);
    return rc;
  }
  owner->cap = NULL; /* cap es NULL tras destroy OK */
  return OPENRELAY_CAPTURE_OK;
}
