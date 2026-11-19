/* Preparacion comun de las pruebas del owner y de los controles del core.
 * Solo usa la API publica del core real base. Sin owner, sin stubs. */
#ifndef OPENRELAY_OWNER_TEST_SETUP_H
#define OPENRELAY_OWNER_TEST_SETUP_H

#include <stdint.h>
#include <stdio.h>

#include "capture-abi.h"
#include "backend_fixture.h"

#define OT_NSAMP OPENRELAY_CAPTURE_FRAME_SAMPLES

/* Captura CERRADA con 240 muestras conocidas en el anillo (patron i+1).
 * Setup valido: open/start/push/stop/close. Devuelve 0 o 2 (SETUP_FAIL). */
static inline int setup_closed(openrelay_capture **cap) {
  openrelay_capture_status st;
  int16_t pat[OT_NSAMP];
  uint32_t i;
  int32_t rc;

  for (i = 0; i < OT_NSAMP; i++) pat[i] = (int16_t)(i + 1u);
  openrelay_fx_reset();
  openrelay_fx_set_prepare_format(OPENRELAY_CAPTURE_SAMPLE_RATE_HZ, OPENRELAY_CAPTURE_CHANNELS,
                            OPENRELAY_CAPTURE_BYTES_PER_SAMPLE,
                            OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u);
  openrelay_fx_set_effective_format(OPENRELAY_CAPTURE_SAMPLE_RATE_HZ, OPENRELAY_CAPTURE_CHANNELS,
                              OPENRELAY_CAPTURE_BYTES_PER_SAMPLE,
                              OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u);
  *cap = NULL;
  rc = openrelay_capture_open(cap, OPENRELAY_CAPTURE_RING_SAMPLES);
  if (rc != OPENRELAY_CAPTURE_OK || *cap == NULL) {
    printf("SETUP_FAIL open rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_start(*cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL start rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_get_status(*cap, &st);
  if (rc != OPENRELAY_CAPTURE_OK || st.state != OPENRELAY_CAPTURE_STATE_STARTED) {
    printf("SETUP_FAIL status rc=%d state=%u\n", rc, (unsigned)st.state);
    return 2;
  }
  rc = openrelay_capture_push(*cap, st.generation, pat, OT_NSAMP);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL push rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_stop(*cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL stop rc=%d\n", rc);
    return 2;
  }
  rc = openrelay_capture_close(*cap);
  if (rc != OPENRELAY_CAPTURE_OK) {
    printf("SETUP_FAIL close rc=%d\n", rc);
    return 2;
  }
  return 0;
}

static inline int pattern_ok(const int16_t *d) {
  uint32_t i;
  for (i = 0; i < OT_NSAMP; i++) {
    if (d[i] != (int16_t)(i + 1u)) return 0;
  }
  return 1;
}

#endif /* OPENRELAY_OWNER_TEST_SETUP_H */
