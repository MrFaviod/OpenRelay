/* JNI fino para dev.arl.audio.AudioEngine. Sin lógica: cada función llama a
 * arl_capture_owned_* con un owner reservado en el heap. El handle Java es el
 * puntero al owner como jlong; 0 significa "no hay objeto". */
#include <jni.h>
#include <stdint.h>
#include <stdlib.h>

#include "capture-abi.h"
#include "capture-owned.h"
#include "capture-owner.h"

static arl_capture_owner *owner_of(jlong h) {
  return (arl_capture_owner *)(intptr_t)h;
}

JNIEXPORT jlong JNICALL
Java_dev_arl_audio_AudioEngine_nativeCreate(JNIEnv *env, jobject thiz) {
  (void)env;
  (void)thiz;
  arl_capture *cap = NULL;
  if (arl_capture_open(&cap, ARL_CAPTURE_RING_SAMPLES) != ARL_CAPTURE_OK) {
    return 0;
  }
  arl_capture_owner *o = malloc(sizeof *o);
  if (o == NULL) {
    arl_capture_destroy(&cap);
    return 0;
  }
  if (arl_owner_init(o, cap) != ARL_CAPTURE_OK) {
    free(o);
    arl_capture_destroy(&cap);
    return 0;
  }
  return (jlong)(intptr_t)o;
}

JNIEXPORT jint JNICALL
Java_dev_arl_audio_AudioEngine_nativeStart(JNIEnv *env, jobject thiz, jlong h) {
  (void)env;
  (void)thiz;
  return arl_capture_owned_start(owner_of(h));
}

JNIEXPORT jint JNICALL
Java_dev_arl_audio_AudioEngine_nativeStop(JNIEnv *env, jobject thiz, jlong h) {
  (void)env;
  (void)thiz;
  return arl_capture_owned_stop(owner_of(h));
}

JNIEXPORT jint JNICALL
Java_dev_arl_audio_AudioEngine_nativeClose(JNIEnv *env, jobject thiz, jlong h) {
  (void)env;
  (void)thiz;
  return arl_capture_owned_close(owner_of(h));
}

/* Devuelve muestras copiadas (>= 0) o un código ARL_CAPTURE_ERR_* (< 0).
 * dst.length es el tope; el ABI rechaza más de 240 muestras. */
JNIEXPORT jint JNICALL
Java_dev_arl_audio_AudioEngine_nativeRead(JNIEnv *env, jobject thiz, jlong h,
                                          jshortArray dst) {
  (void)thiz;
  jsize len = (*env)->GetArrayLength(env, dst);
  int16_t buf[ARL_CAPTURE_FRAME_SAMPLES];
  uint32_t n = (uint32_t)len;
  int32_t rc = arl_capture_owned_read(owner_of(h), buf, &n);
  if (rc != ARL_CAPTURE_OK) {
    return rc;
  }
  (*env)->SetShortArrayRegion(env, dst, 0, (jsize)n, buf);
  return (jint)n;
}

/* Libera el owner sólo si el núcleo destruyó el objeto (OK). BUSY o estado
 * incorrecto dejan el handle vivo para reintentar. */
JNIEXPORT jint JNICALL
Java_dev_arl_audio_AudioEngine_nativeDestroy(JNIEnv *env, jobject thiz, jlong h) {
  (void)env;
  (void)thiz;
  arl_capture_owner *o = owner_of(h);
  int32_t rc = arl_owner_try_destroy(o);
  if (rc == ARL_CAPTURE_OK) {
    free(o);
  }
  return rc;
}
