/* tests/owner_smoke.c - smoke del owner privado sobre el core real (no mock). */
#include <stdio.h>
#include <stdint.h>

#include "capture-abi.h"
#include "capture-owner.h"

static int fails = 0;
static int cases = 0;
#define CHECK(c, msg) do { cases++; if (!(c)) { fails++; printf("FAIL %s\n", msg); } else { printf("ok %s\n", msg); } } while (0)

int main(void) {
  openrelay_capture_owner owner;
  openrelay_capture *cap = NULL;
  openrelay_capture *got = NULL;
  int32_t rc;

  rc = openrelay_capture_open(&cap, OPENRELAY_CAPTURE_RING_SAMPLES);
  CHECK(rc == OPENRELAY_CAPTURE_OK && cap != NULL, "open capture");
  CHECK(openrelay_owner_init(&owner, cap) == OPENRELAY_CAPTURE_OK, "owner init");
  CHECK(openrelay_owner_init(&owner, NULL) == OPENRELAY_CAPTURE_ERR_NULL, "init null rejected");

  rc = openrelay_owner_acquire(&owner, &got);
  CHECK(rc == OPENRELAY_CAPTURE_OK && got == cap, "acquire copies cap");
  rc = openrelay_owner_try_destroy(&owner);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_BUSY && owner.cap == cap, "try_destroy BUSY while borrowed, cap unchanged");
  openrelay_owner_release(&owner);

  rc = openrelay_owner_try_destroy(&owner);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_STATE && owner.cap == cap, "destroy from OPEN => ERR_STATE, cap unchanged");

  rc = openrelay_owner_acquire(&owner, &got);
  CHECK(rc == OPENRELAY_CAPTURE_OK, "acquire again");
  openrelay_owner_release(&owner);

  CHECK(openrelay_capture_close(cap) == OPENRELAY_CAPTURE_OK, "close capture");
  rc = openrelay_owner_try_destroy(&owner);
  CHECK(rc == OPENRELAY_CAPTURE_OK && owner.cap == NULL, "destroy OK when quiescent, cap NULL");
  rc = openrelay_owner_acquire(&owner, &got);
  CHECK(rc == OPENRELAY_CAPTURE_ERR_BUSY && got == NULL, "acquire after destroy => BUSY, out NULL");

  printf("OWNER_SMOKE_CASES=%d fails=%d\n", cases, fails);
  printf(fails ? "RESULT=FAIL owner smoke\n" : "RESULT=PASS owner smoke\n");
  return fails ? 1 : 0;
}
