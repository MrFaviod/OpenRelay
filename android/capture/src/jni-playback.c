/* JNI fino para dev.openrelay.app.PlaybackEngine. Sin lógica: cada función delega
 * en openrelay_playback_* (playback-oboe.cpp) y traduce el array Java. El handle
 * Java es el puntero opaco como jlong; 0 significa "no hay objeto". */
#include <jni.h>
#include <stdint.h>

#include "capture-abi.h"

/* C ABI privada de playback-oboe.cpp; este archivo es su único consumidor. */
typedef struct openrelay_playback openrelay_playback;

int32_t openrelay_playback_create(openrelay_playback **out);
int32_t openrelay_playback_start(openrelay_playback *p);
int32_t openrelay_playback_write(openrelay_playback *p, const int16_t *samples, uint32_t n,
                           uint32_t *written);
int32_t openrelay_playback_stop(openrelay_playback *p);
int32_t openrelay_playback_destroy(openrelay_playback **owner);

static openrelay_playback *playback_of(jlong h) {
  return (openrelay_playback *)(intptr_t)h;
}

JNIEXPORT jlong JNICALL
Java_dev_openrelay_app_PlaybackEngine_nativeCreate(JNIEnv *env, jobject thiz) {
  (void)env;
  (void)thiz;
  openrelay_playback *p = NULL;
  if (openrelay_playback_create(&p) != OPENRELAY_CAPTURE_OK) {
    return 0;
  }
  return (jlong)(intptr_t)p;
}

JNIEXPORT jint JNICALL
Java_dev_openrelay_app_PlaybackEngine_nativeStart(JNIEnv *env, jobject thiz,
                                              jlong h) {
  (void)env;
  (void)thiz;
  return openrelay_playback_start(playback_of(h));
}

/* Devuelve muestras aceptadas (>= 0) o un código OPENRELAY_CAPTURE_ERR_* (< 0). */
JNIEXPORT jint JNICALL
Java_dev_openrelay_app_PlaybackEngine_nativeWrite(JNIEnv *env, jobject thiz,
                                              jlong h, jshortArray src) {
  (void)thiz;
  if (src == NULL) return OPENRELAY_CAPTURE_ERR_NULL;
  jsize len = (*env)->GetArrayLength(env, src);
  jshort *samples = (*env)->GetShortArrayElements(env, src, NULL);
  if (samples == NULL) return OPENRELAY_CAPTURE_ERR_STATE; /* fallo de la VM */
  uint32_t written = 0u;
  int32_t rc = openrelay_playback_write(playback_of(h), samples, (uint32_t)len,
                                  &written);
  (*env)->ReleaseShortArrayElements(env, src, samples, JNI_ABORT);
  if (rc != OPENRELAY_CAPTURE_OK) return rc;
  return (jint)written;
}

JNIEXPORT jint JNICALL
Java_dev_openrelay_app_PlaybackEngine_nativeStop(JNIEnv *env, jobject thiz,
                                             jlong h) {
  (void)env;
  (void)thiz;
  return openrelay_playback_stop(playback_of(h));
}

JNIEXPORT jint JNICALL
Java_dev_openrelay_app_PlaybackEngine_nativeDestroy(JNIEnv *env, jobject thiz,
                                                jlong h) {
  (void)env;
  (void)thiz;
  openrelay_playback *p = playback_of(h);
  return openrelay_playback_destroy(&p);
}
