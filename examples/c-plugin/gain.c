/*
 * A minimal nodeplayer plugin in C: a gain node with its own UI, and an extension that
 * logs track changes and adds a menu item.
 *
 *   cc -O2 -shared -fPIC -I../../sdk/include gain.c -o libnp_cgain.so -lm
 *   cp libnp_cgain.so ~/.local/share/nodeplayer/plugins/
 */
#include <math.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>

#include "nodeplayer_plugin.h"

/* ---- Gain node ---- */

typedef struct {
    _Atomic double gain_db; /* written on the audio thread, read by the UI */
    float current;          /* smoothed linear gain, audio thread only */
} Gain;

static const NpParamInfo gain_params[] = {
    {.id = 0, .name = "Gain", .unit = "dB", .min = -24.0, .max = 24.0, .def = 0.0},
};

static void *gain_create(float sr, uint32_t max_block) {
    (void)sr;
    (void)max_block;
    Gain *g = calloc(1, sizeof *g);
    g->current = 1.0f;
    return g;
}

static void gain_destroy(void *inst) { free(inst); }

static void gain_process(void *inst, const float *il, const float *ir, float *ol, float *or_, uint32_t n) {
    Gain *g = inst;
    float target = powf(10.0f, (float)atomic_load(&g->gain_db) / 20.0f);
    for (uint32_t k = 0; k < n; k++) {
        g->current += (target - g->current) * 0.002f;
        ol[k] = il[k] * g->current;
        or_[k] = ir[k] * g->current;
    }
}

static void gain_set_param(void *inst, uint32_t id, double v) {
    if (id == 0) atomic_store(&((Gain *)inst)->gain_db, v);
}

static double gain_get_param(void *inst, uint32_t id) {
    return id == 0 ? atomic_load(&((Gain *)inst)->gain_db) : 0.0;
}

static void gain_draw(void *inst, const NpUi *ui) {
    Gain *g = inst;
    double db = atomic_load(&g->gain_db);
    ui->vt->label(ui, NP_STR("Written in C"), NP_TEXT_WEAK);
    /* The UI may set values directly here since the parameter is atomic. */
    if (ui->vt->slider(ui, NP_STR("gain"), &db, -24.0, 24.0, 0)) atomic_store(&g->gain_db, db);
    if (ui->vt->button(ui, NP_STR("0 dB"), true)) atomic_store(&g->gain_db, 0.0);
}

static const NpNodeDesc nodes[] = {{
    .id = "c_gain",
    .name = "C Gain",
    .category = "Fx|Utility",
    .params = gain_params,
    .param_count = 1,
    .create = gain_create,
    .destroy = gain_destroy,
    .process = gain_process,
    .set_param = gain_set_param,
    .get_param = gain_get_param,
    .draw = gain_draw,
}};

/* ---- Extension ---- */

typedef struct {
    const NpHost *host;
    uint32_t menu;
} Ext;

static void collect(void *ctx, const uint8_t *data, size_t len) {
    char *buf = ctx;
    size_t used = 0;
    while (buf[used]) used++;
    for (size_t i = 0; i < len && used < 511; i++) buf[used++] = (char)data[i];
    buf[used] = 0;
}

static void *ext_create(const NpHost *host) {
    Ext *e = calloc(1, sizeof *e);
    e->host = host;
    e->menu = host->add_menu_item(host, NP_STR("Say hello (C)"));
    return e;
}

static void ext_destroy(void *ext) { free(ext); }

static void ext_event(void *ext, const NpEvent *ev) {
    Ext *e = ext;
    if (ev->kind != NP_EVENT_TRACK_CHANGED) return;
    char title[512] = {0};
    NpBuf out = {title, collect};
    e->host->track_text(e->host, NP_TRACK_TITLE, &out);
    char msg[600];
    int n = snprintf(msg, sizeof msg, "now playing: %s", title);
    e->host->log(e->host, NP_LOG_INFO, (NpStr){msg, (size_t)n});
}

static void ext_menu(void *ext, uint32_t id) {
    Ext *e = ext;
    if (id == e->menu) e->host->log(e->host, NP_LOG_WARN, NP_STR("hello from C"));
}

static const NpExtension extension = {
    .create = ext_create,
    .destroy = ext_destroy,
    .on_event = ext_event,
    .on_menu = ext_menu,
};

static const NpPluginDesc desc = {
    .abi_version = NP_ABI_VERSION,
    .name = "C example",
    .version = "0.1.0",
    .author = "nodeplayer",
    .nodes = nodes,
    .node_count = 1,
    .extension = &extension,
};

const NpPluginDesc *nodeplayer_plugin_entry(void) { return &desc; }
