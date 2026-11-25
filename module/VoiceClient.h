// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The McBopomofo Authors.
//
// Thin Unix-socket client for the voxkit daemon. Built only when
// ENABLE_VOICE_POC is on. It does not link an ASR runtime.

#ifndef SRC_VOICECLIENT_H_
#define SRC_VOICECLIENT_H_

#include <fcitx-utils/event.h>
#include <json-c/json.h>

#include <cstdint>
#include <functional>
#include <memory>
#include <string>
#include <vector>

namespace fcitx {
class Instance;
}

namespace McBopomofo {

struct VoiceEvent {
  enum class Kind { Partial, Final, Discarded, Error, Level };
  Kind kind = Kind::Partial;
  uint64_t req = 0;
  std::string stable;
  std::string tentative;
  std::string text;
  std::string code;
  std::string message;
  double rms = 0;
  // Final of a translate request: `text` is the translation, or the
  // original transcript when `translated` is false (`translateError` says
  // why: plugin-missing, model-downloading, translate-failed, ...).
  bool translateMode = false;
  bool translated = false;
  std::string translateError;
  std::string source;
};

// Reads the user phrase files the daemon should bias toward. `data.txt`
// contributes its word column only (the Bopomofo reading is not a pronunciation
// hint). Quick phrases and the user English list contribute the whole line.
std::vector<std::string> LoadVoiceVocabulary(const std::string& userDataDir);

// Non-blocking line-JSON client. Callbacks run on the fcitx event loop thread.
class VoiceClient {
 public:
  using Callback = std::function<void(const VoiceEvent&)>;

  explicit VoiceClient(fcitx::Instance* instance);
  ~VoiceClient();

  VoiceClient(const VoiceClient&) = delete;
  VoiceClient& operator=(const VoiceClient&) = delete;

  void setCallback(Callback callback);

  // false when the daemon socket is not accepting connections.
  bool start(uint64_t req, const std::vector<std::string>& vocabulary, bool traditional);
  // `translate` sends mode "translate" with `target`. Only call it after
  // supportsTranslate(): an older daemon would ignore the fields and dictate.
  bool start(uint64_t req, const std::vector<std::string>& vocabulary, bool traditional,
             bool translate, const std::string& target);

  // Whether the daemon lists "translate" in model.status capabilities.
  // Connects if needed and asks once per connection (blocks up to
  // `timeoutMs` on the first call). false when the daemon is down or old.
  bool supportsTranslate(int timeoutMs = 400);
  bool connected() const { return fd_ >= 0; }
  void stop(uint64_t req);
  void cancel(uint64_t req);

 private:
  bool ensureConnected();
  bool probeCapabilities(int timeoutMs);
  void armIo();
  void onIo();
  void flushOut();
  void closeSocket();
  void sendJson(json_object* object);
  void handleLine(const std::string& line);

  fcitx::Instance* instance_;
  Callback callback_;
  int fd_ = -1;
  std::string inbuf_;
  std::string outbuf_;
  std::unique_ptr<fcitx::EventSourceIO> io_;
  uint64_t activeReq_ = 0;
  enum class Caps { Unknown, None, Translate };
  Caps caps_ = Caps::Unknown;
};

}  // namespace McBopomofo

#endif  // SRC_VOICECLIENT_H_
