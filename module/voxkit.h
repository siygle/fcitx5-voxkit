/*
 * SPDX-FileCopyrightText: 2026 siygle
 * SPDX-License-Identifier: MIT
 */
#ifndef _FCITX5_VOXKIT_VOXKIT_H_
#define _FCITX5_VOXKIT_VOXKIT_H_

#include <chrono>
#include <cstdint>
#include <memory>
#include <string>

#include <fcitx-config/configuration.h>
#include <fcitx-config/iniparser.h>
#include <fcitx-config/option.h>
#include <fcitx-utils/event.h>
#include <fcitx-utils/handlertable.h>
#include <fcitx-utils/i18n.h>
#include <fcitx-utils/trackableobject.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/instance.h>

#include "VoiceClient.h"
#include "voxkit_public.h"

namespace fcitx {

FCITX_CONFIGURATION(
    VoxkitConfig,
    Option<bool> enabled{this, "Enabled", _("Enable push-to-talk dictation"), true};
    // Bare modifiers only: a printable key would also type a character.
    KeyListOption hotkey{this, "Hotkey", _("Push-to-talk key (hold)"),
                         {Key("Alt_R")},
                         KeyListConstrain({KeyConstrainFlag::AllowModifierOnly,
                                           KeyConstrainFlag::AllowModifierLess})};
    Option<bool> traditional{this, "Traditional",
                             _("Convert to Traditional Chinese (OpenCC s2twp)"), true};
    Option<int, IntConstrain> minHoldMs{this, "MinHoldMs",
        _("Ignore holds shorter than (ms)"), 250, IntConstrain(0, 2000)};);

class Voxkit final : public AddonInstance {
public:
    explicit Voxkit(Instance *instance);
    ~Voxkit() override;

    void reloadConfig() override;
    const Configuration *getConfig() const override { return &config_; }
    void setConfig(const RawConfig &config) override {
        config_.load(config, true);
        safeSaveAsIni(config_, "conf/voxkit.conf");
    }

    std::unique_ptr<HandlerTableEntry<VoxkitRegistration>>
    registerInputMethod(const std::string &uniqueName,
                        VoxkitRegistration registration);

    // Entry points used by unit tests (no fcitx event loop needed).
    enum class Phase { Idle, Listening, Recognizing };
    Phase phase() const { return phase_; }

private:
    bool onKey(KeyEvent &event);
    void begin(InputContext *ic);
    void end(InputContext *ic);
    void abandon();
    void onVoiceEvent(const McBopomofo::VoiceEvent &event);
    void deliver(InputContext *ic, const std::string &text);
    bool isHotkey(const Key &raw) const;
    VoxkitRegistration *registrationFor(InputContext *ic);
    bool imBusy(InputContext *ic);
    // Dictation UI lives in the aux area so it never fights the IM preedit.
    void showStatus(InputContext *ic, const std::string &text, bool transient = false);
    void clearStatus(InputContext *ic);

    FCITX_ADDON_EXPORT_FUNCTION(Voxkit, registerInputMethod);

    Instance *instance_;
    VoxkitConfig config_;
    std::unique_ptr<McBopomofo::VoiceClient> client_;
    MultiHandlerTable<std::string, VoxkitRegistration> registrations_;
    std::vector<std::unique_ptr<HandlerTableEntry<EventHandler>>> watchers_;
    std::unique_ptr<EventSourceTime> hintTimer_;

    Phase phase_ = Phase::Idle;
    TrackableObjectReference<InputContext> ic_;
    uint64_t serial_ = 0;
    uint64_t req_ = 0;
    bool statusShown_ = false;
    std::chrono::steady_clock::time_point pressedAt_;
    std::chrono::steady_clock::time_point levelAt_;
};

class VoxkitFactory : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new Voxkit(manager->instance());
    }
};

} // namespace fcitx

#endif
