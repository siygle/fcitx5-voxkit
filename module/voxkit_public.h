/*
 * SPDX-FileCopyrightText: 2026 siygle
 * SPDX-License-Identifier: MIT
 *
 * Public API of the fcitx5 "voxkit" module (push-to-talk dictation).
 *
 * Input methods need nothing to get basic dictation: while their composition
 * is empty the module commits the transcript itself. To insert dictation
 * into a non-empty composition (or to add vocabulary), an input method
 * registers an InsertHandler for its InputMethodEntry unique name:
 *
 *   #include <voxkit_public.h>
 *   FCITX_ADDON_DEPENDENCY_LOADER(voxkit, instance_->addonManager());
 *   ...
 *   handle_ = voxkit()->call<fcitx::IVoxkit::registerInputMethod>(
 *       "chewing",
 *       [this](fcitx::InputContext *ic, const std::string &text) {
 *           return insertDictation(ic, text);   // true = handled
 *       },
 *       [this]() { return userVocabulary(); });
 *
 * Keep the returned handle; destroying it unregisters. Add "voxkit" to the
 * addon's [Addon/OptionalDependencies] so it works without the module too.
 */
#ifndef _FCITX5_VOXKIT_PUBLIC_H_
#define _FCITX5_VOXKIT_PUBLIC_H_

#include <fcitx-utils/handlertable.h>
#include <fcitx/addoninstance.h>
#include <fcitx/inputcontext.h>
#include <functional>
#include <memory>
#include <string>
#include <vector>

namespace fcitx {

// Called on the final transcript. Return true if the text was inserted or
// committed; false makes the module fall back (commit when the IM is idle,
// otherwise show a short notice and drop it).
using VoxkitInsertHandler =
    std::function<bool(InputContext *ic, const std::string &text)>;
// Extra terms biasing recognition (names, jargon). Called at speech start.
using VoxkitVocabularyProvider = std::function<std::vector<std::string>()>;
// Whether the IM is busy composing (dictation must not be committed in
// front of a half-typed composition). Optional; default: preedit non-empty.
using VoxkitBusyCheck = std::function<bool(InputContext *ic)>;

struct VoxkitRegistration {
    VoxkitInsertHandler insert;
    VoxkitVocabularyProvider vocabulary;
    VoxkitBusyCheck busy;
};

} // namespace fcitx

FCITX_ADDON_DECLARE_FUNCTION(
    Voxkit, registerInputMethod,
    std::unique_ptr<fcitx::HandlerTableEntry<fcitx::VoxkitRegistration>>(
        const std::string &uniqueName, fcitx::VoxkitRegistration registration));

#endif
