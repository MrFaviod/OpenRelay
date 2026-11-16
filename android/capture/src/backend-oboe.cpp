/* arl capture - adapter Oboe del backend privado (T13a).
 * Implementa arl_be_ops sobre oboe::AudioStream de entrada y la factory
 * arl_capture_backend_create de capture-backend.h. Un solo stream por objeto.
 * El callback de datos empuja con arl_capture_push; el hilo de error llama a
 * arl_capture_note_disconnect / note_error con el token de la generacion.
 * Retiro: contadores atomicos de callbacks dentro (DATA/ERROR); mientras el
 * stream no este cerrado se reporta FW|MAY_REENTER. */
#include <atomic>
#include <cstdint>
#include <memory>
#include <new>

#include <oboe/Oboe.h>

#include "capture-backend.h"

namespace {

struct arl_oboe;

class OboeCb final : public oboe::AudioStreamDataCallback,
                     public oboe::AudioStreamErrorCallback {
 public:
  explicit OboeCb(arl_oboe *self) : self_(self) {}
  oboe::DataCallbackResult onAudioReady(oboe::AudioStream *stream,
                                        void *data, int32_t frames) override;
  void onErrorAfterClose(oboe::AudioStream *stream,
                         oboe::Result error) override;

 private:
  arl_oboe *self_;
};

/* arl_be primero: el motor solo lee ops sobre este puntero. */
struct arl_oboe {
  arl_be be;
  arl_capture *owner = nullptr;
  std::shared_ptr<oboe::AudioStream> stream; /* null hasta prepare */
  std::shared_ptr<OboeCb> cb;
  std::atomic<uint64_t> gen{0};
  std::atomic<uint32_t> data_inside{0};
  std::atomic<uint32_t> err_inside{0};
  bool closed = true; /* sin stream abierto */
  bool has_epoch = false;
  uint64_t epoch = 0;
};

arl_oboe *as_oboe(arl_be *be) { return reinterpret_cast<arl_oboe *>(be); }

oboe::DataCallbackResult OboeCb::onAudioReady(oboe::AudioStream *stream, void *data,
                                              int32_t frames) {
  arl_oboe *o = self_;
  o->data_inside.fetch_add(1u);
  /* n del contrato son muestras: frames por canales (CHANNELS=1 hoy). */
  uint32_t n = static_cast<uint32_t>(frames) *
               static_cast<uint32_t>(stream->getChannelCount());
  arl_capture_push(o->owner, o->gen.load(),
                   static_cast<const int16_t *>(data), n);
  o->data_inside.fetch_sub(1u);
  return oboe::DataCallbackResult::Continue;
}

void OboeCb::onErrorAfterClose(oboe::AudioStream *, oboe::Result error) {
  arl_oboe *o = self_;
  uint64_t g = o->gen.load();
  o->err_inside.fetch_add(1u);
  if (error == oboe::Result::ErrorDisconnected) {
    arl_capture_note_disconnect(o->owner, g);
  } else {
    arl_capture_note_error(o->owner, g, static_cast<int32_t>(error));
  }
  o->err_inside.fetch_sub(1u);
}

/* Idempotente. Conserva el objeto de stream para poder releerlo. */
void close_stream(arl_oboe *o) {
  if (o->stream && !o->closed) {
    o->stream->close();
  }
  o->closed = true;
}

/* Relee el formato en vigor del stream abierto. frame_samples no es dato de
 * Oboe: es la constante del cable. */
void fill_format(const oboe::AudioStream *s, arl_capture_format *out) {
  out->sample_rate_hz = static_cast<uint32_t>(s->getSampleRate());
  out->channels = static_cast<uint16_t>(s->getChannelCount());
  out->bytes_per_sample = static_cast<uint16_t>(s->getBytesPerSample());
  out->frame_samples = ARL_CAPTURE_FRAME_SAMPLES;
  out->reserved0 = 0u;
}

int32_t op_prepare(arl_be *be, const arl_capture_format *requested,
                   arl_capture_format *effective) {
  arl_oboe *o = as_oboe(be);
  if (requested == nullptr || effective == nullptr) return ARL_CAPTURE_ERR_NULL;
  close_stream(o); /* prepare reabre desde cero */

  oboe::AudioStreamBuilder b;
  b.setDirection(oboe::Direction::Input)
      ->setSharingMode(oboe::SharingMode::Shared)
      ->setPerformanceMode(oboe::PerformanceMode::LowLatency)
      ->setFormat(oboe::AudioFormat::I16)
      ->setChannelCount(static_cast<int32_t>(requested->channels))
      ->setSampleRate(static_cast<int32_t>(requested->sample_rate_hz))
      ->setDataCallback(o->cb)
      ->setErrorCallback(o->cb);

  std::shared_ptr<oboe::AudioStream> s;
  if (b.openStream(s) != oboe::Result::OK || !s) return ARL_CAPTURE_ERR_STATE;
  o->stream = s;
  o->closed = false;
  fill_format(s.get(), effective);
  return ARL_CAPTURE_OK;
}

int32_t op_effective(arl_be *be, arl_capture_format *out) {
  arl_oboe *o = as_oboe(be);
  if (out == nullptr) return ARL_CAPTURE_ERR_NO_OUTPUT;
  if (!o->stream || o->closed) return ARL_CAPTURE_ERR_STATE;
  fill_format(o->stream.get(), out);
  return ARL_CAPTURE_OK;
}

int32_t op_enable(arl_be *be, uint64_t generation) {
  arl_oboe *o = as_oboe(be);
  if (!o->stream || o->closed) return ARL_CAPTURE_ERR_STATE;
  /* Un token nuevo no pisa callbacks del anterior todavia dentro. */
  if (o->has_epoch && generation != o->epoch &&
      (o->data_inside.load() != 0u || o->err_inside.load() != 0u)) {
    return ARL_CAPTURE_ERR_BUSY;
  }
  o->gen.store(generation);
  o->epoch = generation;
  o->has_epoch = true;
  if (o->stream->requestStart() != oboe::Result::OK) return ARL_CAPTURE_ERR_STATE;
  return ARL_CAPTURE_OK;
}

int32_t op_disable(arl_be *be) {
  arl_oboe *o = as_oboe(be);
  if (!o->stream || o->closed) return ARL_CAPTURE_OK;
  if (o->stream->requestStop() != oboe::Result::OK) return ARL_CAPTURE_ERR_STATE;
  return ARL_CAPTURE_OK;
}

int32_t op_close(arl_be *be) {
  close_stream(as_oboe(be));
  return ARL_CAPTURE_OK;
}

int32_t op_retire(arl_be *be, uint64_t generation, uint32_t *flags) {
  arl_oboe *o = as_oboe(be);
  if (flags == nullptr) return ARL_CAPTURE_ERR_NO_OUTPUT;
  uint32_t data = o->data_inside.load();
  uint32_t err = o->err_inside.load();
  if (!o->has_epoch) {
    /* Nunca habilitada: solo acredita retiro si ya cerrada y sin callbacks. */
    if (o->closed && data == 0u && err == 0u) {
      *flags = 0u;
      return ARL_CAPTURE_OK;
    }
    *flags = ARL_BE_RETIRE_ALL;
    return ARL_CAPTURE_ERR_ARGS;
  }
  if (generation != o->epoch) {
    *flags = ARL_BE_RETIRE_ALL;
    return ARL_CAPTURE_ERR_ARGS;
  }
  uint32_t f = 0u;
  if (data != 0u) f |= ARL_BE_RETIRE_DATA;
  if (err != 0u) f |= ARL_BE_RETIRE_ERROR;
  if (!o->closed) f |= ARL_BE_RETIRE_FW | ARL_BE_RETIRE_MAY_REENTER;
  *flags = f;
  return ARL_CAPTURE_OK;
}

void op_release(arl_be *be) {
  arl_oboe *o = as_oboe(be);
  close_stream(o);
  o->stream.reset();
  delete o;
}

const arl_be_ops g_ops = {op_prepare, op_effective, op_enable, op_disable,
                          op_close,   op_retire,    op_release};

}  // namespace

int32_t arl_capture_backend_create(arl_be **out, arl_capture *owner) {
  if (out == nullptr) return ARL_CAPTURE_ERR_NULL;
  *out = nullptr;
  if (owner == nullptr) return ARL_CAPTURE_ERR_NULL;
  arl_oboe *o = new (std::nothrow) arl_oboe();
  if (o == nullptr) return ARL_CAPTURE_ERR_STATE;
  o->be.ops = &g_ops;
  o->owner = owner;
  o->cb.reset(new (std::nothrow) OboeCb(o));
  if (!o->cb) {
    delete o;
    return ARL_CAPTURE_ERR_STATE;
  }
  *out = &o->be;
  return ARL_CAPTURE_OK;
}
