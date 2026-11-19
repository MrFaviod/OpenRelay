/* tests/owned_smoke.c - smoke de wrappers owned sobre el core real (fixture de backend). */
#include <stdio.h>
#include <stdint.h>

#include "capture-abi.h"
#include "capture-owner.h"
#include "capture-owned.h"
#include "backend_fixture.h"

static int fails = 0, cases = 0;
#define CHECK(c, msg) do { cases++; if (!(c)) { fails++; printf("FAIL %s\n", msg); } else { printf("ok %s\n", msg); } } while (0)

int main(void) {
  openrelay_capture_owner o;
  openrelay_capture *cap = NULL;
  int16_t buf[OPENRELAY_CAPTURE_FRAME_SAMPLES] = {0};
  uint32_t n, dropped;
  openrelay_capture_status st;
  openrelay_capture_counters ctr;

  openrelay_fx_reset();
  openrelay_fx_set_prepare_format(OPENRELAY_CAPTURE_SAMPLE_RATE_HZ, OPENRELAY_CAPTURE_CHANNELS,
                            OPENRELAY_CAPTURE_BYTES_PER_SAMPLE, OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u);
  openrelay_fx_set_effective_format(OPENRELAY_CAPTURE_SAMPLE_RATE_HZ, OPENRELAY_CAPTURE_CHANNELS,
                              OPENRELAY_CAPTURE_BYTES_PER_SAMPLE, OPENRELAY_CAPTURE_FRAME_SAMPLES, 0u);
  CHECK(openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES) == OPENRELAY_CAPTURE_OK, "open");
  CHECK(openrelay_owner_init(&o, cap) == OPENRELAY_CAPTURE_OK, "owner init");

  /* Control via wrappers. */
  CHECK(openrelay_capture_owned_start(&o) == OPENRELAY_CAPTURE_OK, "owned start OPEN->STARTED");
  CHECK(openrelay_capture_owned_get_status(&o, &st) == OPENRELAY_CAPTURE_OK &&
        st.state == OPENRELAY_CAPTURE_STATE_STARTED && st.generation == 1u, "owned status STARTED gen1");

  /* Push: token vigente OK, token viejo ERR_CLOSED (sin BUSY). */
  CHECK(openrelay_capture_owned_push(&o, 1u, buf, OPENRELAY_CAPTURE_FRAME_SAMPLES) == OPENRELAY_CAPTURE_OK, "owned push token vigente OK");
  CHECK(openrelay_capture_owned_push(&o, 2u, buf, OPENRELAY_CAPTURE_FRAME_SAMPLES) == OPENRELAY_CAPTURE_ERR_CLOSED, "owned push token viejo ERR_CLOSED");

  /* Read/drain/counters con owner: OK y outputs escritos. */
  n = OPENRELAY_CAPTURE_FRAME_SAMPLES;
  CHECK(openrelay_capture_owned_read(&o, buf, &n) == OPENRELAY_CAPTURE_OK && n == OPENRELAY_CAPTURE_FRAME_SAMPLES, "owned read 240");
  dropped = 0u;
  CHECK(openrelay_capture_owned_drain(&o, &dropped) == OPENRELAY_CAPTURE_OK, "owned drain OK");
  CHECK(openrelay_capture_owned_get_counters(&o, &ctr) == OPENRELAY_CAPTURE_OK && ctr.samples_written == OPENRELAY_CAPTURE_FRAME_SAMPLES, "owned counters written=240");

  /* Read con output NULL: NO_OUTPUT sin prestamo. */
  CHECK(openrelay_capture_owned_read(&o, buf, NULL) == OPENRELAY_CAPTURE_ERR_NO_OUTPUT, "owned read NULL n => NO_OUTPUT");
  CHECK(openrelay_capture_owned_drain(&o, NULL) == OPENRELAY_CAPTURE_ERR_NO_OUTPUT, "owned drain NULL => NO_OUTPUT");

  /* Stop/close via wrappers. */
  CHECK(openrelay_capture_owned_stop(&o) == OPENRELAY_CAPTURE_OK, "owned stop STARTED->STOPPED");
  CHECK(openrelay_capture_owned_close(&o) == OPENRELAY_CAPTURE_OK, "owned close STOPPED->CLOSED");

  /* Prestamo vivo manual: destroy BUSY, cap intacto; wrapper concurrente sigue (multi-prestamo). */
  {
    openrelay_capture *tmp = NULL;
    CHECK(openrelay_owner_acquire(&o, &tmp) == OPENRELAY_CAPTURE_OK && tmp == cap, "manual loan acquire");
    CHECK(openrelay_owner_try_destroy(&o) == OPENRELAY_CAPTURE_ERR_BUSY && o.cap == cap, "try_destroy BUSY while loaned, cap intact");
    openrelay_owner_release(&o);
  }

  CHECK(openrelay_owner_try_destroy(&o) == OPENRELAY_CAPTURE_OK && o.cap == NULL, "owner destroy OK quiescent, cap NULL");

  /* Tras destroy: wrappers devuelven OWNER_BUSY (owner cerrado), no CLOSED, outputs a 0. */
  n = 99u;
  CHECK(openrelay_capture_owned_read(&o, buf, &n) == OPENRELAY_OWNED_ERR_OWNER_BUSY && n == 0u, "after destroy read => OWNED_BUSY, n=0");
  dropped = 99u;
  CHECK(openrelay_capture_owned_drain(&o, &dropped) == OPENRELAY_OWNED_ERR_OWNER_BUSY && dropped == 0u, "after destroy drain => OWNED_BUSY, dropped=0");
  CHECK(openrelay_capture_owned_push(&o, 1u, buf, 1u) == OPENRELAY_OWNED_ERR_OWNER_BUSY, "after destroy push => OWNED_BUSY, not CLOSED");

  printf("OWNED_SMOKE_CASES=%d fails=%d\n", cases, fails);
  printf(fails ? "RESULT=FAIL owned smoke\n" : "RESULT=PASS owned smoke\n");
  return fails ? 1 : 0;
}
