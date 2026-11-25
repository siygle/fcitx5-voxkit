/*
 * SPDX-FileCopyrightText: 2026 siygle
 * SPDX-License-Identifier: MIT
 *
 * C ABI of the optional translation plugin (libvoxkit-mt.so / .dylib).
 *
 * The voxkit daemon dlopen()s the plugin (RTLD_LOCAL) only when a translate
 * request arrives and dlclose()s it after the idle timeout. The plugin
 * carries llama.cpp and its OWN statically linked ggml with hidden
 * visibility, so it never clashes with the ggml inside transcribe-cpp that
 * the daemon links. Only the vk_mt_* functions below are exported (see
 * voxkit_mt.map / voxkit_mt.exp).
 *
 * All functions are synchronous. One model handle must not be used by two
 * threads at once (the daemon serialises with a mutex).
 */
#ifndef VOXKIT_MT_H_
#define VOXKIT_MT_H_

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define VOXKIT_MT_ABI 1

/* Result codes of vk_mt_generate. */
#define VOXKIT_MT_ERR_FAILED (-1)
#define VOXKIT_MT_ERR_CANCELLED (-2)
#define VOXKIT_MT_ERR_TOO_LONG (-3)

/* VOXKIT_MT_ABI the plugin was built with. The daemon refuses others. */
int vk_mt_abi(void);

/* Human-readable build info, e.g. "llama.cpp b12722 ggml 1.0.0". */
const char *vk_mt_info(void);

/* Load a GGUF model. NULL on failure (reason in `err`). */
void *vk_mt_load(const char *path, int n_threads, char *err, size_t err_cap);

/* Run one chat turn: `user` is the user message; the model's own chat
 * template is applied. Greedy decoding with a light repetition penalty.
 * Writes NUL-terminated UTF-8 into `out` and returns its length in bytes,
 * or a negative VOXKIT_MT_ERR_*. `cancel` may be NULL; when it points to a
 * non-zero int the run stops early with VOXKIT_MT_ERR_CANCELLED. */
int vk_mt_generate(void *model, const char *user, int max_tokens,
                   const volatile int *cancel, char *out, size_t out_cap,
                   char *err, size_t err_cap);

/* Free the model and its context. NULL is ignored. */
void vk_mt_unload(void *model);

#ifdef __cplusplus
}
#endif

#endif /* VOXKIT_MT_H_ */
