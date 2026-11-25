/*
 * SPDX-FileCopyrightText: 2026 siygle
 * SPDX-License-Identifier: MIT
 *
 * Optional translation plugin for the voxkit daemon: llama.cpp with its own
 * statically linked ggml, behind the small C ABI in voxkit_mt.h.
 */
#include "voxkit_mt.h"

#include <algorithm>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <vector>

#include "ggml.h"
#include "llama.h"

#if defined(__GNUC__)
#pragma GCC diagnostic ignored "-Wdeprecated-declarations"
#endif

#define VK_EXPORT extern "C" __attribute__((visibility("default")))

#ifndef VOXKIT_MT_LLAMA_TAG
#define VOXKIT_MT_LLAMA_TAG "unknown"
#endif

namespace {

struct Handle {
    llama_model *model = nullptr;
    llama_context *ctx = nullptr;
    int n_ctx = 0;
};

void set_err(char *err, size_t cap, const std::string &text) {
    if (err && cap > 0) {
        std::snprintf(err, cap, "%s", text.c_str());
    }
}

void quiet_log(enum ggml_log_level level, const char *text, void *) {
    static const bool verbose = [] {
        const char *v = std::getenv("VOXKIT_MT_DEBUG");
        return v && v[0] && v[0] != '0';
    }();
    if (verbose || level == GGML_LOG_LEVEL_ERROR) {
        std::fputs(text, stderr);
    }
}

std::once_flag backend_once;

bool abort_cb(void *data) {
    auto *cancel = static_cast<const volatile int *>(data);
    return cancel && *cancel != 0;
}

// Fallback when the GGUF has no usable template (HY-MT's own format, as in
// the model card's Ollama Modelfile).
std::string hunyuan_prompt(const std::string &user) {
    return "<\xEF\xBD\x9Chy_begin\xE2\x96\x81of\xE2\x96\x81sentence\xEF\xBD\x9C>"
           "<\xEF\xBD\x9Chy_User\xEF\xBD\x9C>" +
           user + "<\xEF\xBD\x9Chy_Assistant\xEF\xBD\x9C>";
}

std::string format_chat(const llama_model *model, const std::string &user) {
    const char *tmpl = llama_model_chat_template(model, nullptr);
    llama_chat_message msg{"user", user.c_str()};
    std::vector<char> buf(user.size() * 2 + 512);
    int n = llama_chat_apply_template(tmpl, &msg, 1, true, buf.data(),
                                      static_cast<int32_t>(buf.size()));
    if (n > static_cast<int>(buf.size())) {
        buf.resize(static_cast<size_t>(n) + 1);
        n = llama_chat_apply_template(tmpl, &msg, 1, true, buf.data(),
                                      static_cast<int32_t>(buf.size()));
    }
    if (n < 0) {
        return hunyuan_prompt(user);
    }
    return std::string(buf.data(), static_cast<size_t>(n));
}

} // namespace

VK_EXPORT int vk_mt_abi(void) { return VOXKIT_MT_ABI; }

VK_EXPORT const char *vk_mt_info(void) {
    static std::string info =
        std::string("llama.cpp ") + VOXKIT_MT_LLAMA_TAG + " ggml " + ggml_version();
    return info.c_str();
}

VK_EXPORT void *vk_mt_load(const char *path, int n_threads, char *err,
                           size_t err_cap) {
    std::call_once(backend_once, [] {
        llama_log_set(quiet_log, nullptr);
        llama_backend_init();
    });
    if (!path || !path[0]) {
        set_err(err, err_cap, "no model path");
        return nullptr;
    }
    llama_model_params mp = llama_model_default_params();
    mp.n_gpu_layers = 0;
    llama_model *model = llama_model_load_from_file(path, mp);
    if (!model) {
        set_err(err, err_cap, std::string("cannot load ") + path);
        return nullptr;
    }
    llama_context_params cp = llama_context_default_params();
    cp.n_ctx = 2048;
    cp.n_batch = 2048;
    cp.n_ubatch = 512;
    int threads = n_threads > 0 ? n_threads : 4;
    cp.n_threads = threads;
    cp.n_threads_batch = threads;
    cp.no_perf = true;
    llama_context *ctx = llama_init_from_model(model, cp);
    if (!ctx) {
        llama_model_free(model);
        set_err(err, err_cap, "cannot create llama context");
        return nullptr;
    }
    auto *h = new Handle;
    h->model = model;
    h->ctx = ctx;
    h->n_ctx = static_cast<int>(cp.n_ctx);
    return h;
}

VK_EXPORT int vk_mt_generate(void *handle, const char *user, int max_tokens,
                             const volatile int *cancel, char *out,
                             size_t out_cap, char *err, size_t err_cap) {
    auto *h = static_cast<Handle *>(handle);
    if (!h || !user || !out || out_cap == 0) {
        set_err(err, err_cap, "bad arguments");
        return VOXKIT_MT_ERR_FAILED;
    }
    out[0] = '\0';
    const llama_vocab *vocab = llama_model_get_vocab(h->model);
    std::string prompt = format_chat(h->model, user);

    int n = -llama_tokenize(vocab, prompt.c_str(), static_cast<int32_t>(prompt.size()),
                            nullptr, 0, false, true);
    if (n <= 0) {
        set_err(err, err_cap, "tokenize failed");
        return VOXKIT_MT_ERR_FAILED;
    }
    std::vector<llama_token> tokens(static_cast<size_t>(n) + 1);
    n = llama_tokenize(vocab, prompt.c_str(), static_cast<int32_t>(prompt.size()),
                       tokens.data(), static_cast<int32_t>(tokens.size()), false, true);
    if (n < 0) {
        set_err(err, err_cap, "tokenize failed");
        return VOXKIT_MT_ERR_FAILED;
    }
    tokens.resize(static_cast<size_t>(n));
    if (llama_vocab_get_add_bos(vocab) &&
        (tokens.empty() || tokens.front() != llama_vocab_bos(vocab))) {
        tokens.insert(tokens.begin(), llama_vocab_bos(vocab));
    }
    if (max_tokens <= 0) {
        max_tokens = 512;
    }
    if (static_cast<int>(tokens.size()) + max_tokens > h->n_ctx) {
        max_tokens = h->n_ctx - static_cast<int>(tokens.size());
        if (max_tokens < 16) {
            set_err(err, err_cap, "input too long");
            return VOXKIT_MT_ERR_TOO_LONG;
        }
    }

    llama_memory_clear(llama_get_memory(h->ctx), true);
    llama_set_abort_callback(h->ctx, abort_cb, const_cast<int *>(cancel));

    llama_sampler *smpl = llama_sampler_chain_init(llama_sampler_chain_default_params());
    llama_sampler_chain_add(smpl, llama_sampler_init_top_k(40));
    llama_sampler_chain_add(
        smpl, llama_sampler_init_penalties(llama_vocab_n_tokens(vocab), 64, 1.05f, 0.0f, 0.0f));
    llama_sampler_chain_add(smpl, llama_sampler_init_greedy());

    std::string text;
    int rc = 0;
    llama_batch batch = llama_batch_get_one(tokens.data(), static_cast<int32_t>(tokens.size()));
    llama_token next = 0;
    for (int i = 0; i < max_tokens; ++i) {
        if (cancel && *cancel) {
            rc = VOXKIT_MT_ERR_CANCELLED;
            break;
        }
        int d = llama_decode(h->ctx, batch);
        if (d != 0) {
            rc = (cancel && *cancel) ? VOXKIT_MT_ERR_CANCELLED : VOXKIT_MT_ERR_FAILED;
            if (rc == VOXKIT_MT_ERR_FAILED) {
                set_err(err, err_cap, "decode failed (" + std::to_string(d) + ")");
            }
            break;
        }
        next = llama_sampler_sample(smpl, h->ctx, -1);
        if (llama_vocab_is_eog(vocab, next)) {
            break;
        }
        char piece[256];
        int k = llama_token_to_piece(vocab, next, piece, sizeof piece, 0, false);
        if (k > 0) {
            text.append(piece, static_cast<size_t>(k));
        }
        batch = llama_batch_get_one(&next, 1);
    }
    llama_sampler_free(smpl);
    llama_set_abort_callback(h->ctx, nullptr, nullptr);
    if (rc != 0) {
        return rc;
    }
    if (text.size() + 1 > out_cap) {
        set_err(err, err_cap, "output buffer too small");
        return VOXKIT_MT_ERR_TOO_LONG;
    }
    std::memcpy(out, text.data(), text.size());
    out[text.size()] = '\0';
    return static_cast<int>(text.size());
}

VK_EXPORT void vk_mt_unload(void *handle) {
    auto *h = static_cast<Handle *>(handle);
    if (!h) {
        return;
    }
    if (h->ctx) {
        llama_free(h->ctx);
    }
    if (h->model) {
        llama_model_free(h->model);
    }
    delete h;
}
