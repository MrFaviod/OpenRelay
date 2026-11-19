/* Private caller-side lifetime owner. Not part of capture ABI.
 * Storage is caller-owned and MUST outlive all attempts to acquire it.
 * No caller may read cap without a successful borrow. Borrow lasts for the
 * entire raw capture call. Control methods remain serialized as in ABI.
 * Backend callback context lifetime/retirement is a separate obligation.
 * init is serial, takes ownership of a non-null live capture.
 * try_destroy: single bounded close attempt; borrowers => ERR_BUSY, no wait.
 * Core failure reopens admission without changing cap. Success leaves closed
 * owner with cap NULL. Owner storage itself is NOT freed by try_destroy.
 * acquire: bounded attempt, contention/full/closed => ERR_BUSY, *out=NULL.
 * release: exactly once per successful acquire, no accesses to cap afterward.
 */
#ifndef OPENRELAY_CAPTURE_OWNER_H
#define OPENRELAY_CAPTURE_OWNER_H
#include <stdatomic.h>
#include <stdint.h>
#include "capture-abi.h"
#define OPENRELAY_OWNER_CLOSED UINT32_C(0x80000000)
#define OPENRELAY_OWNER_MAX_BORROWS UINT32_C(0x7fffffff)
typedef struct openrelay_capture_owner {
  _Atomic uint32_t gate;
  openrelay_capture *cap;
} openrelay_capture_owner;
int32_t openrelay_owner_init(openrelay_capture_owner *owner, openrelay_capture *cap);
int32_t openrelay_owner_acquire(openrelay_capture_owner *owner, openrelay_capture **out);
void openrelay_owner_release(openrelay_capture_owner *owner);
int32_t openrelay_owner_try_destroy(openrelay_capture_owner *owner);
#endif
