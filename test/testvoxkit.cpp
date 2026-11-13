/*
 * SPDX-FileCopyrightText: 2026 siygle
 * SPDX-License-Identifier: MIT
 *
 * Module test against test/fake_voxkit.py (real protocol, no microphone):
 * hold/release Right Alt through fcitx5's test frontend, check what gets
 * committed or handed to a registered input method. Driven by a timer so the
 * event loop services the daemon socket between steps.
 */
#include <cstdio>
#include <cstdlib>
#include <fcitx-utils/event.h>
#include <fcitx-utils/eventdispatcher.h>
#include <fcitx-utils/key.h>
#include <fcitx-utils/log.h>
#include <fcitx-utils/testing.h>
#include <fcitx/addonmanager.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputcontextmanager.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <fstream>
#include <memory>
#include <sstream>
#include <string>

#include "testdir.h"
#include "testfrontend_public.h"
#include "voxkit_public.h"

using namespace fcitx;

namespace {

int failures = 0;
void check(bool ok, const std::string &what) {
    std::printf("%s %s\n", ok ? "PASS" : "FAIL", what.c_str());
    if (!ok) {
        ++failures;
    }
}

std::string readFile(const char *path) {
    std::ifstream in(path ? path : "");
    std::stringstream s;
    s << in.rdbuf();
    return s.str();
}

struct Harness {
    Instance *instance = nullptr;
    AddonInstance *voxkit = nullptr;
    AddonInstance *tf = nullptr;
    ICUUID uuid{};
    InputContext *ic = nullptr;
    std::string committed;
    std::string handedToIm;
    std::unique_ptr<HandlerTableEntry<EventHandler>> commitWatch;
    std::unique_ptr<HandlerTableEntry<VoxkitRegistration>> registration;
    std::unique_ptr<EventSourceTime> timer;
    int step = 0;

    void key(const Key &k, bool release) {
        tf->call<ITestFrontend::keyEvent>(uuid, k, release);
    }
    std::string aux() const { return ic->inputPanel().auxUp().toString(); }
};

const Key kAltR(FcitxKey_Alt_R);
// fcitx reports the modifier state on the modifier's own release.
const Key kAltRUp(FcitxKey_Alt_R, KeyState::Alt);

// One tick = 100 ms.
bool tick(Harness &h) {
    const std::string expected = std::getenv("FAKE_TEXT") ? std::getenv("FAKE_TEXT") : "";
    switch (h.step++) {
    case 0:
        h.key(kAltR, false);
        check(h.aux().find("聆聽") != std::string::npos,
              "hold Right Alt: listening status, got [" + h.aux() + "]");
        break;
    case 4:  // ~400 ms hold
        h.key(kAltRUp, true);
        check(h.aux().find("辨識") != std::string::npos,
              "release: recognizing status, got [" + h.aux() + "]");
        break;
    case 8:
        check(h.committed == expected,
              "transcript committed on idle IM, got [" + h.committed + "]");
        check(h.aux().empty(), "status cleared after final");
        h.committed.clear();
        h.key(kAltR, false);  // short tap
        h.key(kAltRUp, true);
        break;
    case 11:
        check(h.committed.empty(), "short tap does not dictate");
        h.ic->setCapabilityFlags(CapabilityFlag::Password);
        h.key(kAltR, false);
        check(h.aux().empty(), "password field: hotkey ignored");
        h.key(kAltRUp, true);
        h.ic->setCapabilityFlags(CapabilityFlags{});
        break;
    case 13:
        // A registered IM receives the transcript instead of a commit.
        h.registration = h.voxkit->call<IVoxkit::registerInputMethod>(
            h.instance->inputMethod(h.ic),
            VoxkitRegistration{
                [&h](InputContext *, const std::string &t) {
                    h.handedToIm = t;
                    return true;
                },
                []() { return std::vector<std::string>{"新酷音"}; },
                nullptr});
        h.key(kAltR, false);
        break;
    case 17:
        h.key(kAltRUp, true);
        break;
    case 21:
        check(h.handedToIm == expected,
              "registered IM gets the text, got [" + h.handedToIm + "]");
        check(h.committed.empty(), "module does not commit for registered IM");
        check(readFile(std::getenv("FAKE_LOG")).find("\"新酷音\"") !=
                  std::string::npos,
              "IM vocabulary sent to the daemon");
        // Unregistering falls back to committing.
        h.registration.reset();
        h.key(kAltR, false);
        break;
    case 25:
        h.key(kAltRUp, true);
        break;
    case 29:
        check(h.committed == expected, "after unregister: commits again");
        h.timer->setEnabled(false);
        std::printf(failures ? "VOXKIT TESTS FAILED (%d)\n" : "VOXKIT TESTS OK\n",
                    failures);
        h.instance->exit();
        break;
    default:
        break;
    }
    return true;
}

} // namespace

int main() {
    setupTestingEnvironment(TESTING_BINARY_DIR, {TESTING_BINARY_DIR "/module"},
                            {TESTING_BINARY_DIR "/test"});
    char arg0[] = "testvoxkit";
    char arg1[] = "--disable=all";
    char arg2[] = "--enable=testim,testfrontend,voxkit";
    char *argv[] = {arg0, arg1, arg2};
    Log::setLogRule("default=3");
    Instance instance(3, argv);
    instance.addonManager().registerDefaultLoader(nullptr);

    Harness h;
    h.instance = &instance;
    instance.eventDispatcher().schedule([&h]() {
        h.voxkit = h.instance->addonManager().addon("voxkit", true);
        check(h.voxkit != nullptr, "voxkit module loads");
        h.tf = h.instance->addonManager().addon("testfrontend");
        h.uuid = h.tf->call<ITestFrontend::createInputContext>("testapp");
        h.ic = h.instance->inputContextManager().findByUUID(h.uuid);
        h.ic->focusIn();
        h.commitWatch = h.instance->watchEvent(
            EventType::InputContextCommitString,
            EventWatcherPhase::PreInputMethod, [&h](Event &e) {
                auto &ce = static_cast<CommitStringEvent &>(e);
                if (ce.inputContext() == h.ic) {
                    h.committed += ce.text();
                }
            });
        h.timer = h.instance->eventLoop().addTimeEvent(
            CLOCK_MONOTONIC, now(CLOCK_MONOTONIC) + 100000, 0,
            [&h](EventSourceTime *src, uint64_t) {
                src->setNextInterval(100000);
                src->setOneShot();
                return tick(h);
            });
    });
    instance.exec();
    return failures ? 1 : 0;
}
