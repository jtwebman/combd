// The QuickJS ceiling: the same app.js and requests, with QuickJS compiled
// natively (gcc -O2, computed gotos), no Wasm. Built and run by Dockerfile.native.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "quickjs.h"

static const char app_js[] = {
#embed "app.js"
    , 0};
static int64_t n;

static JSValue js_counter(JSContext *c, JSValueConst t, int argc, JSValueConst *argv) { return JS_NewInt64(c, ++n); }
static JSValue js_log(JSContext *c, JSValueConst t, int argc, JSValueConst *argv) { return JS_UNDEFINED; }

static double now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e6 + ts.tv_nsec / 1e3;
}

static JSContext *boot(JSValue *handler) {
    JSRuntime *rt = JS_NewRuntime();
    JSContext *ctx = JS_NewContext(rt);
    JSValue g = JS_GetGlobalObject(ctx);
    JSValue combd = JS_NewObject(ctx);
    JS_SetPropertyStr(ctx, combd, "counter", JS_NewCFunction(ctx, js_counter, "counter", 0));
    JS_SetPropertyStr(ctx, g, "combd", combd);
    JSValue console = JS_NewObject(ctx);
    JS_SetPropertyStr(ctx, console, "log", JS_NewCFunction(ctx, js_log, "log", 1));
    JS_SetPropertyStr(ctx, g, "console", console);
    JS_FreeValue(ctx, JS_Eval(ctx, app_js, sizeof app_js - 1, "app.js", JS_EVAL_TYPE_GLOBAL));
    *handler = JS_GetPropertyStr(ctx, g, "handle");
    JS_FreeValue(ctx, g);
    return ctx;
}

static size_t call(JSContext *ctx, JSValue h, const char *req) {
    JSValue arg = JS_NewString(ctx, req);
    JSValue r = JS_Call(ctx, h, JS_UNDEFINED, 1, &arg);
    size_t len;
    const char *s = JS_ToCStringLen(ctx, &len, r);
    JS_FreeCString(ctx, s);
    JS_FreeValue(ctx, r);
    JS_FreeValue(ctx, arg);
    return len;
}

int main(void) {
    char reqs[8][2048];
    int nreq = 0;
    FILE *f = fopen("requests.txt", "r");
    while (nreq < 8 && fgets(reqs[nreq], sizeof reqs[nreq], f)) {
        reqs[nreq][strcspn(reqs[nreq], "\n")] = 0;
        nreq++;
    }
    double t = now_us();
    for (int i = 0; i < 200; i++) {
        JSValue h;
        JSContext *ctx = boot(&h);
        call(ctx, h, reqs[0]);
        JSRuntime *rt = JS_GetRuntime(ctx);
        JS_FreeValue(ctx, h);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
    }
    printf("js       qjs-native cold (new runtime + app + first request) %.1f us\n", (now_us() - t) / 200);
    JSValue h;
    JSContext *ctx = boot(&h);
    for (int r = 0; r < nreq; r++) {
        int iters = strncmp(reqs[r], "fib", 3) == 0 ? 20 : 5000;
        for (int i = 0; i < 200; i++) call(ctx, h, reqs[r]);
        t = now_us();
        size_t len = 0;
        for (int i = 0; i < iters; i++) len = call(ctx, h, reqs[r]);
        printf("js       qjs-native %-22.22s %9.1f us   -> %zu bytes\n", reqs[r], (now_us() - t) / iters, len);
    }
    return 0;
}
