/*
 * SPDX-FileCopyrightText: 2026 siygle
 * SPDX-License-Identifier: MIT
 *
 * Push-to-talk dictation for any fcitx5 input method. Recognition runs in the
 * separate voxkit daemon (see daemon/); this module only watches the hotkey,
 * talks to the daemon over a Unix socket and inserts the final transcript.
 */
#include "voxkit.h"

#include <fcitx-utils/capabilityflags.h>
#include <fcitx-utils/log.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputmethodentry.h>
#include <fcitx/inputpanel.h>
#include <fcitx/userinterface.h>

FCITX_DEFINE_LOG_CATEGORY(voxkit_log, "voxkit");
#define VOXKIT_DEBUG() FCITX_LOGC(voxkit_log, Debug)

namespace fcitx {
namespace {

bool Sensitive(InputContext *ic) {
    return ic->capabilityFlags().testAny(CapabilityFlag::PasswordOrSensitive);
}

std::string TranslateFallbackText(const std::string &code) {
    if (code == "model-downloading") {
        return _("翻譯模型下載中，已輸入原文");
    }
    if (code == "license-required") {
        return _("請先在語音模型同意翻譯模型授權，已輸入原文");
    }
    if (code == "plugin-missing") {
        return _("未安裝翻譯外掛，已輸入原文");
    }
    if (code == "bad-target") {
        return _("不支援的翻譯語言，已輸入原文");
    }
    return _("翻譯失敗，已輸入原文");
}

bool MatchesModifier(const KeyList &keys, const Key &raw) {
    for (const auto &k : keys) {
        if (k.isValid() && k.isModifier() && k.sym() == raw.sym()) {
            return true;
        }
    }
    return false;
}

std::string ErrorText(const std::string &code) {
    if (code == "model-not-ready") {
        return _("語音模型未安裝");
    }
    if (code == "no-input") {
        return _("找不到麥克風");
    }
    if (code == "disconnected") {
        return _("語音服務已斷線");
    }
    return _("語音辨識失敗");
}

} // namespace

Voxkit::Voxkit(Instance *instance)
    : instance_(instance),
      client_(std::make_unique<McBopomofo::VoiceClient>(instance)) {
    reloadConfig();
    client_->setCallback(
        [this](const McBopomofo::VoiceEvent &e) { onVoiceEvent(e); });

    // PreInputMethod sees the key before the IM, and releases too.
    watchers_.emplace_back(instance_->watchEvent(
        EventType::InputContextKeyEvent, EventWatcherPhase::PreInputMethod,
        [this](Event &event) {
            auto &keyEvent = static_cast<KeyEvent &>(event);
            if (onKey(keyEvent)) {
                keyEvent.filterAndAccept();
            }
        }));
    // Focus change or IM switch while dictating: drop the round.
    for (auto type : {EventType::InputContextFocusOut,
                      EventType::InputContextSwitchInputMethod,
                      EventType::InputContextReset}) {
        watchers_.emplace_back(instance_->watchEvent(
            type, EventWatcherPhase::PreInputMethod, [this](Event &event) {
                auto &e = static_cast<InputContextEvent &>(event);
                if (phase_ != Phase::Idle && e.inputContext() == ic_.get()) {
                    abandon();
                }
            }));
    }
}

Voxkit::~Voxkit() = default;

void Voxkit::reloadConfig() { readAsIni(config_, "conf/voxkit.conf"); }

std::unique_ptr<HandlerTableEntry<VoxkitRegistration>>
Voxkit::registerInputMethod(const std::string &uniqueName,
                            VoxkitRegistration registration) {
    return registrations_.add(uniqueName, std::move(registration));
}

VoxkitRegistration *Voxkit::registrationFor(InputContext *ic) {
    const auto *entry = instance_->inputMethodEntry(ic);
    if (!entry) {
        return nullptr;
    }
    for (auto &r : registrations_.view(entry->uniqueName())) {
        return &r;  // newest wins is fine; one IM registers once
    }
    return nullptr;
}

bool Voxkit::imBusy(InputContext *ic) {
    if (auto *r = registrationFor(ic); r && r->busy) {
        return r->busy(ic);
    }
    const auto &panel = ic->inputPanel();
    return !panel.preedit().empty() || !panel.clientPreedit().empty() ||
           panel.candidateList();
}

bool Voxkit::isHotkey(const Key &raw) const {
    return MatchesModifier(*config_.hotkey, raw);
}

bool Voxkit::isTranslateHotkey(const Key &raw) const {
    // The dictation key wins if both are set to the same key.
    return *config_.translateEnabled && !isHotkey(raw) &&
           MatchesModifier(*config_.translateHotkey, raw);
}

bool Voxkit::onKey(KeyEvent &event) {
    if (!*config_.enabled) {
        return false;
    }
    auto *ic = event.inputContext();
    const bool translateKey = isTranslateHotkey(event.rawKey());
    // While a round is running only its own key ends it.
    const bool hotkey = phase_ == Phase::Listening
                            ? (translating_ ? translateKey : isHotkey(event.rawKey()))
                            : (translateKey || isHotkey(event.rawKey()));
    if (!hotkey) {
        // Typing while holding the key cancels: avoids racing the IM.
        if (!event.isRelease() && phase_ == Phase::Listening &&
            ic == ic_.get()) {
            abandon();
        }
        return false;
    }
    if (event.isRelease()) {
        if (phase_ != Phase::Listening || ic != ic_.get()) {
            return false;
        }
        end(ic);
        return true;
    }
    if (phase_ == Phase::Listening && ic == ic_.get()) {
        return true;  // key auto-repeat
    }
    if (phase_ != Phase::Idle || Sensitive(ic)) {
        return false;
    }
    begin(ic, translateKey);
    return phase_ == Phase::Listening;
}

void Voxkit::begin(InputContext *ic, bool translate) {
    if (translate && !client_->supportsTranslate()) {
        // Never send mode=translate to a daemon that does not advertise it:
        // it would ignore the field and type the Chinese transcript.
        showStatus(ic, client_->connected()
                           ? _("語音服務版本太舊，不支援翻譯")
                           : _("語音服務未啟動（voxkit）"),
                   true);
        return;
    }
    std::vector<std::string> vocabulary;
    if (auto *r = registrationFor(ic); r && r->vocabulary) {
        vocabulary = r->vocabulary();
    }
    req_ = ++serial_;
    ic_ = ic->watch();
    pressedAt_ = std::chrono::steady_clock::now();
    translating_ = translate;
    const bool started =
        translate ? client_->start(req_, vocabulary, *config_.traditional, true,
                                   *config_.translateTarget)
                  : client_->start(req_, vocabulary, *config_.traditional);
    if (!started) {
        req_ = 0;
        translating_ = false;
        showStatus(ic, _("語音服務未啟動（voxkit）"), true);
        return;
    }
    VOXKIT_DEBUG() << "start req=" << req_ << " mode="
                   << (translate ? "translate target=" + *config_.translateTarget
                                 : std::string("dictate"));
    phase_ = Phase::Listening;
    showStatus(ic, translate ? _("🌐 翻譯聆聽中…") : _("🎙 聆聽中…"));
}

void Voxkit::end(InputContext *ic) {
    auto held = std::chrono::steady_clock::now() - pressedAt_;
    if (held < std::chrono::milliseconds(*config_.minHoldMs)) {
        abandon();  // a tap on Right Alt, not dictation
        return;
    }
    client_->stop(req_);
    phase_ = Phase::Recognizing;
    showStatus(ic, translating_ ? _("⋯ 辨識並翻譯中") : _("⋯ 辨識中"));
}

void Voxkit::abandon() {
    if (req_ != 0) {
        client_->cancel(req_);
    }
    req_ = 0;
    phase_ = Phase::Idle;
    if (auto *ic = ic_.get()) {
        clearStatus(ic);
    }
}

void Voxkit::deliver(InputContext *ic, const std::string &text) {
    if (text.empty() || Sensitive(ic)) {
        return;
    }
    if (auto *r = registrationFor(ic); r && r->insert && r->insert(ic, text)) {
        return;
    }
    if (imBusy(ic)) {
        // Committing now would land before the IM's composition.
        showStatus(ic, _("組字中，語音結果未輸入"), true);
        return;
    }
    ic->commitString(text);
}

void Voxkit::onVoiceEvent(const McBopomofo::VoiceEvent &e) {
    if (e.req == 0 || e.req != req_) {
        return;
    }
    auto *ic = ic_.get();
    if (!ic) {
        abandon();
        return;
    }
    using Kind = McBopomofo::VoiceEvent::Kind;
    switch (e.kind) {
    case Kind::Final:
        req_ = 0;
        phase_ = Phase::Idle;
        clearStatus(ic);
        VOXKIT_DEBUG() << "final req=" << e.req << " translate=" << e.translateMode
                       << " translated=" << e.translated
                       << " error=" << e.translateError;
        // Translated or not, the text goes through the same insert path as
        // dictation. Nothing user-specific is applied to it here.
        deliver(ic, e.text);
        if (e.translateMode && !e.translated) {
            showStatus(ic, TranslateFallbackText(e.translateError), true);
        }
        break;
    case Kind::Discarded:
        req_ = 0;
        phase_ = Phase::Idle;
        showStatus(ic, _("（沒有聽到語音）"), true);
        break;
    case Kind::Error:
        req_ = 0;
        phase_ = Phase::Idle;
        showStatus(ic, ErrorText(e.code), true);
        break;
    case Kind::Level: {
        if (phase_ != Phase::Listening) {
            break;
        }
        auto now = std::chrono::steady_clock::now();
        if (now - levelAt_ < std::chrono::milliseconds(150)) {
            break;
        }
        levelAt_ = now;
        int filled = e.rms > -12 ? 5 : e.rms > -20 ? 4 : e.rms > -28 ? 3
                   : e.rms > -35 ? 2 : e.rms > -45 ? 1 : 0;
        std::string meter = translating_ ? _("🌐 翻譯聆聽中 ") : _("🎙 聆聽中 ");
        for (int i = 0; i < 5; ++i) {
            meter += i < filled ? "●" : "○";
        }
        showStatus(ic, meter);
        break;
    }
    case Kind::Partial:
        break;  // the daemon only sends status partials today
    }
}

void Voxkit::showStatus(InputContext *ic, const std::string &text,
                        bool transient) {
    ic->inputPanel().setAuxUp(Text(text));
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    statusShown_ = true;
    hintTimer_.reset();
    if (!transient) {
        return;
    }
    auto ref = ic->watch();
    hintTimer_ = instance_->eventLoop().addTimeEvent(
        CLOCK_MONOTONIC, now(CLOCK_MONOTONIC) + 1800 * 1000, 0,
        [this, ref](EventSourceTime *, uint64_t) {
            if (auto *ic = ref.get(); ic && phase_ == Phase::Idle) {
                clearStatus(ic);
            }
            return true;
        });
}

void Voxkit::clearStatus(InputContext *ic) {
    if (!statusShown_) {
        return;
    }
    ic->inputPanel().setAuxUp(Text());
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
    statusShown_ = false;
}

} // namespace fcitx

FCITX_ADDON_FACTORY_V2_BACKWARDS(voxkit, fcitx::VoxkitFactory);
