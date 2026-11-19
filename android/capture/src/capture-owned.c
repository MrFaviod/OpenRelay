/* src/capture-owned.c - wrappers privados con prestamo de owner (ver header). */
#include <stddef.h>
#include "capture-owned.h"

/* Acquire -> llamada raw -> release. Un solo prestamo por llamada; ninguna
 * llamada raw anida otro prestamo. */
static int32_t loan_begin(openrelay_capture_owner *o, openrelay_capture **cap) {
  if (o == NULL) {
    return OPENRELAY_CAPTURE_ERR_NULL;
  }
  if (openrelay_owner_acquire(o, cap) != OPENRELAY_CAPTURE_OK) {
    return OPENRELAY_OWNED_ERR_OWNER_BUSY;
  }
  return OPENRELAY_CAPTURE_OK;
}

int32_t openrelay_capture_owned_start(openrelay_capture_owner *o) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_start(cap);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_stop(openrelay_capture_owner *o) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_stop(cap);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_close(openrelay_capture_owner *o) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_close(cap);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_read(openrelay_capture_owner *o, int16_t *dst, uint32_t *n) {
  openrelay_capture *cap;
  int32_t rc;
  if (n == NULL) {
    return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  }
  rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    *n = 0u;
    return rc;
  }
  rc = openrelay_capture_read(cap, dst, n);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_drain(openrelay_capture_owner *o, uint32_t *dropped) {
  openrelay_capture *cap;
  int32_t rc;
  if (dropped == NULL) {
    return OPENRELAY_CAPTURE_ERR_NO_OUTPUT;
  }
  rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    *dropped = 0u;
    return rc;
  }
  rc = openrelay_capture_drain(cap, dropped);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_get_status(openrelay_capture_owner *o, openrelay_capture_status *out) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_get_status(cap, out);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_get_counters(openrelay_capture_owner *o, openrelay_capture_counters *out) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_get_counters(cap, out);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_push(openrelay_capture_owner *o, uint64_t generation,
                               const int16_t *samples, uint32_t n) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_push(cap, generation, samples, n);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_note_error(openrelay_capture_owner *o, uint64_t generation, int32_t code) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_note_error(cap, generation, code);
  openrelay_owner_release(o);
  return rc;
}

int32_t openrelay_capture_owned_note_disconnect(openrelay_capture_owner *o, uint64_t generation) {
  openrelay_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    return rc;
  }
  rc = openrelay_capture_note_disconnect(cap, generation);
  openrelay_owner_release(o);
  return rc;
}
