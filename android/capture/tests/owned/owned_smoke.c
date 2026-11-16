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
  arl_capture_owner o;
  arl_capture *cap = NULL;
  int16_t buf[ARL_CAPTURE_FRAME_SAMPLES] = {0};
  uint32_t n, dropped;
  arl_capture_status st;
  arl_capture_counters ctr;

  arl_fx_reset();
  arl_fx_set_prepare_format(ARL_CAPTURE_SAMPLE_RATE_HZ, ARL_CAPTURE_CHANNELS,
                            ARL_CAPTURE_BYTES_PER_SAMPLE, ARL_CAPTURE_FRAME_SAMPLES, 0u);
  arl_fx_set_effective_format(ARL_CAPTURE_SAMPLE_RATE_HZ, ARL_CAPTURE_CHANNELS,
                              ARL_CAPTURE_BYTES_PER_SAMPLE, ARL_CAPTURE_FRAME_SAMPLES, 0u);
  CHECK(arl_capture_open(&cap, ARL_CAPTURE_RING_SAMPLES) == ARL_CAPTURE_OK, "open");
  CHECK(arl_owner_init(&o, cap) == ARL_CAPTURE_OK, "owner init");

  /* Control via wrappers. */
  CHECK(arl_capture_owned_start(&o) == ARL_CAPTURE_OK, "owned start OPEN->STARTED");
  CHECK(arl_capture_owned_get_status(&o, &st) == ARL_CAPTURE_OK &&
        st.state == ARL_CAPTURE_STATE_STARTED && st.generation == 1u, "owned status STARTED gen1");

  /* Push: token vigente OK, token viejo ERR_CLOSED (sin BUSY). */
  CHECK(arl_capture_owned_push(&o, 1u, buf, ARL_CAPTURE_FRAME_SAMPLES) == ARL_CAPTURE_OK, "owned push token vigente OK");
  CHECK(arl_capture_owned_push(&o, 2u, buf, ARL_CAPTURE_FRAME_SAMPLES) == ARL_CAPTURE_ERR_CLOSED, "owned push token viejo ERR_CLOSED");

  /* Read/drain/counters con owner: OK y outputs escritos. */
  n = ARL_CAPTURE_FRAME_SAMPLES;
  CHECK(arl_capture_owned_read(&o, buf, &n) == ARL_CAPTURE_OK && n == ARL_CAPTURE_FRAME_SAMPLES, "owned read 240");
  dropped = 0u;
  CHECK(arl_capture_owned_drain(&o, &dropped) == ARL_CAPTURE_OK, "owned drain OK");
  CHECK(arl_capture_owned_get_counters(&o, &ctr) == ARL_CAPTURE_OK && ctr.samples_written == ARL_CAPTURE_FRAME_SAMPLES, "owned counters written=240");

  /* Read con output NULL: NO_OUTPUT sin prestamo. */
  CHECK(arl_capture_owned_read(&o, buf, NULL) == ARL_CAPTURE_ERR_NO_OUTPUT, "owned read NULL n => NO_OUTPUT");
  CHECK(arl_capture_owned_drain(&o, NULL) == ARL_CAPTURE_ERR_NO_OUTPUT, "owned drain NULL => NO_OUTPUT");

  /* Stop/close via wrappers. */
  CHECK(arl_capture_owned_stop(&o) == ARL_CAPTURE_OK, "owned stop STARTED->STOPPED");
  CHECK(arl_capture_owned_close(&o) == ARL_CAPTURE_OK, "owned close STOPPED->CLOSED");

  /* Prestamo vivo manual: destroy BUSY, cap intacto; wrapper concurrente sigue (multi-prestamo). */
  {
    arl_capture *tmp = NULL;
    CHECK(arl_owner_acquire(&o, &tmp) == ARL_CAPTURE_OK && tmp == cap, "manual loan acquire");
    CHECK(arl_owner_try_destroy(&o) == ARL_CAPTURE_ERR_BUSY && o.cap == cap, "try_destroy BUSY while loaned, cap intact");
    arl_owner_release(&o);
  }

  CHECK(arl_owner_try_destroy(&o) == ARL_CAPTURE_OK && o.cap == NULL, "owner destroy OK quiescent, cap NULL");

  /* Tras destroy: wrappers devuelven OWNER_BUSY (owner cerrado), no CLOSED, outputs a 0. */
  n = 99u;
  CHECK(arl_capture_owned_read(&o, buf, &n) == ARL_OWNED_ERR_OWNER_BUSY && n == 0u, "after destroy read => OWNED_BUSY, n=0");
  dropped = 99u;
  CHECK(arl_capture_owned_drain(&o, &dropped) == ARL_OWNED_ERR_OWNER_BUSY && dropped == 0u, "after destroy drain => OWNED_BUSY, dropped=0");
  CHECK(arl_capture_owned_push(&o, 1u, buf, 1u) == ARL_OWNED_ERR_OWNER_BUSY, "after destroy push => OWNED_BUSY, not CLOSED");

  printf("OWNED_SMOKE_CASES=%d fails=%d\n", cases, fails);
  printf(fails ? "RESULT=FAIL owned smoke\n" : "RESULT=PASS owned smoke\n");
  return fails ? 1 : 0;
}
