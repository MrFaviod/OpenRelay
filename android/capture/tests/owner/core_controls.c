/* Controles del core real base (inmutable), sin owner.
 * Modos: quiescent | retire_reopen. Exit 0 PASS, 1 FAIL, 2 SETUP_FAIL. */
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <string.h>

#include "capture-abi.h"
#include "capture-backend.h"
#include "owner_setup.h"

int main(int argc, char **argv) {
  openrelay_capture *cap = NULL;
  openrelay_capture *before;
  openrelay_capture_counters ct;
  int32_t rc;
  int fails = 0;

  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 2) {
    printf("SETUP_FAIL usage: core_controls <quiescent|retire_reopen>\n");
    return 2;
  }
  printf("MODE=%s\n", argv[1]);
  if (setup_closed(&cap) != 0) return 2;
  before = cap;

  if (strcmp(argv[1], "quiescent") == 0) {
    rc = openrelay_capture_destroy(&cap);
    printf("CORE_DESTROY rc=%d owner_after_null=%d\n", rc, cap == NULL);
    if (rc == OPENRELAY_CAPTURE_OK && cap == NULL) {
      printf("RESULT=PASS core quiescent destroy\n");
      return 0;
    }
    printf("RESULT=FAIL core quiescent destroy\n");
    return 1;
  }

  if (strcmp(argv[1], "retire_reopen") == 0) {
    /* Obligacion viva por token: destroy debe dar BUSY y dejar el objeto. */
    openrelay_fx_set_retire(openrelay_fx_enable_token(), OPENRELAY_BE_RETIRE_DATA);
    rc = openrelay_capture_destroy(&cap);
    printf("CORE_DESTROY_HELD rc=%d owner_same=%d\n", rc, cap == before);
    if (rc != OPENRELAY_CAPTURE_ERR_BUSY || cap != before) fails++;
    /* Core reabre admision tras el fallo: consultas siguen validas. */
    rc = openrelay_capture_get_counters(before, &ct);
    printf("REOPEN_GET_COUNTERS rc=%d\n", rc);
    if (rc != OPENRELAY_CAPTURE_OK) fails++;
    openrelay_fx_ack_retire(openrelay_fx_enable_token());
    rc = openrelay_capture_destroy(&cap);
    printf("CORE_DESTROY_ACK rc=%d owner_after_null=%d\n", rc, cap == NULL);
    if (rc != OPENRELAY_CAPTURE_OK || cap != NULL) fails++;
    printf(fails ? "RESULT=FAIL core retire reopen\n"
                 : "RESULT=PASS core retire reopen\n");
    return fails ? 1 : 0;
  }

  printf("SETUP_FAIL unknown mode\n");
  return 2;
}
