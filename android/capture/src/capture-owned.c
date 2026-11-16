/* src/capture-owned.c - wrappers privados con prestamo de owner (ver header). */
#include <stddef.h>
#include "capture-owned.h"

/* Acquire -> llamada raw -> release. Un solo prestamo por llamada; ninguna
 * llamada raw anida otro prestamo. */
static int32_t loan_begin(arl_capture_owner *o, arl_capture **cap) {
  if (o == NULL) {
    return ARL_CAPTURE_ERR_NULL;
  }
  if (arl_owner_acquire(o, cap) != ARL_CAPTURE_OK) {
    return ARL_OWNED_ERR_OWNER_BUSY;
  }
  return ARL_CAPTURE_OK;
}

int32_t arl_capture_owned_start(arl_capture_owner *o) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_start(cap);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_stop(arl_capture_owner *o) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_stop(cap);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_close(arl_capture_owner *o) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_close(cap);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_read(arl_capture_owner *o, int16_t *dst, uint32_t *n) {
  arl_capture *cap;
  int32_t rc;
  if (n == NULL) {
    return ARL_CAPTURE_ERR_NO_OUTPUT;
  }
  rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    *n = 0u;
    return rc;
  }
  rc = arl_capture_read(cap, dst, n);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_drain(arl_capture_owner *o, uint32_t *dropped) {
  arl_capture *cap;
  int32_t rc;
  if (dropped == NULL) {
    return ARL_CAPTURE_ERR_NO_OUTPUT;
  }
  rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    *dropped = 0u;
    return rc;
  }
  rc = arl_capture_drain(cap, dropped);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_get_status(arl_capture_owner *o, arl_capture_status *out) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_get_status(cap, out);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_get_counters(arl_capture_owner *o, arl_capture_counters *out) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_get_counters(cap, out);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_push(arl_capture_owner *o, uint64_t generation,
                               const int16_t *samples, uint32_t n) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_push(cap, generation, samples, n);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_note_error(arl_capture_owner *o, uint64_t generation, int32_t code) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_note_error(cap, generation, code);
  arl_owner_release(o);
  return rc;
}

int32_t arl_capture_owned_note_disconnect(arl_capture_owner *o, uint64_t generation) {
  arl_capture *cap;
  int32_t rc = loan_begin(o, &cap);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  rc = arl_capture_note_disconnect(cap, generation);
  arl_owner_release(o);
  return rc;
}
