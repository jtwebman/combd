// The combd test cell in JavaScript: QuickJS (quickjs-ng) plus this wrapper.
// QuickJS boots and runs app.js once, during _initialize. Snapshot the cell
// after that, and every copy starts warm.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "quickjs.h"

__attribute__((import_module("combd"), import_name("counter_incr"))) int64_t counter_incr(void);

// QuickJS wants the source NUL-terminated.
static const char app_js[] = {
#embed "app.js"
    , 0};

static JSContext *ctx;
static JSValue handler;

static JSValue js_counter(JSContext *c, JSValueConst this_val, int argc, JSValueConst *argv) {
    return JS_NewInt64(c, counter_incr());
}

static JSValue js_log(JSContext *c, JSValueConst this_val, int argc, JSValueConst *argv) {
    for (int i = 0; i < argc; i++) {
        const char *s = JS_ToCString(c, argv[i]);
        if (s) {
            fputs(s, stdout);
            JS_FreeCString(c, s);
        }
        fputc(i + 1 < argc ? ' ' : '\n', stdout);
    }
    fflush(stdout);
    return JS_UNDEFINED;
}

__attribute__((constructor)) static void boot(void) {
    JSRuntime *rt = JS_NewRuntime();
    JS_SetMaxStackSize(rt, 512 * 1024);
    ctx = JS_NewContext(rt);
    JSValue global = JS_GetGlobalObject(ctx);
    JSValue combd = JS_NewObject(ctx);
    JS_SetPropertyStr(ctx, combd, "counter", JS_NewCFunction(ctx, js_counter, "counter", 0));
    JS_SetPropertyStr(ctx, global, "combd", combd);
    JSValue console = JS_NewObject(ctx);
    JS_SetPropertyStr(ctx, console, "log", JS_NewCFunction(ctx, js_log, "log", 1));
    JS_SetPropertyStr(ctx, global, "console", console);
    JSValue r = JS_Eval(ctx, app_js, sizeof app_js - 1, "app.js", JS_EVAL_TYPE_GLOBAL);
    if (JS_IsException(r)) __builtin_trap();
    JS_FreeValue(ctx, r);
    handler = JS_GetPropertyStr(ctx, global, "handle");
    JS_FreeValue(ctx, global);
}

__attribute__((export_name("alloc"))) void *cell_alloc(uint32_t len) { return malloc(len ? len : 1); }

// Returns the response as (ptr << 32) | len. An uncaught JS error crashes the cell.
__attribute__((export_name("handle"))) uint64_t handle(char *req, uint32_t len) {
    JSValue arg = JS_NewStringLen(ctx, req, len);
    free(req);
    JSValue r = JS_Call(ctx, handler, JS_UNDEFINED, 1, &arg);
    JS_FreeValue(ctx, arg);
    if (JS_IsException(r)) __builtin_trap();
    size_t n;
    const char *s = JS_ToCStringLen(ctx, &n, r);
    char *out = malloc(n ? n : 1);
    memcpy(out, s, n);
    JS_FreeCString(ctx, s);
    JS_FreeValue(ctx, r);
    return ((uint64_t)(uintptr_t)out << 32) | (uint32_t)n;
}
